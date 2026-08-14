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

/// The project was called `drive-sync` before it was AlloyFS. These are the
/// paths that data actually lives in, so they cannot simply be renamed in
/// the source and forgotten: the overlay holds files that exist NOWHERE
/// ELSE (client-side excludes are never sent to any server), and the sync
/// manifests are what stop a re-sync from looking like a conflict.
const LEGACY_DIR_NAME: &str = "drive-sync";

fn data_dir_parent() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("USERPROFILE").map(|p| PathBuf::from(p).join("AppData/Local")))
            .unwrap_or_else(|_| PathBuf::from("."))
    }
    #[cfg(unix)]
    {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// The local data directory, adopting a pre-rename one if that is all there
/// is. Moving it is safe precisely once — when the new name does not exist
/// yet — and if the move fails we KEEP USING THE OLD PATH rather than
/// silently starting empty, because an empty overlay looks like "the user
/// deleted all their local-only files" to the next sync.
pub fn default_data_dir() -> PathBuf {
    let parent = data_dir_parent();
    let current = parent.join("alloyfs");
    let legacy = parent.join(LEGACY_DIR_NAME);

    if !current.exists() && legacy.is_dir() {
        match std::fs::rename(&legacy, &current) {
            Ok(()) => tracing::info!(
                from = %legacy.display(),
                to = %current.display(),
                "migrated data directory to the AlloyFS name"
            ),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %legacy.display(),
                    "could not migrate the data directory; continuing to use the old one"
                );
                return legacy;
            }
        }
    }
    current
}

/// Config search order when --config/--export are absent (matters for
/// `serve --stdio`, which is spawned remotely with no arguments).
/// YAML preferred; TOML kept for existing deployments.
/// Searches the AlloyFS locations first, then the pre-rename ones — an
/// existing install keeps working untouched, which matters most for
/// `serve --stdio`, spawned over ssh with no arguments and no chance to be
/// told where its config went.
pub fn default_config_path() -> Option<PathBuf> {
    #[cfg(unix)]
    let dirs: Vec<PathBuf> = std::env::var("HOME")
        .ok()
        .map(|h| {
            vec![
                PathBuf::from(&h).join(".config/alloyfs"),
                PathBuf::from(&h).join(".config/drive-sync"),
            ]
        })
        .unwrap_or_default();
    #[cfg(windows)]
    let dirs: Vec<PathBuf> = vec![PathBuf::from("C:\\MyApps")];

    #[cfg(unix)]
    let names = ["agent.yml", "agent.yaml", "agent.toml"];
    #[cfg(windows)]
    let names = [
        "alloyfs.yml",
        "alloyfs.yaml",
        "alloyfs.toml",
        "drive-sync.yml",
        "drive-sync.yaml",
        "drive-sync.toml",
    ];

    for dir in dirs {
        if let Some(found) = names.iter().map(|n| dir.join(n)).find(|p| p.is_file()) {
            return Some(found);
        }
    }
    None
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
