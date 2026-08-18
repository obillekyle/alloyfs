//! Finding and loading the config, and where this machine keeps its data.
//!
//! One file describes both halves of a machine — what it serves and what it
//! mounts — and three shapes of that file have existed. `detect` works out
//! which one is in front of it and converts; everything above this module sees
//! only [`schema::Config`].

mod detect;
mod legacy;
mod schema;

use std::path::{Path, PathBuf};

use alloyfs_agent::AgentConfig;

pub use detect::{to_agent_config, Shape};

pub use schema::{ClientSection, Config, CURRENT_VERSION};

// Size parsing lives in alloyfs-common (the agent's `client:` section uses the
// same forms); re-exported so callers keep one import path.
pub use alloyfs_common::parse_size;

/// Load a config file, upgrading it in place if it is an older shape.
///
/// There is no permanent legacy mode. An old file is recognised, converted,
/// and **rewritten as v{CURRENT_VERSION}** — so the old layouts are a one-time
/// reader rather than a second code path to keep working forever. A carried
/// compatibility mode is the thing that quietly becomes permanent, and then
/// every later change has to be made twice.
///
/// The original is kept beside it as `.bak` because the rewrite cannot
/// preserve comments: serde round-trips values, not the prose around them.
/// Losing somebody's annotated config without a copy would be unforgivable for
/// a convenience feature.
///
/// A file that cannot be rewritten — read-only, or a directory this process
/// cannot write — still loads. It converts in memory and says why it could not
/// be upgraded. Refusing to start because a *cosmetic* upgrade failed would be
/// the wrong trade entirely.
pub fn load(path: &Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
    let value: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("config {} is not valid YAML: {e}", path.display()))?;
    let (mut config, shape) =
        detect::from_value(value).map_err(|e| anyhow::anyhow!("config {}: {e}", path.display()))?;

    if shape != Shape::V3 {
        config.version = Some(CURRENT_VERSION);
        match upgrade_in_place(path, &config) {
            Ok(backup) => tracing::info!(
                path = %path.display(),
                backup = %backup.display(),
                "upgraded this config to version {CURRENT_VERSION}; the original is kept as .bak \
                 (comments do not survive the rewrite)"
            ),
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not upgrade this config to version {CURRENT_VERSION} on disk; \
                 running from the converted form in memory"
            ),
        }
    }
    Ok(config)
}

/// Write `config` over `path`, keeping the original as `<path>.bak`.
///
/// Staged through a temporary file and renamed, so an interrupted upgrade
/// leaves either the old config or the new one and never half of either.
fn upgrade_in_place(path: &Path, config: &Config) -> anyhow::Result<PathBuf> {
    let backup = path.with_extension(format!(
        "{}.bak",
        path.extension().and_then(|e| e.to_str()).unwrap_or("yml")
    ));
    std::fs::copy(path, &backup)?;

    let yaml = serde_yaml::to_string(config)?;
    let staged = path.with_extension("alloyfs-upgrade");
    std::fs::write(&staged, format!("{UPGRADE_HEADER}{yaml}"))?;
    std::fs::rename(&staged, path)?;
    Ok(backup)
}

/// Prepended to an auto-upgraded file, because a config that silently changed
/// shape under someone is alarming to find and the note costs four lines.
const UPGRADE_HEADER: &str = "\
# Upgraded automatically to the version 3 layout by alloyfs.
# The previous file is beside this one with a .bak extension — including any
# comments, which a rewrite cannot preserve.
";

// ## Where things live
//
// One home-directory tree on both platforms, so the layout is the same
// thing to explain, back up, and reason about everywhere:
//
// ```text
// ~/.alloyfs/                 (%USERPROFILE%\.alloyfs on Windows)
//   config.yml                created on first use if absent
//   data/<host>/              DURABLE — losing it loses real work
//     overlay/<export>/         files that exist on NO server
//     sync/<export>-<tag>.json  sync baselines
//   cache/<host>/             DISPOSABLE — delete it freely
//     <export>/                 downloaded blobs
//     <export>.manifest.json
// ```
//
// Splitting `data` from `cache` at the top is the point: everything under
// `cache` can be deleted at any time and only costs re-downloading,
// while everything under `data` is unrecoverable. A single mixed
// directory invites "just clear the whole thing" and silently destroys
// the overlay.

/// Pre-AlloyFS and pre-layout locations, searched only to migrate away from.
const LEGACY_ROOTS: &[&str] = &["alloyfs", "drive-sync"];

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(unix)]
    let var = "HOME";
    std::env::var(var)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// `~/.alloyfs`, created on demand.
pub fn app_dir() -> PathBuf {
    let dir = home_dir().join(".alloyfs");
    if !dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, path = %dir.display(), "could not create the AlloyFS directory");
        }
    }
    dir
}

/// Where the old flat layout lived, for one-time migration.
fn legacy_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    let parents: Vec<PathBuf> = std::env::var("LOCALAPPDATA")
        .map(|p| vec![PathBuf::from(p)])
        .unwrap_or_default();
    #[cfg(unix)]
    let parents: Vec<PathBuf> = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|p| vec![p])
        .unwrap_or_default();
    parents
        .iter()
        .flat_map(|p| LEGACY_ROOTS.iter().map(move |n| p.join(n)))
        .collect()
}

/// Durable per-host tree: overlay + sync baselines.
pub fn data_root(host: &str) -> PathBuf {
    app_dir().join("data").join(host)
}

/// Disposable per-host tree: downloaded blobs.
pub fn cache_root(host: &str) -> PathBuf {
    app_dir().join("cache").join(host)
}

/// Adopt one mount's data from the old flat layout, once.
///
/// The old paths were `<root>/overlay/<key>` and `<root>/cache/<key>` with
/// an opaque hashed key, which cannot be reversed into host/export — so
/// this runs at mount time, where both the old key and the new location are
/// known. Only ever moves INTO an empty destination.
pub fn migrate_legacy_mount(old_key: &str, host: &str, export: &str) {
    let moves = [
        (
            format!("overlay/{old_key}"),
            data_root(host).join("overlay").join(export),
        ),
        (format!("cache/{old_key}"), cache_root(host).join(export)),
        (
            format!("cache/{old_key}.manifest.json"),
            cache_root(host).join(format!("{export}.manifest.json")),
        ),
    ];
    for root in legacy_roots() {
        if !root.is_dir() {
            continue;
        }
        for (rel, dest) in &moves {
            let src = root.join(rel);
            if !src.exists() || dest.exists() {
                continue;
            }
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::rename(&src, dest) {
                Ok(()) => tracing::info!(
                    from = %src.display(), to = %dest.display(),
                    "migrated mount data to the ~/.alloyfs layout"
                ),
                Err(e) => tracing::warn!(
                    error = %e, from = %src.display(),
                    "could not migrate mount data; the old copy is left in place"
                ),
            }
        }
    }
}

/// A starter config, written when none exists. Commented rather than empty:
/// the first thing anyone needs is to see the shape of the file.
const CONFIG_TEMPLATE: &str = "\
# AlloyFS configuration.
#
# An alloyfs.yml placed NEXT TO THE EXECUTABLE overrides this file, which
# makes a portable install (binary + config on a stick or in one folder)
# work without touching the home directory.

agent:
  # Serve mounts over TCP. A non-loopback address REQUIRES tcp_token.
  tcp_listen: \"127.0.0.1:7440\"
  # tcp_token: \"change-me\"
  # http_listen: \"127.0.0.1:7441\"
  # http_token: \"change-me\"

# Folders this machine offers to others. Add one block per export.
exports: {}
#  projects:
#    path: /home/you/projects
#    read_only: false
#    exclude:
#      - \"**/.git\"
#    client:            # settings suggested to anyone who mounts this
#      exclude: [node_modules]
#      auto_cache_max: 2M
";

/// Names an auto-discovered config may have, most preferred first. JSON is
/// accepted because YAML parses it — see `AgentConfig::from_path`.
const CONFIG_NAMES: &[&str] = &["alloyfs.yml", "alloyfs.yaml", "alloyfs.json"];

/// The per-user config keeps its own name: `~/.alloyfs/alloyfs.yml` would
/// stutter, and this file predates the discovered ones.
const HOME_CONFIG_NAMES: &[&str] = &["config.yml", "config.yaml", "config.json"];

/// First of `names` that exists in `dir`. The order of `names` is the
/// preference order, so this is where "`.yml` beats `.json`" is decided.
fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The config to use when `--config` was not given.
///
/// Order:
///
/// 1. `./alloyfs.{yml,yaml,json}` — the directory you are standing in, which
///    is what `alloyfs init` writes. This is the same convention as cargo,
///    npm and docker compose: a tool invoked inside a project reads that
///    project's config.
/// 2. The same names beside the executable, for a portable install where the
///    binary and its config travel together.
/// 3. `~/.alloyfs/config.{yml,yaml,json}` — the per-user default.
/// 4. Pre-layout locations, for installs predating the current layout.
///
/// If nothing exists anywhere, the home config is CREATED from a commented
/// template and returned: a first run should leave you a file to edit, not an
/// error telling you to invent one.
///
/// The working directory is checked FIRST, above the executable's own folder.
/// Standing in a directory is the most deliberate signal available about which
/// config was meant — and the alternative surprises worse, by having a
/// portable binary silently ignore the config sitting right next to you.
/// Whichever is chosen is logged, because "which config is this agent
/// actually serving" should never need guessing.
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(found) = std::env::current_dir()
        .ok()
        .and_then(|cwd| first_existing(&cwd, CONFIG_NAMES))
    {
        tracing::info!(path = %found.display(), "using the config in the current directory");
        return Some(found);
    }

    // Portable install: next to the binary.
    if let Some(found) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().and_then(|dir| first_existing(dir, CONFIG_NAMES)))
    {
        tracing::info!(path = %found.display(), "using the config beside the executable");
        return Some(found);
    }

    let home_config = app_dir().join("config.yml");
    if let Some(found) = first_existing(&app_dir(), HOME_CONFIG_NAMES) {
        tracing::info!(path = %found.display(), "using the per-user config");
        return Some(found);
    }

    // Pre-layout locations, so an existing install keeps working. This
    // matters most for `serve --stdio`, spawned over ssh with no arguments
    // and no opportunity to be told where its config went.
    //
    // Windows has none: those configs lived at a hardcoded C:\MyApps, and a
    // fixed absolute path applies to EVERY install on the machine — a dev
    // build run out of target/release picked up an unrelated agent's config
    // and served the wrong folders. The exe-adjacent rule above covers that
    // layout properly, since the binary sits there too; a config with no
    // binary beside it is not ours.
    #[cfg(unix)]
    let legacy: Vec<PathBuf> = {
        let mut v = Vec::new();
        if let Ok(h) = std::env::var("HOME") {
            for dir in [".config/alloyfs", ".config/drive-sync"] {
                for name in ["agent.yml", "agent.yaml", "agent.toml"] {
                    v.push(PathBuf::from(&h).join(dir).join(name));
                }
            }
        }
        v
    };
    #[cfg(windows)]
    let legacy: Vec<PathBuf> = Vec::new();

    if let Some(found) = legacy.into_iter().find(|p| p.is_file()) {
        tracing::info!(path = %found.display(), "using a pre-layout config; move it to ~/.alloyfs/config.yml when convenient");
        return Some(found);
    }

    // Nothing anywhere: leave the user a file to edit.
    match std::fs::write(&home_config, CONFIG_TEMPLATE) {
        Ok(()) => {
            tracing::info!(path = %home_config.display(), "created a starter config");
            Some(home_config)
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %home_config.display(), "could not create a config");
            None
        }
    }
}

pub fn load_agent_config(config: Option<PathBuf>, inline_exports: &[String]) -> anyhow::Result<AgentConfig> {
    // A thin adapter over the unified loader: the agent still takes the shape
    // it always did, so nothing in `alloyfs-agent` had to learn about v3.
    //
    // `default_config_path` already logs which location won, and says which
    // one; logging again here printed two lines for one file and read as
    // though two configs had been considered.
    let mut cfg = match config {
        Some(path) => to_agent_config(&load(&path)?),
        // No explicit config: a default file (if present) supplies exports —
        // essential for `serve --stdio`, which is spawned with no arguments.
        None if inline_exports.is_empty() => match default_config_path() {
            Some(path) => to_agent_config(&load(&path)?),
            None => AgentConfig::default(),
        },
        None => AgentConfig::default(),
    };
    for spec in inline_exports {
        let (name, path) = spec
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--export wants NAME=PATH, got {spec}"))?;
        cfg.exports.insert(
            name.to_string(),
            alloyfs_agent::ExportConfig {
                path: PathBuf::from(path),
                read_only: false,
                exclude: vec![],
                // A throwaway `--export` gets the same OS-artifact hiding as a
                // configured one; opting out is a config-file decision.
                ..Default::default()
            },
        );
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order within one directory. Pinned because a reorder would be
    /// SILENT: every candidate parses, so picking the wrong one does not fail,
    /// it just serves whatever the other file said.
    #[test]
    fn yaml_is_preferred_over_json_in_one_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        for name in CONFIG_NAMES.iter().rev() {
            std::fs::write(dir.path().join(name), "exports: {}\n").unwrap();
            // Each write adds a *more* preferred name, so the answer must move.
            assert_eq!(
                first_existing(dir.path(), CONFIG_NAMES).unwrap(),
                dir.path().join(name)
            );
        }
    }

    /// `default_config_path` cannot be tested directly — it reads the process's
    /// working directory and `$HOME`, both global. What it composes can be:
    /// this asserts the two name lists it searches, so dropping `.json` or
    /// renaming the per-user file is a test failure rather than a support
    /// question.
    #[test]
    fn the_searched_names_are_the_documented_ones() {
        assert_eq!(CONFIG_NAMES, ["alloyfs.yml", "alloyfs.yaml", "alloyfs.json"]);
        assert_eq!(HOME_CONFIG_NAMES, ["config.yml", "config.yaml", "config.json"]);
    }

    #[test]
    fn an_empty_directory_yields_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(first_existing(dir.path(), CONFIG_NAMES).is_none());
        // A directory named like a config is not a config.
        std::fs::create_dir(dir.path().join("alloyfs.yml")).unwrap();
        assert!(first_existing(dir.path(), CONFIG_NAMES).is_none());
    }

    /// JSON has to actually load, not merely be listed. YAML 1.2 is a superset
    /// of JSON, which is why there is no second parser — this asserts the
    /// assumption rather than trusting it.
    #[test]
    fn a_json_config_parses_through_the_yaml_loader() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("alloyfs.json");
        std::fs::write(
            &path,
            r#"{"agent":{"tcp_listen":"127.0.0.1:7440"},
                "exports":{"docs":{"path":"/srv/docs","read_only":true}}}"#,
        )
        .unwrap();

        let cfg = alloyfs_agent::AgentConfig::from_path(&path).expect("json config loads");
        let export = cfg.exports.get("docs").expect("export present");
        assert_eq!(export.path, std::path::PathBuf::from("/srv/docs"));
        assert!(export.read_only);
        assert_eq!(cfg.agent.tcp_listen.as_deref(), Some("127.0.0.1:7440"));
    }

    /// The same document in both dialects has to produce the same config —
    /// otherwise "JSON is just YAML" is a claim rather than a fact.
    #[test]
    fn json_and_yaml_agree() {
        let dir = tempfile::TempDir::new().unwrap();
        let j = dir.path().join("a.json");
        let y = dir.path().join("a.yml");
        std::fs::write(&j, r#"{"exports":{"p":{"path":"/srv/p"}}}"#).unwrap();
        std::fs::write(&y, "exports:\n  p:\n    path: /srv/p\n").unwrap();

        let from_json = alloyfs_agent::AgentConfig::from_path(&j).unwrap();
        let from_yaml = alloyfs_agent::AgentConfig::from_path(&y).unwrap();
        assert_eq!(
            from_json.exports.get("p").unwrap().path,
            from_yaml.exports.get("p").unwrap().path
        );
    }
}
