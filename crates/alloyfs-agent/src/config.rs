use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Agent configuration, usually from a TOML file:
///
/// ```toml
/// [agent]
/// tcp_listen = "0.0.0.0:7440"
///
/// [exports.projects]
/// path = "/home/kyle/projects"
/// read_only = false
/// ```
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub exports: BTreeMap<String, ExportConfig>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSection {
    pub tcp_listen: Option<String>,
    /// Shared secret required from every TCP mount (protocol v3+ Auth
    /// request). Mandatory when tcp_listen is not loopback; optional on
    /// localhost. ssh/stdio sessions never need it — ssh already
    /// authenticated the user.
    pub tcp_token: Option<String>,
    /// Optional HTTP API (status/browse/files/SSE events), e.g. "127.0.0.1:7441".
    pub http_listen: Option<String>,
    /// Bearer token required on every /api request (`Authorization: Bearer …`).
    /// Mandatory when http_listen is not loopback; optional on localhost.
    pub http_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
    /// Gitignore-flavored globs. Matching paths exist on the server but are
    /// never listed, resolvable, or event-broadcast to any client.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Also hide the OS bookkeeping in `alloyfs_common::LOCAL_ARTIFACTS`
    /// (`System Volume Information`, recycle bins, `.DS_Store`, …). On by
    /// default: a client mounting this export would otherwise create its own
    /// machine's volume-service directories inside someone else's folder.
    ///
    /// Set false only when the export IS a whole volume being backed up and
    /// those directories are part of what you meant to copy.
    #[serde(default = "default_true")]
    pub default_excludes: bool,
    /// Suggested CLIENT settings, sent to v2+ mounts at attach time. The
    /// client unions the lists with its own and uses the sizes only where it
    /// has no explicit value; `--no-server-defaults` opts out entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientDefaults>,
}

fn default_true() -> bool {
    true
}

/// Hand-written rather than derived so `default_excludes` defaults to TRUE in
/// both directions — a derive would make it false and quietly disagree with
/// what the config file does when the key is omitted.
impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            read_only: false,
            exclude: Vec::new(),
            default_excludes: true,
            client: None,
        }
    }
}

/// The `client:` section of an export: what this server recommends mounts
/// configure locally (overlay excludes, pins, cache sizing).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientDefaults {
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub pin: Vec<String>,
    pub auto_cache_max: Option<alloyfs_common::SizeField>,
    pub auto_cache_budget: Option<alloyfs_common::SizeField>,
}

impl AgentConfig {
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// Parse by extension: .yml/.yaml/.json → YAML (preferred), .toml → TOML
    /// (back-compat), anything else tries YAML then TOML.
    ///
    /// JSON goes through the YAML parser deliberately rather than pulling in a
    /// second one: YAML 1.2 is a superset of JSON, so every valid JSON config
    /// already parses, and one parser means one set of behaviours to know.
    pub fn from_path(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext.to_ascii_lowercase().as_str() {
            "yml" | "yaml" | "json" => Ok(serde_yaml::from_str(&text)?),
            "toml" => Self::from_toml(&text),
            _ => serde_yaml::from_str(&text).or_else(|ye| {
                Self::from_toml(&text)
                    .map_err(|te| anyhow::anyhow!("config parses as neither YAML ({ye}) nor TOML ({te})"))
            }),
        }
    }
}
