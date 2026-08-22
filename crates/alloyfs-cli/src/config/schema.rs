//! The v3 config: one file describing both halves of a machine.
//!
//! ```yaml
//! version: 3
//! server:
//!   tcp_listen: "0.0.0.0:7440"
//!   exports:
//!     projects: { path: /srv/projects }
//! client:
//!   exclude: [node_modules]
//!   mounts:
//!     work: { url: ssh://azure/projects, at: "P:" }
//! ```
//!
//! **Nothing in here is required.** Not `version`, not either section, not
//! anything inside them. A machine that only mounts writes no `server:`; a
//! machine whose exports are temporarily commented out leaves `server:` with
//! nothing under it, which YAML reads as `null`. Both load, because commenting
//! a block out is how people disable things and a parser that punishes it is a
//! parser people fight.
//!
//! That is why every optional section is `#[serde(default)] Option<T>`:
//! `default` covers the absent key, `Option` covers the explicit null, and
//! either alone fails the other case.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The config schema this binary writes and understands.
pub const CURRENT_VERSION: u32 = 3;

/// Defaults when the config says nothing, or says only `cache: true`.
pub const CACHE_AUTO_SIZE_DEFAULT: u64 = 2 * 1024 * 1024;
pub const CACHE_AUTO_MAX_DEFAULT: u64 = 512 * 1024 * 1024;
pub const CACHE_WARM_MAX_DEFAULT: u64 = 4 * 1024 * 1024 * 1024;

/// `cache:` accepts a bool or a table, because most configs want to say one
/// of two things and neither deserves three keys:
///
/// ```yml
/// cache: false          # off entirely
/// cache: true           # the defaults below — same as saying nothing
/// cache:                # or set the ones you care about
///   warm-max: 16G
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum CacheConfig {
    /// `true` = defaults, `false` = no local cache at all.
    Enabled(bool),
    Detailed(CacheSection),
}

impl CacheConfig {
    /// (auto-size, auto-max, warm-max) in bytes, defaults filled in.
    ///
    /// An absent `cache:` is the same as `cache: true`, so the caller passes
    /// `None` through here rather than branching.
    pub fn resolve(this: Option<&CacheConfig>) -> anyhow::Result<(u64, u64, u64)> {
        let d = (
            CACHE_AUTO_SIZE_DEFAULT,
            CACHE_AUTO_MAX_DEFAULT,
            CACHE_WARM_MAX_DEFAULT,
        );
        let bytes = |f: &Option<alloyfs_common::SizeField>, fallback: u64| -> anyhow::Result<u64> {
            match f {
                Some(s) => s.to_bytes().map_err(|e| anyhow::anyhow!("cache: {e}")),
                None => Ok(fallback),
            }
        };
        match this {
            None | Some(CacheConfig::Enabled(true)) => Ok(d),
            // Off means the walker takes nothing and a read warms nothing;
            // zero per-file size is what the client already reads as OFF.
            Some(CacheConfig::Enabled(false)) => Ok((0, d.1, 0)),
            Some(CacheConfig::Detailed(s)) => Ok((
                bytes(&s.auto_size, d.0)?,
                bytes(&s.auto_max, d.1)?,
                bytes(&s.warm_max, d.2)?,
            )),
        }
    }
}

/// `cache:` — what the local cache pulls down and how much it keeps.
///
/// Three numbers because they answer three different questions, and the old
/// flat `auto_cache_max` / `auto_cache_budget` conflated the first two:
///
/// - `auto-size` is PER FILE. The walker downloads anything up to it ahead of
///   use and nothing larger. It bounds a guess about what will be wanted.
/// - `auto-max` is the total pool those guesses may occupy.
/// - `warm-max` is the total pool for files a READ pulled down. A read is
///   demand rather than speculation, so it is not bound by `auto-size` at
///   all — a 4 GB file someone opened is cached, a 4 GB file nobody touched
///   is not. Separate pools so a large read cannot evict the prefetched
///   working set, and a prefetch cannot evict what is being read.
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct CacheSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_size: Option<alloyfs_common::SizeField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_max: Option<alloyfs_common::SizeField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_max: Option<alloyfs_common::SizeField>,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Absent means "whatever shape this file is in", which the loader has
    /// already worked out from its keys. Present, it is authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientSection>,
}

/// What this machine offers to others.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_listen: Option<String>,
    /// Shared secret required from every TCP mount. Mandatory when
    /// `tcp_listen` is not loopback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_listen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_token: Option<String>,
    /// Opt this server's outgoing large frames into zstd on v13+ sessions
    /// (mirrors the agent-level key; the client direction opts in per
    /// mount). This key existed on AgentConfig before it existed HERE, and
    /// deny_unknown_fields turned that gap into a crash-looping mount the
    /// moment a config used it — the schema test now covers the mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zstd: Option<bool>,
    /// Absent, null and `{}` all mean "nothing to serve".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exports: Option<BTreeMap<String, alloyfs_agent::ExportConfig>>,
}

/// What this machine consumes from others: defaults, then the mounts they
/// apply to.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_cache_max: Option<alloyfs_common::SizeField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_cache_budget: Option<alloyfs_common::SizeField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_server_defaults: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_conflicts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zstd: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Absent, null and `{}` all mean "no mounts".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mounts: Option<BTreeMap<String, MountEntry>>,
}

/// One named mount. Every field except `url` and `at` overrides the same key
/// on the enclosing [`ClientSection`].
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MountEntry {
    pub url: String,
    /// Where it appears: a drive letter (`P:`) on Windows, a directory on
    /// Linux. Named `at` rather than `mountpoint` because it reads as a
    /// sentence next to `url` and this file is meant to be read.
    pub at: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_cache_max: Option<alloyfs_common::SizeField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_cache_budget: Option<alloyfs_common::SizeField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_server_defaults: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_conflicts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zstd: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// One mount's settings after the config layers are collapsed, but before CLI
/// flags are applied on top.
///
/// Lists **replace** rather than union. Union reads as the friendly choice
/// until somebody needs one mount to NOT inherit a pattern, at which point
/// there is no way to say it — and `exclude: []` has to be able to mean
/// nothing. Scalars override the same way, which is the rule nobody is
/// surprised by.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedMount {
    pub url: String,
    pub at: PathBuf,
    pub exclude: Vec<String>,
    pub pin: Vec<String>,
    pub auto_cache_max: Option<alloyfs_common::SizeField>,
    pub auto_cache_budget: Option<alloyfs_common::SizeField>,
    pub cache: Option<CacheConfig>,
    pub data_dir: Option<PathBuf>,
    pub no_server_defaults: bool,
    pub detect_conflicts: bool,
    pub zstd: bool,
    pub token: Option<String>,
}

impl ClientSection {
    /// Collapse `client:` defaults and one named mount into what that mount
    /// actually wants. CLI flags are applied by the caller, above this.
    pub fn resolve(&self, entry: &MountEntry) -> ResolvedMount {
        ResolvedMount {
            url: entry.url.clone(),
            at: entry.at.clone(),
            exclude: entry
                .exclude
                .clone()
                .or_else(|| self.exclude.clone())
                .unwrap_or_default(),
            pin: entry.pin.clone().or_else(|| self.pin.clone()).unwrap_or_default(),
            auto_cache_max: entry
                .auto_cache_max
                .clone()
                .or_else(|| self.auto_cache_max.clone()),
            auto_cache_budget: entry
                .auto_cache_budget
                .clone()
                .or_else(|| self.auto_cache_budget.clone()),
            // The mount's own `cache:` block wins whole, then the client
            // default's. Merging them key-by-key would let a mount that
            // states only `warm-max` silently inherit a client-level
            // `auto-size` it never mentioned — the same trap `exclude`
            // already documents for lists.
            cache: entry.cache.clone().or_else(|| self.cache.clone()),
            data_dir: entry.data_dir.clone().or_else(|| self.data_dir.clone()),
            no_server_defaults: entry
                .no_server_defaults
                .or(self.no_server_defaults)
                .unwrap_or(false),
            detect_conflicts: entry.detect_conflicts.or(self.detect_conflicts).unwrap_or(false),
            zstd: entry.zstd.or(self.zstd).unwrap_or(false),
            token: entry.token.clone().or_else(|| self.token.clone()),
        }
    }

    /// Every named mount, resolved, in name order.
    pub fn resolved_mounts(&self) -> Vec<(String, ResolvedMount)> {
        self.mounts
            .as_ref()
            .map(|m| {
                m.iter()
                    .map(|(name, e)| (name.clone(), self.resolve(e)))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Config {
    /// Named mount, or `None`. The caller decides whether a miss is an error.
    pub fn mount(&self, name: &str) -> Option<ResolvedMount> {
        let client = self.client.as_ref()?;
        let entry = client.mounts.as_ref()?.get(name)?;
        Some(client.resolve(entry))
    }

    /// Names of every configured mount, for error messages that list what IS
    /// available instead of only what is not.
    pub fn mount_names(&self) -> Vec<String> {
        self.client
            .as_ref()
            .and_then(|c| c.mounts.as_ref())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn has_exports(&self) -> bool {
        self.server
            .as_ref()
            .and_then(|s| s.exports.as_ref())
            .is_some_and(|e| !e.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        serde_yaml::from_str(text).expect("should load")
    }

    /// Commenting a section's contents out leaves the key with a null value.
    /// That is the normal way people disable things and it must not be an
    /// error — `#[serde(default)]` alone would not cover it.
    #[test]
    fn a_null_section_is_not_an_error() {
        let cfg = parse("version: 3\nserver:\nclient:\n");
        assert!(cfg.server.is_none());
        assert!(cfg.client.is_none());
        assert!(!cfg.has_exports());
        assert!(cfg.mount_names().is_empty());
    }

    #[test]
    fn sections_can_be_omitted_entirely() {
        // Only a version.
        let cfg = parse("version: 3\n");
        assert!(cfg.server.is_none() && cfg.client.is_none());

        // Mount-only machine: no version, no server.
        let cfg = parse("client:\n  mounts:\n    work: { url: 'tcp://h/x', at: 'P:' }\n");
        assert_eq!(cfg.mount_names(), ["work"]);
        assert!(!cfg.has_exports());

        // Serve-only machine.
        let cfg = parse("server:\n  exports:\n    p: { path: /srv/p }\n");
        assert!(cfg.has_exports());
        assert!(cfg.mount_names().is_empty());
    }

    /// Empty maps mean the same as absent, one level down too.
    #[test]
    fn empty_collections_mean_nothing_configured() {
        let cfg = parse("server:\n  exports: {}\nclient:\n  mounts: {}\n");
        assert!(!cfg.has_exports());
        assert!(cfg.mount_names().is_empty());

        let cfg = parse("server:\n  exports:\nclient:\n  mounts:\n");
        assert!(!cfg.has_exports());
        assert!(cfg.mount_names().is_empty());
    }

    /// The rule that cannot be inferred from behaviour later: a mount's list
    /// REPLACES the client default rather than appending to it.
    #[test]
    fn lists_replace_they_do_not_union() {
        let cfg = parse(
            "client:\n  exclude: [node_modules, .venv]\n  mounts:\n\
             \x20   work: { url: 'tcp://h/x', at: 'P:', exclude: [target] }\n",
        );
        assert_eq!(cfg.mount("work").unwrap().exclude, ["target"]);
    }

    /// ...which is what makes it possible to say "this one inherits nothing".
    #[test]
    fn an_explicit_empty_list_means_none() {
        let cfg = parse(
            "client:\n  exclude: [node_modules]\n  mounts:\n\
             \x20   media: { url: 'tcp://h/m', at: 'M:', exclude: [] }\n",
        );
        assert!(cfg.mount("media").unwrap().exclude.is_empty());
    }

    #[test]
    fn an_unset_key_inherits_the_client_default() {
        let cfg = parse(
            "client:\n  exclude: [node_modules]\n  detect_conflicts: true\n  mounts:\n\
             \x20   docs: { url: 'tcp://h/d', at: 'D:' }\n",
        );
        let m = cfg.mount("docs").unwrap();
        assert_eq!(m.exclude, ["node_modules"]);
        assert!(m.detect_conflicts);
    }

    #[test]
    fn a_mount_scalar_overrides_the_client_default() {
        let cfg = parse(
            "client:\n  detect_conflicts: true\n  mounts:\n\
             \x20   loose: { url: 'tcp://h/x', at: 'P:', detect_conflicts: false }\n",
        );
        assert!(!cfg.mount("loose").unwrap().detect_conflicts);
    }

    #[test]
    fn an_unknown_mount_is_none_and_the_known_ones_are_listable() {
        let cfg = parse("client:\n  mounts:\n    a: { url: 'tcp://h/a', at: 'A:' }\n");
        assert!(cfg.mount("nope").is_none());
        assert_eq!(cfg.mount_names(), ["a"]);
    }

    /// A typo in a key has to fail rather than being silently ignored — a
    /// mount that quietly does not exclude what you told it to is worse than
    /// one that refuses to start.
    #[test]
    fn unknown_keys_are_refused() {
        assert!(serde_yaml::from_str::<Config>("server:\n  tcp_lisen: x\n").is_err());
        assert!(serde_yaml::from_str::<Config>("clientt:\n  mounts: {}\n").is_err());
        assert!(serde_yaml::from_str::<Config>(
            "client:\n  mounts:\n    a: { url: u, at: 'A:', exclud: [x] }\n"
        )
        .is_err());
    }
}
