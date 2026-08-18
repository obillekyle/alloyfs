//! What one managed instance is, and where its definition lives.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One managed unit: either a drive to mount or an agent to run.
///
/// Tagged rather than inferred from which fields are present, so a half-filled
/// file fails to parse instead of being guessed at — a service that silently
/// serves when it was meant to mount is worse than one that refuses to start.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Instance {
    Mount {
        url: String,
        /// Drive letter (`P:`) or directory.
        mountpoint: PathBuf,
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
    },
    Agent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tcp: Option<String>,
    },
}

impl Instance {
    /// One-line description for `service list`.
    pub fn summary(&self) -> String {
        match self {
            Self::Mount { url, mountpoint, .. } => format!("{url} -> {}", mountpoint.display()),
            Self::Agent { config, tcp } => match (config, tcp) {
                (Some(c), _) => format!("serve --config {}", c.display()),
                (None, Some(t)) => format!("serve --tcp {t}"),
                (None, None) => "serve".to_string(),
            },
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Mount { .. } => "mount",
            Self::Agent { .. } => "agent",
        }
    }

    /// The argv this instance runs as, minus the executable.
    ///
    /// Built here rather than at the call site so the service and any future
    /// caller cannot drift from each other about what an instance means.
    pub fn argv(&self) -> Vec<String> {
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

/// Where instance definitions live.
///
/// ProgramData, not the user profile: the service runs as LocalSystem and
/// cannot see `C:\Users\<name>`. `setup` restricts this directory to SYSTEM
/// and Administrators, because whoever can write here chooses what a SYSTEM
/// service launches.
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

    /// The argv is what the service actually launches, so a dropped flag here
    /// is a mount that silently behaves differently from the one that was
    /// registered.
    #[test]
    fn a_mount_round_trips_through_argv() {
        let instance = Instance::Mount {
            url: "ssh://azure/projects".into(),
            mountpoint: PathBuf::from("P:"),
            exclude: vec!["node_modules".into(), ".git".into()],
            pin: vec!["*.lockb".into()],
            auto_cache_max: Some("2M".into()),
            auto_cache_budget: None,
            detect_conflicts: true,
        };
        let argv = instance.argv();
        assert_eq!(argv[0], "mount");
        assert_eq!(argv[1], "ssh://azure/projects");
        assert_eq!(argv[2], "P:");
        assert!(argv.windows(2).any(|w| w == ["--exclude", "node_modules"]));
        assert!(argv.windows(2).any(|w| w == ["--exclude", ".git"]));
        assert!(argv.windows(2).any(|w| w == ["--pin", "*.lockb"]));
        assert!(argv.windows(2).any(|w| w == ["--auto-cache-max", "2M"]));
        assert!(argv.contains(&"--detect-conflicts".to_string()));
        assert!(
            !argv.contains(&"--auto-cache-budget".to_string()),
            "an unset option must not appear at all"
        );
    }

    #[test]
    fn an_agent_round_trips_through_argv() {
        let instance = Instance::Agent {
            config: Some(PathBuf::from("C:/alloyfs.yml")),
            tcp: None,
        };
        assert_eq!(instance.argv(), ["serve", "--config", "C:/alloyfs.yml"]);
        assert_eq!(instance.kind(), "agent");
    }

    /// The on-disk form is read by a SYSTEM service and written by the CLI, so
    /// it has to survive the round trip in both directions.
    #[test]
    fn the_yaml_form_round_trips() {
        let instance = Instance::Mount {
            url: "tcp://127.0.0.1:7440/x".into(),
            mountpoint: PathBuf::from("S:"),
            exclude: vec![],
            pin: vec![],
            auto_cache_max: None,
            auto_cache_budget: None,
            detect_conflicts: false,
        };
        let text = serde_yaml::to_string(&instance).unwrap();
        assert!(text.contains("kind: mount"), "{text}");
        // Empty collections stay out of the file rather than adding noise.
        assert!(!text.contains("exclude"), "{text}");
        let back: Instance = serde_yaml::from_str(&text).unwrap();
        assert_eq!(back.argv(), instance.argv());
    }

    #[test]
    fn a_file_with_no_kind_is_refused_rather_than_guessed() {
        let bad = "url: tcp://x/y\nmountpoint: 'P:'\n";
        assert!(serde_yaml::from_str::<Instance>(bad).is_err());
    }
}
