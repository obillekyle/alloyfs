//! `alloyfs service` — mounts and agents that start on their own.
//!
//! Three rules shape everything here.
//!
//! **One source of truth.** An instance is a reference to what the config
//! already describes — `alloyfs start`, or `alloyfs mount work` — never a
//! second copy of it. See [`instance`] for what that costs and buys.
//!
//! **No terminal.** The managed process is launched with `CREATE_NO_WINDOW`
//! into the interactive session, so nothing flashes at logon.
//!
//! **No elevation prompt.** Every subcommand that touches the service control
//! manager requires an already-elevated shell and says so plainly. A tool that
//! silently re-launches itself through `runas` teaches people to click through
//! a UAC dialog they did not ask for, which is a worse habit than an error
//! message. Elevation is not optional cosmetics either: without it the mount
//! cannot register with the Mount Manager, and a session-local drive letter
//! breaks `GetFinalPathNameByHandle` round-trips — the reason bun reports
//! spurious ENOENT on this class of drive.

// On non-Windows every command below is `#[cfg(not(windows))] return
// unsupported();` followed by a cfg'd-out block. The `return` is load-bearing
// there — without it the function would fall through to nothing — but clippy
// only sees the last statement in a function and calls it needless.
#![cfg_attr(not(windows), allow(clippy::needless_return))]
pub mod instance;

#[cfg(windows)]
pub mod runtime;
#[cfg(windows)]
pub mod spawn;

#[cfg(windows)]
mod scm;

pub use instance::{Instance, Scope};

/// Everything except `list` refuses to run unelevated.
#[cfg(windows)]
pub fn require_elevation() -> anyhow::Result<()> {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    anyhow::ensure!(
        opened != 0,
        "OpenProcessToken: {}",
        std::io::Error::last_os_error()
    );

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut size = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    };
    unsafe { CloseHandle(token) };
    anyhow::ensure!(
        ok != 0,
        "GetTokenInformation: {}",
        std::io::Error::last_os_error()
    );

    anyhow::ensure!(
        elevation.TokenIsElevated != 0,
        "this needs an elevated terminal.\n\
         \n\
         Open PowerShell or Terminal with \"Run as administrator\" and try again. \
         alloyfs will not raise its own privileges: registering a service that runs \
         at boot is not something a tool should arrange on your behalf behind a \
         dialog you did not open."
    );
    Ok(())
}

// Never reached on this platform — every command bails with `unsupported()`
// first — but kept so the call site does not need a cfg of its own.
#[cfg_attr(not(windows), allow(dead_code))]
#[cfg(not(windows))]
pub fn require_elevation() -> anyhow::Result<()> {
    Ok(())
}

/// Everything on this platform routes through here.
#[cfg(not(windows))]
fn unsupported() -> anyhow::Result<()> {
    anyhow::bail!(
        "`alloyfs service` is Windows-only for now.\n\
         \n\
         On Linux, install the agent as a systemd unit with:\n\
         \x20 curl -fsSL https://alloy.okyle.dev/service.sh | sudo sh"
    )
}

// ------------------------------------------------------------------ commands

pub fn setup() -> anyhow::Result<()> {
    #[cfg(not(windows))]
    return unsupported();
    #[cfg(windows)]
    {
        require_elevation()?;
        scm::verify_winfsp()?;
        let dir = instance::store_dir();
        std::fs::create_dir_all(&dir)?;
        scm::restrict_to_administrators(&dir)?;
        println!("ready.");
        println!("  instances: {}", dir.display());
        println!();
        println!("Add one with:");
        println!("  alloyfs service add alloyfs         # everything the config describes");
        println!("  alloyfs service add work --mount work   # one mount from client.mounts");
        Ok(())
    }
}

pub fn add(id: String, instance: Instance, start_now: bool) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = (id, instance, start_now);
        return unsupported();
    }
    #[cfg(windows)]
    {
        require_elevation()?;
        instance::validate_id(&id)?;
        if let Instance::Mount { name, .. } = &instance {
            instance::validate_mount_name(name)?;
        }
        anyhow::ensure!(
            !instance::instance_path(&id).exists(),
            "service {id:?} already exists; remove it first"
        );
        // An agent-only instance works on a machine that never mounts
        // anything, so the WinFsp check follows what the instance will do
        // rather than inventing a requirement for all of them.
        if instance.mounts_anything() {
            scm::verify_winfsp()?;
        }
        check_reference(&instance)?;

        instance::save(&id, &instance)?;
        scm::create(&id)?;
        println!("added {id}: alloyfs {}", instance.command());
        if start_now {
            scm::start(&id)?;
            println!("started.");
        } else {
            println!("It will start at boot. To start it now: alloyfs service start {id}");
        }
        Ok(())
    }
}

pub fn remove(id: String) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = id;
        return unsupported();
    }
    #[cfg(windows)]
    {
        require_elevation()?;
        instance::validate_id(&id)?;
        let _ = scm::stop(&id);
        scm::delete(&id)?;
        let path = instance::instance_path(&id);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        println!("removed {id}");
        Ok(())
    }
}

/// `start`, `stop` and `restart` all fan out the same way: one id, or every
/// instance when none is given.
pub fn control(action: &str, id: Option<String>) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = (action, id);
        return unsupported();
    }
    #[cfg(windows)]
    {
        require_elevation()?;
        let ids = match id {
            Some(one) => {
                instance::validate_id(&one)?;
                vec![one]
            }
            None => instance::list_ids(),
        };
        anyhow::ensure!(
            !ids.is_empty(),
            "no services defined; add one with `alloyfs service add`"
        );

        // Keep going after a failure and report at the end: stopping four of
        // five drives and aborting silently on the last is worse than saying
        // which one refused.
        let mut failed = Vec::new();
        for one in &ids {
            let outcome = match action {
                "start" => scm::start(one),
                "stop" => scm::stop(one),
                "restart" => scm::stop(one).and_then(|()| scm::start(one)),
                other => anyhow::bail!("unknown action {other}"),
            };
            match outcome {
                Ok(()) => println!("{action}ed {one}"),
                Err(e) => {
                    eprintln!("{one}: {e}");
                    failed.push(one.clone());
                }
            }
        }
        anyhow::ensure!(failed.is_empty(), "failed for: {}", failed.join(", "));
        Ok(())
    }
}

pub fn list() -> anyhow::Result<()> {
    #[cfg(not(windows))]
    return unsupported();
    #[cfg(windows)]
    {
        // Deliberately readable without elevation: "what is registered" is a
        // question worth answering from any shell.
        let ids = instance::list_ids();
        if ids.is_empty() {
            println!("no services defined.");
            println!("  alloyfs service add alloyfs   # everything the config describes");
            return Ok(());
        }
        // The command, not a paraphrase of it: the child is an ordinary CLI
        // invocation, and printing what it actually runs is what makes
        // reproducing a failure by hand a copy rather than a reconstruction.
        // Twelve for the state: "unregistered" is the longest of them, and it
        // is the one that shows up whenever a definition outlives its service.
        println!("ID               KIND    STATE        COMMAND");
        let mut carried_definitions = false;
        for id in ids {
            let (kind, command) = match instance::load(&id) {
                Ok(i) => {
                    carried_definitions |= matches!(i, Instance::Legacy(_));
                    (i.kind().to_string(), format!("alloyfs {}", i.command()))
                }
                Err(e) => ("?".into(), format!("unreadable: {e}")),
            };
            let state = scm::state(&id);
            println!("{id:<16} {kind:<7} {state:<12} {command}");
        }
        if carried_definitions {
            println!();
            println!(
                "The instances marked `legacy` carry their own copy of a mount definition,\n\
                 which the config cannot override. They keep working as they are. To move one:\n\
                 \x20 1. describe the mount under `client.mounts:` in your config\n\
                 \x20 2. alloyfs service remove <id>\n\
                 \x20 3. alloyfs service add <id> --mount <name>"
            );
        }
        Ok(())
    }
}

pub fn reset(confirm: bool) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = confirm;
        return unsupported();
    }
    #[cfg(windows)]
    {
        require_elevation()?;
        let ids = instance::list_ids();
        if !confirm {
            println!("This removes every AlloyFS service and its definition:");
            if ids.is_empty() {
                println!("  (nothing is defined)");
            }
            for id in &ids {
                println!("  {id}");
            }
            println!();
            anyhow::bail!("re-run with --confirm to proceed");
        }
        for id in &ids {
            let _ = scm::stop(id);
            if let Err(e) = scm::delete(id) {
                eprintln!("{id}: {e}");
            }
        }
        let dir = instance::store_dir();
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        println!("removed {} service(s).", ids.len());
        Ok(())
    }
}

/// Resolve what the instance points at, before anything is registered.
///
/// `add` runs in an elevated shell, as a person — so it can read the config
/// that the service itself never can, and answer the question that matters
/// while somebody is still watching: does this mount name exist? Registered
/// against a name that does not, the service starts at boot, fails inside a
/// process with no window, and says nothing. That is the same argument that
/// puts the WinFsp check here rather than at launch.
///
/// The config consulted is the one THIS shell can see. The child resolves it
/// again at launch as the console user, and the two answers differ when the
/// elevated shell belongs to a different account, or was opened in a directory
/// with an `alloyfs.yml` of its own. Naming the file that was read is what
/// turns that from a mystery into a sentence.
#[cfg(windows)]
fn check_reference(instance: &Instance) -> anyhow::Result<()> {
    let (path, cfg) = crate::config::load_with_path(instance.config().cloned())?;
    let source = match &path {
        Some(p) => p.display().to_string(),
        None => "no config file was found".to_string(),
    };

    let resolved = resolve_reference(instance, &cfg, &source)?;
    for line in &resolved.lines {
        println!("{line}");
    }
    warn_about_ssh(&resolved.urls);
    Ok(())
}

/// What an instance amounts to in one config.
///
/// The fields are read by `check_reference`, which is Windows-only, so on
/// other platforms they have no consumer outside the tests — and a derived
/// `Debug` does not count as one for dead-code analysis.
#[derive(Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Resolution {
    /// What to show whoever ran `add`: the mounts this service will start.
    lines: Vec<String>,
    /// Their urls, for the checks that depend on the scheme.
    urls: Vec<String>,
}

/// Work out what an instance points at, without touching the disk or the
/// terminal — which is what makes the interesting half of `add` testable on a
/// machine where registering a service is not possible.
#[cfg_attr(not(windows), allow(dead_code))]
fn resolve_reference(
    instance: &Instance,
    cfg: &crate::config::Config,
    source: &str,
) -> anyhow::Result<Resolution> {
    match instance {
        Instance::Mount { name, .. } => {
            let mount = cfg.mount(name).ok_or_else(|| unknown_mount(name, cfg, source))?;
            Ok(Resolution {
                lines: vec![format!("{name} -> {} at {}", mount.url, mount.at.display())],
                urls: vec![mount.url],
            })
        }
        Instance::Start { scope, .. } => {
            let mounts = if matches!(scope, Scope::Server) {
                Vec::new()
            } else {
                cfg.client
                    .as_ref()
                    .map(|c| c.resolved_mounts())
                    .unwrap_or_default()
            };
            let agent = !matches!(scope, Scope::Mounts) && cfg.has_exports();

            // Registering the service before filling in the config is a
            // legitimate order to do things in, so an empty config is a note
            // rather than a refusal — but a silent one would leave a service
            // that starts, does nothing, and looks healthy.
            let mut lines = Vec::new();
            if mounts.is_empty() && !agent {
                lines.push(format!(
                    "note: {source} describes nothing for this service to run yet."
                ));
            } else {
                lines.push(format!("from {source}:"));
                if agent {
                    lines.push("  the agent".to_string());
                }
                for (name, mount) in &mounts {
                    lines.push(format!("  {name} -> {} at {}", mount.url, mount.at.display()));
                }
            }
            Ok(Resolution {
                lines,
                urls: mounts.into_iter().map(|(_, m)| m.url).collect(),
            })
        }
        // Never built by `add`; only ever read from a file written before
        // instances became references.
        Instance::Legacy(_) => Ok(Resolution {
            lines: Vec::new(),
            urls: Vec::new(),
        }),
    }
}

/// The error for a name the config does not define, listing the ones it does.
#[cfg_attr(not(windows), allow(dead_code))]
fn unknown_mount(name: &str, cfg: &crate::config::Config, source: &str) -> anyhow::Error {
    let known = cfg.mount_names();
    if known.is_empty() {
        return anyhow::anyhow!(
            "no mount named `{name}`: {source} defines no mounts.\n\n\
             \x20 Describe it under `client.mounts:` first:\n\
             \x20   client:\n\
             \x20     mounts:\n\
             \x20       {name}: {{ url: ssh://host/projects, at: \"P:\" }}"
        );
    }
    anyhow::anyhow!(
        "no mount named `{name}` in {source}.\n\n\
         \x20 configured mounts: {}\n\n\
         \x20 The name is resolved again at launch, by whoever is logged in — so if this \
         shell reads a different config than they do, --config PATH pins both to one file.",
        known.join(", ")
    )
}

/// A LocalSystem service launches into the user's session with the user's
/// environment, so their SSH keys are the ones in scope. Worth confirming out
/// loud, because the opposite arrangement is the usual one and it fails at
/// boot for a reason nobody connects to SSH agent scope.
#[cfg(windows)]
fn warn_about_ssh(urls: &[String]) {
    if urls.iter().any(|url| url.starts_with("ssh://")) {
        println!(
            "note: ssh mounts authenticate as you, using your own SSH keys and agent.\n\
             \x20     Key-based auth must work non-interactively — check with `ssh <host> true`;\n\
             \x20     a key with a passphrase and no agent will hang at boot."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> crate::config::Config {
        serde_yaml::from_str(text).expect("should load")
    }

    const TWO_MOUNTS: &str = "client:\n  mounts:\n\
                              \x20   work: { url: 'ssh://azure/projects', at: 'P:' }\n\
                              \x20   media: { url: 'tcp://nas:7440/media', at: 'M:' }\n";

    /// The check that only `add` can make, because only `add` runs as a person
    /// with the config in reach. A name that is not there has to fail here or
    /// it fails at boot, inside a process with no window.
    #[test]
    fn a_name_the_config_does_not_define_is_refused_and_lists_the_ones_it_does() {
        let cfg = config(TWO_MOUNTS);
        let instance = Instance::Mount {
            name: "wrok".into(),
            config: None,
        };
        let err = resolve_reference(&instance, &cfg, "C:/alloyfs.yml")
            .expect_err("a typo must not register")
            .to_string();
        assert!(err.contains("wrok"), "{err}");
        assert!(err.contains("media, work"), "{err}");
        assert!(err.contains("C:/alloyfs.yml"), "names the file it read: {err}");
    }

    #[test]
    fn an_empty_config_says_where_mounts_go() {
        let err = resolve_reference(
            &Instance::Mount {
                name: "work".into(),
                config: None,
            },
            &config("version: 3\n"),
            "C:/alloyfs.yml",
        )
        .expect_err("nothing to resolve against")
        .to_string();
        assert!(err.contains("client.mounts"), "{err}");
    }

    /// A reference resolves to the mount the config describes, so `add` can
    /// show what it is about to register — and warn when the url is `ssh://`,
    /// which authenticates as the logged-in user rather than as the service.
    #[test]
    fn a_reference_resolves_to_what_the_config_says() {
        let cfg = config(TWO_MOUNTS);
        let one = resolve_reference(
            &Instance::Mount {
                name: "work".into(),
                config: None,
            },
            &cfg,
            "cfg",
        )
        .unwrap();
        assert_eq!(one.urls, ["ssh://azure/projects"]);
        assert_eq!(one.lines, ["work -> ssh://azure/projects at P:"]);
    }

    #[test]
    fn a_start_instance_covers_every_mount_and_the_agent() {
        let cfg = config(&format!(
            "{TWO_MOUNTS}server:\n  exports:\n    p: {{ path: /srv/p }}\n"
        ));

        let all = resolve_reference(
            &Instance::Start {
                config: None,
                scope: Scope::All,
            },
            &cfg,
            "cfg",
        )
        .unwrap();
        assert_eq!(all.urls.len(), 2);
        assert!(all.lines.iter().any(|l| l == "  the agent"), "{:?}", all.lines);

        // Halves are halves: the agent-only form must not pull in mounts,
        // because a machine that runs both would then mount everything twice.
        let server = resolve_reference(
            &Instance::Start {
                config: None,
                scope: Scope::Server,
            },
            &cfg,
            "cfg",
        )
        .unwrap();
        assert!(server.urls.is_empty());
        assert!(server.lines.iter().any(|l| l == "  the agent"));

        let mounts = resolve_reference(
            &Instance::Start {
                config: None,
                scope: Scope::Mounts,
            },
            &cfg,
            "cfg",
        )
        .unwrap();
        assert_eq!(mounts.urls.len(), 2);
        assert!(!mounts.lines.iter().any(|l| l == "  the agent"));
    }

    /// Registering the service first and writing the config afterwards is a
    /// reasonable order, so this is a note rather than a refusal — but it has
    /// to be said, or the result is a service that starts, does nothing, and
    /// looks healthy.
    #[test]
    fn a_config_describing_nothing_is_a_note_not_an_error() {
        let resolved = resolve_reference(
            &Instance::Start {
                config: None,
                scope: Scope::All,
            },
            &config("version: 3\n"),
            "C:/alloyfs.yml",
        )
        .unwrap();
        assert!(resolved.urls.is_empty());
        assert_eq!(resolved.lines.len(), 1);
        assert!(
            resolved.lines[0].contains("describes nothing"),
            "{:?}",
            resolved.lines
        );
    }
}
