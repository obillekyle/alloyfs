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
}

impl MountConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        serde_yaml::from_str(&std::fs::read_to_string(path)?)
            .map_err(|e| anyhow::anyhow!("mount config {}: {e}", path.display()))
    }
}

/// Accepts `auto_cache_max: 2M` (string) or `auto_cache_max: 2097152` (int).
#[derive(serde::Deserialize)]
#[serde(untagged)]
pub enum SizeField {
    Bytes(u64),
    Human(String),
}

impl SizeField {
    pub fn to_bytes(&self) -> Result<u64, String> {
        match self {
            SizeField::Bytes(n) => Ok(*n),
            SizeField::Human(s) => parse_size(s),
        }
    }
}

/// "2M" → 2 MiB, "512K", "1G", bare digits = bytes, "0" disables.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size".into());
    }
    let (digits, mult) = match s.chars().last().unwrap().to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1024u64),
        'M' => (&s[..s.len() - 1], 1024 * 1024),
        'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (s, 1),
        c => return Err(format!("unknown size suffix {c:?} in {s:?}")),
    };
    digits
        .parse::<u64>()
        .map_err(|e| format!("bad size {s:?}: {e}"))?
        .checked_mul(mult)
        .ok_or_else(|| format!("size {s:?} overflows"))
}

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
            },
        );
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_forms() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("42"), Ok(42));
        assert_eq!(parse_size("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_size("512k"), Ok(512 * 1024));
        assert_eq!(parse_size("1G"), Ok(1024 * 1024 * 1024));
        assert!(parse_size("").is_err());
        assert!(parse_size("2X").is_err());
        assert!(parse_size("M").is_err());
    }
}
