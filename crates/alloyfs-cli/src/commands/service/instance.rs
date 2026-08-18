//! What one managed instance is — and, more to the point, what it is not.
//!
//! An instance is a **reference**. It records an id and which part of the
//! config the child should run: the whole config, one half of it, or one named
//! mount. It carries no url, no mountpoint, no excludes and no cache sizes.
//! Those live in exactly one place — `client.mounts` in the user's config —
//! and are read at launch by the child, which runs as that user and can
//! therefore see them.
//!
//! Instances used to hold their own copy of every mount flag, so two files
//! described one drive and nothing kept them in step. Editing the config left
//! the service mounting yesterday's settings, with nothing on screen anywhere
//! to suggest why.
//!
//! The store stays in ProgramData regardless. The service runs as LocalSystem
//! and cannot read `C:\Users\<name>`, so what it reads here is the reference
//! and only the child — running as the user, with the user's environment —
//! resolves it.

// Windows-only in practice: every command in the parent module returns the
// "use service.sh" error on other platforms, so nothing here is reached and
// `-D warnings` turns each unused item into a build failure. Allowed rather
// than cfg-gated because the module is not Windows-specific in principle —
// systemd units will consume exactly these definitions.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which part of the config a `start` instance runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The agent from `server:`, then every mount under `client.mounts:`.
    #[default]
    All,
    /// The agent alone.
    Server,
    /// The mounts alone — for a machine where something else already runs the
    /// agent, such as the scheduled task the Windows installer registers.
    Mounts,
}

/// One managed unit, as a pointer at what the config already says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Stored", into = "Stored")]
pub enum Instance {
    /// `alloyfs start` — everything the config describes, or one half of it.
    Start {
        /// Which config, when the child should not discover one for itself.
        /// It is opened by the CHILD, as the logged-in user, so it has to be a
        /// path that user can read.
        config: Option<PathBuf>,
        scope: Scope,
    },
    /// `alloyfs mount <name>` — one entry under `client.mounts`.
    Mount { name: String, config: Option<PathBuf> },
    /// A definition written before instances became references.
    Legacy(Legacy),
}

/// The shapes that were written when an instance carried its own settings.
///
/// Read so an existing registration survives an upgrade; never written. These
/// are not converted into references, because the conversion is not available
/// to whoever reads the file: a url and a mountpoint do not say WHICH
/// `client.mounts` entry they correspond to, and the process that reads this
/// file is the LocalSystem service, which cannot open the user's config to
/// find out. So a legacy instance keeps running the exact command it always
/// ran, and `service list` marks it as one to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Legacy {
    Mount {
        url: String,
        mountpoint: PathBuf,
        exclude: Vec<String>,
        pin: Vec<String>,
        auto_cache_max: Option<String>,
        auto_cache_budget: Option<String>,
        detect_conflicts: bool,
    },
    Agent {
        config: Option<PathBuf>,
        tcp: Option<String>,
    },
}

impl Instance {
    /// The argv this instance runs as, minus the executable.
    ///
    /// Built here rather than at the call site so the service and any future
    /// caller cannot drift from each other about what an instance means.
    pub fn argv(&self) -> Vec<String> {
        match self {
            Self::Start { config, scope } => {
                let mut out = vec!["start".to_string()];
                match scope {
                    Scope::All => {}
                    Scope::Server => out.push("--server-only".into()),
                    Scope::Mounts => out.push("--mounts-only".into()),
                }
                push_config(&mut out, config);
                out
            }
            Self::Mount { name, config } => {
                let mut out = vec!["mount".to_string(), name.clone()];
                push_config(&mut out, config);
                out
            }
            Self::Legacy(legacy) => legacy.argv(),
        }
    }

    /// The command line this instance launches, for `service list` and for
    /// `add` to echo back.
    ///
    /// Literally the argv, because the child IS an ordinary CLI invocation:
    /// printing anything else would invite people to run something other than
    /// what the service runs when they try to reproduce a failure by hand.
    pub fn command(&self) -> String {
        self.argv().join(" ")
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Mount { .. } => "mount",
            Self::Legacy(_) => "legacy",
        }
    }

    /// Whether this instance ends up mounting a drive.
    ///
    /// `add` checks for WinFsp only when the answer is yes: an agent-only
    /// instance works perfectly well on a machine that never mounts anything,
    /// and refusing to register one would be a check inventing a requirement.
    pub fn mounts_anything(&self) -> bool {
        match self {
            Self::Start { scope, .. } => !matches!(scope, Scope::Server),
            Self::Mount { .. } => true,
            Self::Legacy(Legacy::Mount { .. }) => true,
            Self::Legacy(Legacy::Agent { .. }) => false,
        }
    }

    /// The config this instance pins the child to, if any.
    pub fn config(&self) -> Option<&PathBuf> {
        match self {
            Self::Start { config, .. } | Self::Mount { config, .. } => config.as_ref(),
            Self::Legacy(Legacy::Agent { config, .. }) => config.as_ref(),
            Self::Legacy(Legacy::Mount { .. }) => None,
        }
    }
}

impl Legacy {
    fn argv(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            Self::Mount {
                url,
                mountpoint,
                exclude,
                pin,
                auto_cache_max,
                auto_cache_budget,
                detect_conflicts,
            } => {
                out.push("mount".into());
                out.push(url.clone());
                out.push(mountpoint.display().to_string());
                for e in exclude {
                    out.push("--exclude".into());
                    out.push(e.clone());
                }
                for p in pin {
                    out.push("--pin".into());
                    out.push(p.clone());
                }
                if let Some(v) = auto_cache_max {
                    out.push("--auto-cache-max".into());
                    out.push(v.clone());
                }
                if let Some(v) = auto_cache_budget {
                    out.push("--auto-cache-budget".into());
                    out.push(v.clone());
                }
                if *detect_conflicts {
                    out.push("--detect-conflicts".into());
                }
            }
            Self::Agent { config, tcp } => {
                out.push("serve".into());
                if let Some(c) = config {
                    out.push("--config".into());
                    out.push(c.display().to_string());
                }
                if let Some(t) = tcp {
                    out.push("--tcp".into());
                    out.push(t.clone());
                }
            }
        }
        out
    }
}

fn push_config(out: &mut Vec<String>, config: &Option<PathBuf>) {
    if let Some(path) = config {
        out.push("--config".into());
        out.push(path.display().to_string());
    }
}

// ------------------------------------------------------------------ on disk

/// The on-disk form, wide enough to hold every shape that has ever been
/// written.
///
/// One flat struct rather than a tagged enum because the two `kind: mount`
/// shapes have to be told apart by their contents: the reference names a
/// mount, the old copy carries a url. A serde enum would have to guess.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    kind: String,

    // The reference form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<Scope>,

    // Copied settings: read, never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mountpoint: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pin: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auto_cache_max: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auto_cache_budget: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    detect_conflicts: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tcp: Option<String>,
}

impl Stored {
    /// Whether any of the copied settings are present.
    ///
    /// A reference that also carries them is a half-edited file, and reading
    /// one half while ignoring the other is how a service comes to mount
    /// something nobody asked for.
    fn has_copied_settings(&self) -> bool {
        self.url.is_some()
            || self.mountpoint.is_some()
            || !self.exclude.is_empty()
            || !self.pin.is_empty()
            || self.auto_cache_max.is_some()
            || self.auto_cache_budget.is_some()
            || self.detect_conflicts
            || self.tcp.is_some()
    }
}

impl TryFrom<Stored> for Instance {
    type Error = anyhow::Error;

    fn try_from(stored: Stored) -> anyhow::Result<Self> {
        match stored.kind.as_str() {
            "start" => {
                anyhow::ensure!(
                    stored.name.is_none(),
                    "a `start` instance runs the whole config; it cannot also name one mount \
                     (use kind: mount with name:)"
                );
                anyhow::ensure!(
                    !stored.has_copied_settings(),
                    "a `start` instance carries mount settings, which it cannot apply: \
                     they belong under `client.mounts` in the config the child reads"
                );
                Ok(Self::Start {
                    config: stored.config,
                    scope: stored.scope.unwrap_or_default(),
                })
            }
            "mount" => match (stored.name, stored.url) {
                (Some(name), None) => {
                    validate_mount_name(&name)?;
                    anyhow::ensure!(
                        stored.mountpoint.is_none()
                            && stored.exclude.is_empty()
                            && stored.pin.is_empty()
                            && stored.auto_cache_max.is_none()
                            && stored.auto_cache_budget.is_none()
                            && !stored.detect_conflicts
                            && stored.tcp.is_none(),
                        "this instance names the mount {name:?} AND carries settings of its \
                         own. The settings would be ignored: everything about {name:?} comes \
                         from `client.mounts.{name}` in the config."
                    );
                    Ok(Self::Mount {
                        name,
                        config: stored.config,
                    })
                }
                (None, Some(url)) => Ok(Self::Legacy(Legacy::Mount {
                    url,
                    mountpoint: stored
                        .mountpoint
                        .ok_or_else(|| anyhow::anyhow!("a mount with a url needs a mountpoint"))?,
                    exclude: stored.exclude,
                    pin: stored.pin,
                    auto_cache_max: stored.auto_cache_max,
                    auto_cache_budget: stored.auto_cache_budget,
                    detect_conflicts: stored.detect_conflicts,
                })),
                (Some(name), Some(url)) => anyhow::bail!(
                    "this instance names both a configured mount ({name}) and a url ({url}); \
                     one of the two is left over from an edit and guessing which would mount \
                     the wrong thing"
                ),
                (None, None) => anyhow::bail!(
                    "this instance says neither which configured mount to run (name:) nor \
                     what to mount (url: + mountpoint:)"
                ),
            },
            "agent" => Ok(Self::Legacy(Legacy::Agent {
                config: stored.config,
                tcp: stored.tcp,
            })),
            other => anyhow::bail!("unknown kind {other:?}: expected start, mount or agent"),
        }
    }
}

impl From<Instance> for Stored {
    fn from(instance: Instance) -> Self {
        let empty = Stored {
            kind: String::new(),
            name: None,
            config: None,
            scope: None,
            url: None,
            mountpoint: None,
            exclude: Vec::new(),
            pin: Vec::new(),
            auto_cache_max: None,
            auto_cache_budget: None,
            detect_conflicts: false,
            tcp: None,
        };
        match instance {
            Instance::Start { config, scope } => Stored {
                kind: "start".into(),
                config,
                // `all` is the default, so it stays out of the file: a
                // definition should show what is unusual about it.
                scope: (scope != Scope::All).then_some(scope),
                ..empty
            },
            Instance::Mount { name, config } => Stored {
                kind: "mount".into(),
                name: Some(name),
                config,
                ..empty
            },
            Instance::Legacy(Legacy::Mount {
                url,
                mountpoint,
                exclude,
                pin,
                auto_cache_max,
                auto_cache_budget,
                detect_conflicts,
            }) => Stored {
                kind: "mount".into(),
                url: Some(url),
                mountpoint: Some(mountpoint),
                exclude,
                pin,
                auto_cache_max,
                auto_cache_budget,
                detect_conflicts,
                ..empty
            },
            Instance::Legacy(Legacy::Agent { config, tcp }) => Stored {
                kind: "agent".into(),
                config,
                tcp,
                ..empty
            },
        }
    }
}

/// Instance ids become service names, file names and command-line arguments,
/// so anything outside a conservative set is rejected rather than escaped.
pub fn validate_id(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!id.is_empty(), "the instance id cannot be empty");
    anyhow::ensure!(id.len() <= 64, "the instance id is too long (max 64)");
    anyhow::ensure!(
        id.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "invalid instance id {id:?}: use letters, digits, dashes and underscores"
    );
    // A leading dash would be read as a flag wherever the id is passed along.
    anyhow::ensure!(!id.starts_with('-'), "the instance id cannot start with a dash");
    Ok(())
}

/// A mount name is a key in `client.mounts`, so it is freer than an instance
/// id — but two shapes still have to be caught.
///
/// A leading dash is read as a flag by the child. A value carrying `:`, `/` or
/// `\` is a mountpoint or a url, which is what an old `--url … --mount P:`
/// command line becomes when it is retyped against the current flags; caught
/// here it names the mistake, and left alone it registers a service that fails
/// at boot with nobody watching.
pub fn validate_mount_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!name.is_empty(), "the mount name cannot be empty");
    anyhow::ensure!(!name.starts_with('-'), "the mount name cannot start with a dash");
    anyhow::ensure!(
        !name.contains([':', '/', '\\']),
        "{name:?} is a path or a url, not a mount name.\n\
         \n\
         \x20 A service names a mount that the config already describes:\n\
         \x20   client:\n\
         \x20     mounts:\n\
         \x20       work: {{ url: ssh://host/projects, at: \"P:\" }}\n\
         \x20 then: alloyfs service add work --mount work"
    );
    Ok(())
}

/// Where instance definitions live.
///
/// ProgramData, not the user profile: the service runs as LocalSystem and
/// cannot see `C:\Users\<name>`. `setup` restricts this directory to SYSTEM
/// and Administrators, because whoever can write here chooses what a SYSTEM
/// service launches. What lives here is a reference — the settings it points
/// at are in the user's config, which only the child ever reads.
#[cfg(windows)]
pub fn store_dir() -> PathBuf {
    let root = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
    PathBuf::from(root).join("alloyfs").join("services")
}

#[cfg(not(windows))]
pub fn store_dir() -> PathBuf {
    PathBuf::from("/etc/alloyfs/services")
}

pub fn instance_path(id: &str) -> PathBuf {
    store_dir().join(format!("{id}.yml"))
}

pub fn load(id: &str) -> anyhow::Result<Instance> {
    let path = instance_path(id);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("no service {id:?} ({}): {e}", path.display()))?;
    serde_yaml::from_str(&text).map_err(|e| anyhow::anyhow!("service {id:?} is malformed: {e}"))
}

pub fn save(id: &str, instance: &Instance) -> anyhow::Result<()> {
    let dir = store_dir();
    std::fs::create_dir_all(&dir)?;
    let path = instance_path(id);
    std::fs::write(&path, serde_yaml::to_string(instance)?)?;
    Ok(())
}

/// Every defined instance, sorted, ignoring anything that is not a `.yml`.
pub fn list_ids() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(store_dir()) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = rd
        .flatten()
        .filter_map(|item| {
            let path = item.path();
            (path.extension()? == "yml").then(|| path.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    ids.sort();
    ids
}

/// The Windows service name for an instance. Prefixed so `service stop` with
/// no id can find every one of ours without a registry of its own.
pub fn service_name(id: &str) -> String {
    format!("alloyfs-{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> anyhow::Result<Instance> {
        Ok(serde_yaml::from_str(text)?)
    }

    #[test]
    fn ids_that_would_confuse_a_command_line_are_refused() {
        assert!(validate_id("work").is_ok());
        assert!(validate_id("my-mount_2").is_ok());
        assert!(validate_id("").is_err());
        assert!(validate_id("-x").is_err(), "would parse as a flag");
        assert!(validate_id("../etc/passwd").is_err(), "path traversal");
        assert!(validate_id("a b").is_err());
        assert!(validate_id(&"x".repeat(65)).is_err());
    }

    /// The mistake this catches is retyping the old command line, where
    /// `--mount` took a drive letter and now takes a name.
    #[test]
    fn a_mountpoint_is_not_a_mount_name() {
        assert!(validate_mount_name("work").is_ok());
        assert!(validate_mount_name("work.dev").is_ok());
        assert!(validate_mount_name("P:").is_err());
        assert!(validate_mount_name("C:\\mnt\\work").is_err());
        assert!(validate_mount_name("/mnt/work").is_err());
        assert!(validate_mount_name("ssh://azure/projects").is_err());
        assert!(validate_mount_name("-x").is_err());
        assert!(validate_mount_name("").is_err());
    }

    /// The argv is what the service actually launches, so a dropped flag here
    /// is a service that silently runs something other than what it says.
    #[test]
    fn a_reference_launches_the_command_it_names() {
        let all = Instance::Start {
            config: None,
            scope: Scope::All,
        };
        assert_eq!(all.argv(), ["start"]);
        assert!(all.mounts_anything());

        let mounts = Instance::Start {
            config: Some(PathBuf::from("C:/alloyfs.yml")),
            scope: Scope::Mounts,
        };
        assert_eq!(
            mounts.argv(),
            ["start", "--mounts-only", "--config", "C:/alloyfs.yml"]
        );

        let agent = Instance::Start {
            config: None,
            scope: Scope::Server,
        };
        assert_eq!(agent.argv(), ["start", "--server-only"]);
        assert!(
            !agent.mounts_anything(),
            "an agent-only instance must not demand WinFsp"
        );

        let one = Instance::Mount {
            name: "work".into(),
            config: None,
        };
        assert_eq!(one.argv(), ["mount", "work"]);
        assert_eq!(one.command(), "mount work");
    }

    /// A reference holds an id and a pointer, and nothing else. Anything that
    /// belongs in `client.mounts` appearing in this file is the duplication
    /// that the reference form exists to remove.
    #[test]
    fn a_reference_stores_no_mount_settings() {
        let text = serde_yaml::to_string(&Instance::Mount {
            name: "work".into(),
            config: None,
        })
        .unwrap();
        assert_eq!(text, "kind: mount\nname: work\n", "{text}");

        let text = serde_yaml::to_string(&Instance::Start {
            config: None,
            scope: Scope::All,
        })
        .unwrap();
        assert_eq!(text, "kind: start\n", "{text}");

        let text = serde_yaml::to_string(&Instance::Start {
            config: None,
            scope: Scope::Mounts,
        })
        .unwrap();
        assert_eq!(text, "kind: start\nscope: mounts\n", "{text}");
    }

    #[test]
    fn a_reference_round_trips_through_yaml() {
        for instance in [
            Instance::Start {
                config: None,
                scope: Scope::All,
            },
            Instance::Start {
                config: Some(PathBuf::from("D:/alloyfs.yml")),
                scope: Scope::Server,
            },
            Instance::Mount {
                name: "work".into(),
                config: Some(PathBuf::from("D:/alloyfs.yml")),
            },
        ] {
            let text = serde_yaml::to_string(&instance).unwrap();
            assert_eq!(parse(&text).unwrap(), instance, "{text}");
        }
    }

    /// The compatibility guarantee, in one test: a file written by the version
    /// that copied definitions still parses, and still launches the exact
    /// command it launched then. An upgrade must not silently change what a
    /// registered service mounts.
    #[test]
    fn a_pre_reference_definition_still_runs_what_it_always_ran() {
        let old = "kind: mount\n\
                   url: ssh://azure/projects\n\
                   mountpoint: 'P:'\n\
                   exclude:\n- node_modules\n- .git\n\
                   pin:\n- '*.lockb'\n\
                   auto_cache_max: 2M\n\
                   detect_conflicts: true\n";
        let instance = parse(old).unwrap();
        assert_eq!(instance.kind(), "legacy");
        assert_eq!(
            instance.argv(),
            [
                "mount",
                "ssh://azure/projects",
                "P:",
                "--exclude",
                "node_modules",
                "--exclude",
                ".git",
                "--pin",
                "*.lockb",
                "--auto-cache-max",
                "2M",
                "--detect-conflicts",
            ]
        );
        assert!(instance.mounts_anything());
    }

    #[test]
    fn a_pre_reference_agent_still_runs_serve() {
        let old = "kind: agent\nconfig: C:/alloyfs.yml\ntcp: 0.0.0.0:7440\n";
        let instance = parse(old).unwrap();
        assert_eq!(instance.kind(), "legacy");
        assert_eq!(
            instance.argv(),
            ["serve", "--config", "C:/alloyfs.yml", "--tcp", "0.0.0.0:7440"]
        );
        assert!(!instance.mounts_anything());
    }

    /// Nothing rewrites a legacy file, so it has to survive being read and
    /// written again — `save` after a `load` must not turn a working service
    /// into one that mounts nothing.
    #[test]
    fn a_pre_reference_definition_survives_a_rewrite() {
        let old = "kind: mount\nurl: tcp://127.0.0.1:7440/x\nmountpoint: 'S:'\n";
        let instance = parse(old).unwrap();
        let text = serde_yaml::to_string(&instance).unwrap();
        assert_eq!(parse(&text).unwrap(), instance, "{text}");
    }

    /// Half-edited files are the ones most likely to mount the wrong thing, so
    /// every ambiguous combination refuses to load rather than picking a side.
    #[test]
    fn an_ambiguous_definition_is_refused_rather_than_guessed() {
        // Neither form.
        assert!(parse("kind: mount\n").is_err());
        // Both forms at once.
        assert!(parse("kind: mount\nname: work\nurl: tcp://h/x\nmountpoint: 'P:'\n").is_err());
        // A reference with settings it cannot apply.
        assert!(parse("kind: mount\nname: work\nexclude: [node_modules]\n").is_err());
        assert!(parse("kind: start\nurl: tcp://h/x\n").is_err());
        assert!(parse("kind: start\nname: work\n").is_err());
        // A url with nowhere to put it.
        assert!(parse("kind: mount\nurl: tcp://h/x\n").is_err());
        // Shapes that never existed.
        assert!(parse("kind: whatever\n").is_err());
        assert!(parse("url: tcp://h/x\nmountpoint: 'P:'\n").is_err());
        assert!(parse("kind: start\nscope: everything\n").is_err());
        // A typo must not be silently ignored.
        assert!(parse("kind: mount\nnaem: work\n").is_err());
    }
}
