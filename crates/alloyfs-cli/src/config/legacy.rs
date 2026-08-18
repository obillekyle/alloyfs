//! The pre-v3 mount config, kept because files in this shape still exist.
//!
//! It was only ever loaded through `--config`, never discovered, and it
//! carried no url or mountpoint — both were always command-line arguments.
//! That is why it converts into `client:` DEFAULTS with no named mounts:
//! there was never a name to give one.
//!
//! Nothing new should be added here. The current schema is in `schema.rs`.

use std::path::PathBuf;

use alloyfs_common::SizeField;

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
    /// Refuse writes over concurrently-modified files (CLI: --detect-conflicts).
    #[serde(default)]
    pub detect_conflicts: bool,
    /// Shared secret for token-protected TCP servers (agent.tcp_token).
    pub token: Option<String>,
}
