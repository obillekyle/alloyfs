//! `alloyfs service` — mounts and agents that start on their own.
//!
//! **One source of truth.** An instance is a reference to what the config
//! already describes — `alloyfs start`, or `alloyfs mount work` — never a
//! second copy of it. See [`instance`] for what that costs and buys.
//!
//! **One command surface.** `setup`, `add`, `remove`, `start`, `stop`,
//! `restart`, `list` and `reset` mean the same things on both platforms, so
//! the bodies below are written once and the platform lives in [`scm`] on
//! Windows and [`systemd`] on Linux. Both answer to the same function names;
//! where a concept exists on only one of them, the other's version says so
//! rather than pretending.
//!
//! The two differ most in what they ask of whoever runs them, and it is worth
//! knowing why they differ in *opposite* directions:
//!
//! - **Windows demands an elevated shell.** A drive letter is per-session, so a
//!   mount has to be created inside the logged-in user's session by a process
//!   running as them. A service arranges that with LocalSystem, `SE_TCB_NAME`
//!   and a token swap — see [`runtime`] and [`spawn`] — and none of it is
//!   available unelevated. Nothing here raises its own privileges: a tool that
//!   silently re-launches itself through `runas` teaches people to click
//!   through a UAC dialog they did not ask for.
//! - **Linux refuses `sudo`.** A systemd user unit already runs as the user, in
//!   their session, with their environment and their keys, so there is nothing
//!   to arrange and nothing to elevate for. Running `add` under sudo would
//!   quietly register the mount for root instead.

pub mod instance;

#[cfg(windows)]
pub mod runtime;
#[cfg(windows)]
pub mod spawn;

#[cfg(windows)]
mod scm;
#[cfg(unix)]
mod systemd;

// One alias so the commands below never mention a platform. The two modules are
// kept deliberately parallel; adding a function to one without the other is a
// build failure on the platform that was forgotten, which is the point.
#[cfg(windows)]
use scm as reg;
#[cfg(unix)]
use systemd as reg;

pub use instance::{Instance, Scope};

/// The filesystem-driver preflight, shared with `mount`. The missing-driver
/// message — install URL included — should not depend on WHICH command
/// discovers the driver is missing; a plain `alloyfs mount` used to surface
/// the raw backend error instead.
pub(crate) fn verify_backend() -> anyhow::Result<()> {
    reg::verify_backend()
}

/// Is the platform's service supervisor reachable — the SCM here, the
/// systemd user bus there? For `doctor`, which reports it rather than
/// refusing to continue: a machine that mounts by hand needs no supervisor.
pub(crate) fn verify_supervisor_check() -> anyhow::Result<()> {
    reg::verify_supervisor()
}

/// The platform's note about services not starting until login, if it has
/// one (linger on Linux; Windows services start at boot regardless).
pub(crate) fn linger_note_check() -> Option<String> {
    reg::linger_note()
}

/// Registered instances and their states, on one line, for `doctor`.
///
/// Deliberately not `list`'s table: the question here is "is anything
/// registered, and is it running", not "what exactly does each one run".
pub(crate) fn instance_summary() -> anyhow::Result<String> {
    let ids = instance::list_ids();
    if ids.is_empty() {
        return Ok(String::new());
    }
    let mut parts = Vec::with_capacity(ids.len());
    for id in &ids {
        parts.push(format!("{id}={}", reg::state(id)));
    }
    Ok(format!("{} registered: {}", ids.len(), parts.join(", ")))
}

// ------------------------------------------------------------------ commands

pub fn setup() -> anyhow::Result<()> {
    reg::preflight().map_err(|e| crate::exit::Fatal::err(crate::exit::PRIVILEGE, format!("{e:#}")))?;
    reg::verify_supervisor()?;

    let dir = instance::store_dir();
    std::fs::create_dir_all(&dir)?;
    reg::prepare_store(&dir)?;

    println!("ready.");
    println!("  instances: {}", dir.display());

    // A note, not a refusal. An agent-only instance works perfectly well on a
    // machine that never mounts anything, and `add` is the gate that knows
    // whether this particular instance needs a filesystem driver — so refusing
    // here would invent a requirement for every instance from the needs of one.
    if let Err(e) = reg::verify_backend() {
        println!();
        println!("warning: {e}");
    }
    if let Some(note) = reg::linger_note() {
        println!();
        println!("{note}");
    }

    println!();
    println!("Add one with:");
    println!("  alloyfs service add alloyfs         # everything the config describes");
    println!("  alloyfs service add work --mount work   # one mount from client.mounts");
    Ok(())
}

pub fn add(id: String, instance: Instance, start_now: bool) -> anyhow::Result<()> {
    reg::preflight().map_err(|e| crate::exit::Fatal::err(crate::exit::PRIVILEGE, format!("{e:#}")))?;
    instance::validate_id(&id)?;
    if let Instance::Mount { name, .. } = &instance {
        instance::validate_mount_name(name)?;
    }
    anyhow::ensure!(
        !instance::instance_path(&id).exists(),
        "service {id:?} already exists; remove it first"
    );
    reg::verify_supervisor()?;
    // An agent-only instance works on a machine that never mounts anything, so
    // the filesystem-driver check follows what the instance will do rather than
    // inventing a requirement for all of them.
    if instance.mounts_anything() {
        reg::verify_backend()?;
    }
    check_reference(&instance)?;

    instance::save(&id, &instance)?;
    // A failed registration must not leave the definition behind. `add` refuses
    // an id that already has one, and `remove` used to give up before reaching
    // the file — so the orphan was unreachable by every command that could have
    // cleared it, and only `reset --confirm` or deleting it by hand got out.
    if let Err(e) = reg::create(&id, &instance) {
        let _ = std::fs::remove_file(instance::instance_path(&id));
        return Err(e);
    }
    println!("added {id}: alloyfs {}", instance.command());
    if start_now {
        reg::start(&id)?;
        println!("started.");
    } else {
        println!("To start it now: alloyfs service start {id}");
    }
    // When it comes back on its own. On Windows that is always "at boot"; a
    // systemd user unit says so only when the account lingers, and claiming
    // otherwise would be a promise the machine does not keep.
    match reg::linger_note() {
        None => println!("It starts again at boot."),
        Some(note) => println!("{note}"),
    }
    Ok(())
}

pub fn remove(id: String) -> anyhow::Result<()> {
    reg::preflight().map_err(|e| crate::exit::Fatal::err(crate::exit::PRIVILEGE, format!("{e:#}")))?;
    instance::validate_id(&id)?;
    let _ = reg::stop(&id);
    // Best-effort, and the definition goes either way. A registration that is
    // already absent is the state `remove` is being asked to reach, so treating
    // it as a failure left the definition stranded — refused by `add` for
    // existing, and never reached by `remove` because it gave up one line
    // earlier. Whatever the registry says, the file is what makes the id look
    // taken, so the file is what has to go.
    let unregistered = reg::delete(&id);
    let path = instance::instance_path(&id);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    match unregistered {
        Ok(()) => println!("removed {id}"),
        Err(e) => println!("removed the definition for {id}; it was not registered ({e})"),
    }
    Ok(())
}

/// `start`, `stop` and `restart` all fan out the same way: one id, or every
/// instance when none is given.
pub fn control(action: &str, id: Option<String>) -> anyhow::Result<()> {
    reg::preflight().map_err(|e| crate::exit::Fatal::err(crate::exit::PRIVILEGE, format!("{e:#}")))?;
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

    // Keep going after a failure and report at the end: stopping four of five
    // drives and aborting silently on the last is worse than saying which one
    // refused.
    let mut failed = Vec::new();
    for one in &ids {
        // "stop" does not take a bare -ed, and the report is the only evidence
        // most of these commands leave behind.
        let (outcome, done) = match action {
            "start" => (reg::start(one), "started"),
            "stop" => (reg::stop(one), "stopped"),
            "restart" => (reg::stop(one).and_then(|()| reg::start(one)), "restarted"),
            other => anyhow::bail!("unknown action {other}"),
        };
        match outcome {
            Ok(()) => println!("{done} {one}"),
            Err(e) => {
                eprintln!("{one}: {e}");
                failed.push(one.clone());
            }
        }
    }
    anyhow::ensure!(failed.is_empty(), "failed for: {}", failed.join(", "));
    Ok(())
}

pub fn list(json: bool) -> anyhow::Result<()> {
    // Deliberately skips `preflight`: "what is registered" is a question worth
    // answering from any shell, elevated or not.
    let ids = instance::list_ids();
    if json {
        let services: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                let (kind, command, legacy, error) = match instance::load(id) {
                    Ok(i) => (
                        Some(i.kind().to_string()),
                        Some(format!("alloyfs {}", i.command())),
                        matches!(i, Instance::Legacy(_)),
                        None,
                    ),
                    Err(e) => (None, None, false, Some(format!("{e}"))),
                };
                serde_json::json!({
                    "id": id,
                    "kind": kind,
                    "state": reg::state(id),
                    "command": command,
                    // The "why" behind the state, when the supervisor has
                    // one. This is the field worth alerting on: a crash-loop
                    // reads as `running` in `state`, because the supervisor
                    // is up — it is the child that keeps dying.
                    "detail": reg::detail(id),
                    "legacy": legacy,
                    "error": error,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "services": services }))?
        );
        return Ok(());
    }
    if ids.is_empty() {
        println!("no services defined.");
        println!("  alloyfs service add alloyfs   # everything the config describes");
        return Ok(());
    }
    // The command, not a paraphrase of it: the child is an ordinary CLI
    // invocation, and printing what it actually runs is what makes reproducing
    // a failure by hand a copy rather than a reconstruction. Twelve for the
    // state: "unregistered" is the longest of them, and it is the one that
    // shows up whenever a definition outlives its service.
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
        let state = reg::state(&id);
        println!("{id:<16} {kind:<7} {state:<12} {command}");
        // On its own line rather than a sixth column: this is the answer to
        // "why", it is only sometimes there, and a column wide enough for
        // it would push the command — the thing worth copying — off the
        // screen. It matters most for the state the column cannot express:
        // a supervisor reads `running` while the CHILD it keeps launching
        // dies over and over.
        if let Some(detail) = reg::detail(&id) {
            println!("{:<16} {:<7} {:<12} └ {detail}", "", "", "");
        }
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

pub fn reset(confirm: bool) -> anyhow::Result<()> {
    reg::preflight().map_err(|e| crate::exit::Fatal::err(crate::exit::PRIVILEGE, format!("{e:#}")))?;
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
        let _ = reg::stop(id);
        if let Err(e) = reg::delete(id) {
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

/// Resolve what the instance points at, before anything is registered.
///
/// `add` runs as a person, in a shell — so it can read the config and answer
/// the question that matters while somebody is still watching: does this mount
/// name exist? Registered against a name that does not, the service starts on
/// its own, fails where nobody is looking, and says nothing. That is the same
/// argument that puts the filesystem-driver check here rather than at launch.
///
/// The config consulted is the one THIS shell can see. On Windows the child
/// resolves it again at launch as the console user, and the two answers differ
/// when the elevated shell belongs to a different account, or was opened in a
/// directory with an `alloyfs.yml` of its own. Naming the file that was read is
/// what turns that from a mystery into a sentence.
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
#[derive(Debug)]
struct Resolution {
    /// What to show whoever ran `add`: the mounts this service will start.
    lines: Vec<String>,
    /// Their urls, for the checks that depend on the scheme.
    urls: Vec<String>,
}

/// Work out what an instance points at, without touching the disk or the
/// terminal — which is what makes the interesting half of `add` testable on a
/// machine where registering a service is not possible.
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

/// An `ssh://` mount authenticates as the user, with the user's agent — worth
/// confirming out loud on both platforms, because the opposite arrangement is
/// the usual one for services and it fails at a moment nobody connects to SSH.
///
/// The failure looks different on each. A LocalSystem service launches into the
/// user's session with the user's environment, so their agent is simply in
/// scope. A systemd user unit inherits the *user manager's* environment, which
/// is not the shell's: `SSH_AUTH_SOCK` is set in a terminal and absent in the
/// unit unless something puts it there. So the same mount works when typed by
/// hand and hangs when started by systemd, which is the least obvious way for
/// this to go wrong.
fn warn_about_ssh(urls: &[String]) {
    if !urls.iter().any(|url| url.starts_with("ssh://")) {
        return;
    }
    println!(
        "note: ssh mounts authenticate as you, using your own SSH keys and agent.\n\
         \x20     Key-based auth must work non-interactively — check with `ssh <host> true`;\n\
         \x20     a key with a passphrase and no agent will hang at startup."
    );
    #[cfg(unix)]
    println!(
        "\x20     A user unit does not inherit your shell's environment, so an agent that\n\
         \x20     works in a terminal may be invisible to it. If the mount hangs:\n\
         \x20       systemctl --user import-environment SSH_AUTH_SOCK"
    );
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
    /// it fails at boot, where nobody is looking.
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
