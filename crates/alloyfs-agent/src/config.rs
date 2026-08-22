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
    /// Opt this server's OUTGOING large frames into zstd on v13+ sessions
    /// — better ratio than the always-on lz4 where it matters, the
    /// bandwidth-bound link serving compressible trees. Off by default;
    /// each direction opts in independently (clients use `--zstd`), and a
    /// v13 peer decodes both algorithms regardless of its own setting.
    #[serde(default)]
    pub zstd: bool,
}

/// Hand-written so that an export may be written either way — see the type's
/// documentation. `Serialize` stays derived, and always emits the table.
impl<'de> Deserialize<'de> for ExportConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Mirrors `ExportConfig`'s fields because serde cannot derive the
        // table form and the untagged wrapper on the same type. Both ends are
        // built exhaustively below — no `..` — so adding a field to
        // `ExportConfig` fails to compile here rather than silently becoming
        // unreadable from a config file.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            path: PathBuf,
            #[serde(default)]
            read_only: bool,
            #[serde(default)]
            exclude: Vec<String>,
            #[serde(default = "default_true")]
            default_excludes: bool,
            #[serde(default)]
            client: Option<ClientDefaults>,
            #[serde(default)]
            tree_max_entries: Option<usize>,
        }
        // A visitor rather than an untagged enum, and the difference is the
        // error message. Untagged tries each variant and reports only that
        // nothing matched, so a typo in the table form came out as "data did
        // not match any variant of untagged enum" pointing at the export's
        // name — strictly worse than the "unknown field `read_onyl`" it
        // replaced. Dispatching on the input's own shape means the table's
        // branch reports the table's own errors, unchanged.
        struct ExportVisitor;
        impl<'de> serde::de::Visitor<'de> for ExportVisitor {
            type Value = ExportConfig;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a path, or a table with a `path` key")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ExportConfig, E> {
                // Through `Default` rather than field by field: it is already
                // hand-written to get `default_excludes` right, and spelling
                // the defaults out again is how the two would drift apart.
                Ok(ExportConfig {
                    path: PathBuf::from(v),
                    ..ExportConfig::default()
                })
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<ExportConfig, A::Error> {
                let t = Table::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(ExportConfig {
                    path: t.path,
                    read_only: t.read_only,
                    exclude: t.exclude,
                    default_excludes: t.default_excludes,
                    client: t.client,
                    tree_max_entries: t.tree_max_entries,
                })
            }
        }
        d.deserialize_any(ExportVisitor)
    }
}

/// An export, written either as a bare path or as a table of settings.
///
/// ```yaml
/// exports:
///   docs: /srv/docs              # everything default
///   projects:                    # the same export, with settings
///     path: /home/you/projects
///     read_only: true
/// ```
///
/// The short form exists because it is what people write: it is the shape
/// `--export NAME=PATH` takes on the command line, and it is what every other
/// tool with a name→path map accepts. Until it was allowed, the natural
/// spelling failed with "invalid type: string, expected struct
/// ExportConfig". Serializing always emits the table, so a config the CLI
/// rewrites comes back in one shape rather than two.
///
/// `Deserialize` is therefore hand-written below; the `serde(default)`
/// attributes that would normally carry the defaults live in its inner
/// mirror struct instead. What stays here is `skip_serializing_if`, which is
/// the derived `Serialize`'s business.
#[derive(Debug, Clone, Serialize)]
pub struct ExportConfig {
    pub path: PathBuf,
    pub read_only: bool,
    /// Gitignore-flavored globs. Matching paths exist on the server but are
    /// never listed, resolvable, or event-broadcast to any client.
    pub exclude: Vec<String>,
    /// Also hide the OS bookkeeping in `alloyfs_common::LOCAL_ARTIFACTS`
    /// (`System Volume Information`, recycle bins, `.DS_Store`, …). On by
    /// default: a client mounting this export would otherwise create its own
    /// machine's volume-service directories inside someone else's folder.
    ///
    /// Set false only when the export IS a whole volume being backed up and
    /// those directories are part of what you meant to copy.
    pub default_excludes: bool,
    /// Suggested CLIENT settings, sent to v2+ mounts at attach time. The
    /// client unions the lists with its own and uses the sizes only where it
    /// has no explicit value; `--no-server-defaults` opts out entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientDefaults>,
    /// Entries above which this export is left unindexed and clients fall back
    /// to per-directory `Readdir` (v6). Defaults to
    /// [`crate::tree::DEFAULT_MAX_ENTRIES`].
    ///
    /// The index trades memory for round trips, and the exchange rate depends
    /// on the box: roughly 150 bytes an entry against one saved round trip per
    /// directory. Worth raising on a server with headroom and a deep tree;
    /// worth lowering on a small one. Set 0 to switch indexing off entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_max_entries: Option<usize>,
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
            tree_max_entries: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Both spellings of an export mean the same thing, and the short one
    /// takes every default the long one would.
    #[test]
    fn an_export_may_be_a_bare_path_or_a_table() {
        let both: BTreeMap<String, ExportConfig> =
            serde_yaml::from_str("short: /srv/docs\nlong:\n  path: /srv/docs\n  read_only: true\n")
                .expect("both forms parse");

        let short = &both["short"];
        assert_eq!(short.path, PathBuf::from("/srv/docs"));
        assert!(!short.read_only, "defaults to writable");
        assert!(
            short.default_excludes,
            "the short form must not quietly turn OFF a default-on setting"
        );
        assert!(short.exclude.is_empty());
        assert!(short.client.is_none());
        assert!(short.tree_max_entries.is_none());

        assert_eq!(both["long"].path, short.path);
        assert!(both["long"].read_only);
    }

    /// A misspelled key still names itself.
    ///
    /// This is the assertion that keeps the two-forms support from costing
    /// more than it gives: an untagged enum accepts both shapes just as well,
    /// but reports a typo as "data did not match any variant of untagged
    /// enum" against the export's NAME — throwing away both the field and the
    /// line. Dispatching on the input's shape keeps serde's own message.
    #[test]
    fn an_unknown_key_in_the_table_form_names_the_key() {
        let err = serde_yaml::from_str::<BTreeMap<String, ExportConfig>>(
            "docs:\n  path: /srv/docs\n  read_onyl: true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("read_onyl"),
            "the message must name the misspelled key, not just refuse: {err}"
        );
        assert!(
            err.contains("line 3"),
            "and must still locate it, which untagged also loses: {err}"
        );
    }

    /// The short form is a string, and anything that is neither a string nor
    /// a table says what it wanted rather than what it saw.
    #[test]
    fn a_nonsense_export_explains_both_forms() {
        let err = serde_yaml::from_str::<BTreeMap<String, ExportConfig>>("docs: [1, 2]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("path"), "{err}");
    }

    /// Serialization is always the table form, so a config the CLI rewrites
    /// comes back in one shape rather than two.
    #[test]
    fn serializing_always_writes_the_table_form() {
        let cfg = ExportConfig {
            path: PathBuf::from("/srv/docs"),
            read_only: false,
            exclude: Vec::new(),
            default_excludes: true,
            client: None,
            tree_max_entries: None,
        };
        let out = serde_yaml::to_string(&cfg).unwrap();
        assert!(out.contains("path:"), "must round-trip as a table: {out}");
    }
}
