//! Working out which config shape a file is written in, and converting the
//! older two into the current one.
//!
//! Three shapes have existed, and their top-level keys are completely
//! disjoint:
//!
//! ```text
//! v3            version  server  client
//! legacy agent  agent    exports
//! legacy mount  exclude  pin  auto_cache_max  auto_cache_budget
//!               data_dir  no_server_defaults  detect_conflicts  token
//! ```
//!
//! Nothing overlaps, so a file identifies itself by its own contents and
//! `version:` is a forward-compatibility lever rather than the mechanism.
//! That is worth stating because it is the property that makes upgrading
//! painless: an existing config keeps working with nothing added to it.

use std::collections::BTreeMap;

use super::legacy::MountConfig;
use super::schema::{ClientSection, Config, ServerSection, CURRENT_VERSION};

/// Which of the three shapes a file is written in.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Shape {
    /// `version`/`server`/`client`, or an empty document.
    V3,
    /// `agent`/`exports` at the top level.
    LegacyAgent,
    /// The flat mount config, which was only ever loaded via `--config`.
    LegacyMount,
}

const V3_KEYS: &[&str] = &["version", "server", "client"];
const AGENT_KEYS: &[&str] = &["agent", "exports"];
const MOUNT_KEYS: &[&str] = &[
    "exclude",
    "pin",
    "auto_cache_max",
    "auto_cache_budget",
    "data_dir",
    "no_server_defaults",
    "detect_conflicts",
    "token",
];

fn top_level_keys(value: &serde_yaml::Value) -> Vec<String> {
    match value {
        serde_yaml::Value::Mapping(map) => map
            .keys()
            .filter_map(|k| k.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn matching<'a>(keys: &'a [String], family: &[&str]) -> Vec<&'a str> {
    keys.iter()
        .filter(|k| family.contains(&k.as_str()))
        .map(String::as_str)
        .collect()
}

/// Identify a parsed document.
///
/// An empty file, or one holding only `version:`, is v3 defining nothing —
/// an empty config has always meant "no exports" and has to keep meaning it.
pub fn shape_of(value: &serde_yaml::Value) -> anyhow::Result<Shape> {
    let keys = top_level_keys(value);
    let v3 = matching(&keys, V3_KEYS);
    let agent = matching(&keys, AGENT_KEYS);
    let mount = matching(&keys, MOUNT_KEYS);

    // A file half-migrated by hand is the case most likely to produce a
    // silently wrong mount, so name what pointed where instead of picking.
    let families = [("v3", &v3), ("legacy agent", &agent), ("legacy mount", &mount)];
    let present: Vec<_> = families.iter().filter(|(_, k)| !k.is_empty()).collect();
    if present.len() > 1 {
        let described: Vec<String> = present
            .iter()
            .map(|(name, keys)| format!("{name} ({})", keys.join(", ")))
            .collect();
        anyhow::bail!(
            "this config mixes layouts: {}. Pick one — a file written wholly in an \
             older layout is upgraded automatically, but a half-converted one cannot \
             be, because guessing which half is current could serve the wrong folders.",
            described.join(" and ")
        );
    }

    if !agent.is_empty() {
        return Ok(Shape::LegacyAgent);
    }
    if !mount.is_empty() {
        return Ok(Shape::LegacyMount);
    }
    Ok(Shape::V3)
}

/// Parse a document of any known shape into the current one.
pub fn from_value(value: serde_yaml::Value) -> anyhow::Result<(Config, Shape)> {
    let shape = shape_of(&value)?;
    let config = match shape {
        Shape::V3 => {
            let config: Config = serde_yaml::from_value(value)?;
            // Trust an explicit version over the shape. A file written for a
            // future version must fail loudly rather than be half-read: its
            // keys would parse as unknown and the operator would be told
            // about a typo that is not one.
            if let Some(v) = config.version {
                anyhow::ensure!(
                    v == CURRENT_VERSION,
                    "config version {v} is not supported by this build (it understands \
                     version {CURRENT_VERSION}, and files with no version key). Upgrade \
                     alloyfs, or remove the version line if the file is not really v{v}."
                );
            }
            config
        }
        Shape::LegacyAgent => {
            let agent: alloyfs_agent::AgentConfig = serde_yaml::from_value(value)?;
            from_legacy_agent(agent)
        }
        Shape::LegacyMount => {
            let mount: MountConfig = serde_yaml::from_value(value)?;
            from_legacy_mount(mount)
        }
    };
    Ok((config, shape))
}

/// `agent:` + `exports:` becomes `server:`.
pub fn from_legacy_agent(old: alloyfs_agent::AgentConfig) -> Config {
    let exports = if old.exports.is_empty() {
        None
    } else {
        Some(old.exports.into_iter().collect::<BTreeMap<_, _>>())
    };
    Config {
        version: None,
        server: Some(ServerSection {
            tcp_listen: old.agent.tcp_listen,
            tcp_token: old.agent.tcp_token,
            http_listen: old.agent.http_listen,
            http_token: old.agent.http_token,
            exports,
        }),
        client: None,
    }
}

/// The flat mount config becomes `client:` defaults with no named mounts —
/// it never carried a url or a mountpoint, since both were always arguments.
pub fn from_legacy_mount(old: MountConfig) -> Config {
    Config {
        version: None,
        server: None,
        client: Some(ClientSection {
            exclude: (!old.exclude.is_empty()).then_some(old.exclude),
            pin: (!old.pin.is_empty()).then_some(old.pin),
            auto_cache_max: old.auto_cache_max,
            auto_cache_budget: old.auto_cache_budget,
            data_dir: old.data_dir,
            no_server_defaults: old.no_server_defaults.then_some(true),
            detect_conflicts: old.detect_conflicts.then_some(true),
            token: old.token,
            mounts: None,
        }),
    }
}

/// The `alloyfs_agent::AgentConfig` the agent still takes, built from whatever
/// shape the file was in. Nothing in `alloyfs-agent` had to change for v3.
pub fn to_agent_config(config: &Config) -> alloyfs_agent::AgentConfig {
    let mut out = alloyfs_agent::AgentConfig::default();
    let Some(server) = &config.server else {
        return out;
    };
    out.agent.tcp_listen = server.tcp_listen.clone();
    out.agent.tcp_token = server.tcp_token.clone();
    out.agent.http_listen = server.http_listen.clone();
    out.agent.http_token = server.http_token.clone();
    if let Some(exports) = &server.exports {
        for (name, export) in exports {
            // A clone, not a field-by-field rebuild. Both sides are the same
            // type, so listing the fields bought nothing and cost the export's
            // `client:` block: it was rebuilt as `client: None`, which silently
            // discarded every suggestion a server made to its mounts. serde had
            // parsed the block correctly one step earlier, so nothing failed and
            // nothing warned — the settings simply never arrived, and even a
            // deliberately invalid `auto_cache_max` (which is resolved at boot
            // precisely so a bad value cannot reach a mount) started cleanly.
            //
            // The general shape of the bug matters more than this instance:
            // an exhaustive struct literal over a type you do not own turns
            // every field added upstream into a silent default here. A clone
            // cannot drift.
            out.exports.insert(name.clone(), export.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(text: &str) -> anyhow::Result<Shape> {
        shape_of(&serde_yaml::from_str(text).unwrap())
    }

    #[test]
    fn each_shape_identifies_itself() {
        assert_eq!(shape("version: 3\nserver: {}\n").unwrap(), Shape::V3);
        assert_eq!(shape("agent:\n  tcp_listen: x\n").unwrap(), Shape::LegacyAgent);
        assert_eq!(
            shape("exports:\n  p: { path: /x }\n").unwrap(),
            Shape::LegacyAgent
        );
        assert_eq!(shape("exclude: [node_modules]\n").unwrap(), Shape::LegacyMount);
        assert_eq!(shape("token: secret\n").unwrap(), Shape::LegacyMount);
    }

    /// The property that makes upgrading free: a v3 file needs no version key,
    /// because `server:`/`client:` already say what it is.
    #[test]
    fn v3_is_recognised_without_a_version_key() {
        assert_eq!(shape("client:\n  mounts: {}\n").unwrap(), Shape::V3);
        assert_eq!(shape("server:\n  exports: {}\n").unwrap(), Shape::V3);
    }

    #[test]
    fn an_empty_file_is_a_config_that_defines_nothing() {
        assert_eq!(shape("{}\n").unwrap(), Shape::V3);
        assert_eq!(shape("version: 3\n").unwrap(), Shape::V3);
    }

    /// Half-migrated by hand is the dangerous case: guessing could serve the
    /// wrong folders, so it names both sides instead.
    #[test]
    fn mixing_layouts_is_refused_and_says_which_keys_clashed() {
        let err = shape("agent:\n  tcp_listen: x\nclient:\n  mounts: {}\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mixes layouts"), "{msg}");
        assert!(msg.contains("agent"), "{msg}");
        assert!(msg.contains("client"), "{msg}");
        assert!(msg.contains("Pick one"), "should say how to fix it: {msg}");
    }

    #[test]
    fn a_future_version_fails_loudly() {
        let value = serde_yaml::from_str("version: 99\nserver: {}\n").unwrap();
        let err = from_value(value).unwrap_err().to_string();
        assert!(err.contains("99"), "{err}");
        assert!(
            err.contains("version 3"),
            "should name what it does support: {err}"
        );
    }

    /// A legacy agent config has to produce exactly the AgentConfig it always
    /// did — this is the assertion that lets old files keep working untouched.
    #[test]
    fn a_legacy_agent_config_survives_the_conversion() {
        let text = "agent:\n  tcp_listen: \"0.0.0.0:7440\"\n  tcp_token: sec\n\
                    exports:\n  projects:\n    path: /srv/projects\n    read_only: true\n\
                    \x20   exclude: ['**/.git']\n";
        let direct: alloyfs_agent::AgentConfig = serde_yaml::from_str(text).unwrap();
        let (config, shape) = from_value(serde_yaml::from_str(text).unwrap()).unwrap();
        assert_eq!(shape, Shape::LegacyAgent);

        let round_tripped = to_agent_config(&config);
        assert_eq!(round_tripped.agent.tcp_listen, direct.agent.tcp_listen);
        assert_eq!(round_tripped.agent.tcp_token, direct.agent.tcp_token);
        assert_eq!(round_tripped.exports.len(), direct.exports.len());
        let a = round_tripped.exports.get("projects").unwrap();
        let b = direct.exports.get("projects").unwrap();
        assert_eq!(a.path, b.path);
        assert_eq!(a.read_only, b.read_only);
        assert_eq!(a.exclude, b.exclude);
        assert_eq!(a.default_excludes, b.default_excludes);
    }

    /// The suggestions an export makes to its mounts have to survive the trip
    /// from `server.exports` into the agent config. They did not: the
    /// conversion rebuilt each export field by field and wrote `client: None`,
    /// so a server could describe defaults that never left the file.
    ///
    /// Nothing detected it, and the reason is worth keeping in view — the
    /// round-trip test above asserts the same four fields the broken code
    /// copied, so it agreed with the bug. This one asserts the fifth.
    #[test]
    fn an_export_keeps_the_client_settings_it_suggests() {
        let text = "version: 3\n\
                    server:\n  \
                      exports:\n    \
                        projects:\n      \
                          path: /srv/projects\n      \
                          client:\n        \
                            exclude: [node_modules]\n        \
                            pin: [\"*.lock\"]\n        \
                            auto_cache_max: 2M\n";
        let (config, shape) = from_value(serde_yaml::from_str(text).unwrap()).unwrap();
        assert_eq!(shape, Shape::V3);

        let agent = to_agent_config(&config);
        let suggested = agent
            .exports
            .get("projects")
            .expect("the export survived")
            .client
            .as_ref()
            .expect("its client suggestions survived too");
        assert_eq!(suggested.exclude, ["node_modules"]);
        assert_eq!(suggested.pin, ["*.lock"]);
        assert!(suggested.auto_cache_max.is_some());
    }

    #[test]
    fn a_legacy_mount_config_becomes_client_defaults() {
        let text = "exclude: [node_modules]\nauto_cache_max: 2M\ndetect_conflicts: true\n";
        let (config, shape) = from_value(serde_yaml::from_str(text).unwrap()).unwrap();
        assert_eq!(shape, Shape::LegacyMount);
        let client = config.client.unwrap();
        assert_eq!(client.exclude.unwrap(), ["node_modules"]);
        assert_eq!(client.detect_conflicts, Some(true));
        // It never had named mounts: url and mountpoint were always arguments.
        assert!(client.mounts.is_none());
    }

    /// An unset boolean stays unset rather than becoming an explicit `false`,
    /// so it cannot override a value set somewhere else later.
    #[test]
    fn legacy_false_flags_do_not_become_explicit() {
        let (config, _) = from_value(serde_yaml::from_str("exclude: [x]\n").unwrap()).unwrap();
        let client = config.client.unwrap();
        assert_eq!(client.detect_conflicts, None);
        assert_eq!(client.no_server_defaults, None);
    }

    /// A v3 file with no server section produces an agent config with nothing
    /// in it, rather than failing — `serve` then reports having nothing to do.
    #[test]
    fn a_client_only_config_yields_an_empty_agent_config() {
        let (config, _) = from_value(serde_yaml::from_str("client:\n  mounts: {}\n").unwrap()).unwrap();
        let agent = to_agent_config(&config);
        assert!(agent.exports.is_empty());
        assert!(agent.agent.tcp_listen.is_none());
    }

    #[test]
    fn a_null_server_section_converts_without_panicking() {
        let (config, _) = from_value(serde_yaml::from_str("version: 3\nserver:\n").unwrap()).unwrap();
        assert!(to_agent_config(&config).exports.is_empty());
    }
}
