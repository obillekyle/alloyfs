//! Client/agent configuration: YAML mount config, size parsing, default
//! paths. CLI flags always override file values.

use std::path::PathBuf;

use alloyfs_agent::AgentConfig;

/// Per-mount client config file (YAML). All keys optional; CLI flags win.
#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub pin: Vec<String>,
    pub auto_cache_max: Option<SizeField>,
    pub auto_cache_budget: Option<SizeField>,
    pub data_dir: Option<PathBuf>,
    /// Ignore the server's suggested client settings for this mount.
    #[serde(default)]
    pub no_server_defaults: bool,
    /// Shared secret for token-protected TCP servers (agent.tcp_token).
    pub token: Option<String>,
}

impl MountConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        serde_yaml::from_str(&std::fs::read_to_string(path)?)
            .map_err(|e| anyhow::anyhow!("mount config {}: {e}", path.display()))
    }
}

// Size parsing lives in alloyfs-common (the agent's `client:` section uses the
// same forms); re-exported so callers keep one import path.
pub use alloyfs_common::{parse_size, SizeField};

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

/// The config to use when `--config` was not given.
///
/// Order: an `alloyfs.yml` beside the executable (portable install) wins,
/// then `~/.alloyfs/config.yml`, then the pre-layout locations. If nothing
/// exists at all, the home config is CREATED from a commented template and
/// returned — a first run should leave you with a file to edit, not an
/// error telling you to invent one.
pub fn default_config_path() -> Option<PathBuf> {
    // Portable override: next to the binary.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["alloyfs.yml", "alloyfs.yaml"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    tracing::info!(path = %candidate.display(), "using the config beside the executable");
                    return Some(candidate);
                }
            }
        }
    }

    let home_config = app_dir().join("config.yml");
    if home_config.is_file() {
        return Some(home_config);
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
    let mut cfg = match config {
        Some(path) => AgentConfig::from_path(&path)?,
        // No explicit config: a default file (if present) supplies exports —
        // essential for `serve --stdio`, which is spawned with no arguments.
        None if inline_exports.is_empty() => match default_config_path() {
            Some(path) => {
                tracing::info!(path = %path.display(), "using default config");
                AgentConfig::from_path(&path)?
            }
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
                client: None,
            },
        );
    }
    Ok(cfg)
}
