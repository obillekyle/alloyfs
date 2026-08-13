//! Client/agent configuration: YAML mount config, size parsing, default
//! paths. CLI flags always override file values.

use std::path::PathBuf;

use ds_agent::AgentConfig;

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
}

impl MountConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        serde_yaml::from_str(&std::fs::read_to_string(path)?)
            .map_err(|e| anyhow::anyhow!("mount config {}: {e}", path.display()))
    }
}

// Size parsing lives in ds-common (the agent's `client:` section uses the
// same forms); re-exported so callers keep one import path.
pub use ds_common::{parse_size, SizeField};

pub fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("USERPROFILE").map(|p| PathBuf::from(p).join("AppData/Local")))
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("drive-sync")
    }
    #[cfg(unix)]
    {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("drive-sync")
    }
}

/// Config search order when --config/--export are absent (matters for
/// `serve --stdio`, which is spawned remotely with no arguments).
/// YAML preferred; TOML kept for existing deployments.
pub fn default_config_path() -> Option<PathBuf> {
    #[cfg(unix)]
    let dir = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/drive-sync"));
    #[cfg(windows)]
    let dir = Some(PathBuf::from("C:\\MyApps"));
    let dir = dir?;
    #[cfg(unix)]
    let names = ["agent.yml", "agent.yaml", "agent.toml"];
    #[cfg(windows)]
    let names = ["drive-sync.yml", "drive-sync.yaml", "drive-sync.toml"];
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
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
            ds_agent::ExportConfig {
                path: PathBuf::from(path),
                read_only: false,
                exclude: vec![],
                client: None,
            },
        );
    }
    Ok(cfg)
}
