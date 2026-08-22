//! Registering an instance with systemd.
//!
//! **User units, not system units.** Everything here writes to
//! `~/.config/systemd/user` and talks to `systemctl --user`, so no part of
//! `alloyfs service` needs root on Linux. That is the opposite of the Windows
//! side, where every subcommand demands an elevated shell, and the reason is
//! that the two platforms put the obstacle in different places:
//!
//! - A Windows drive letter is per-session, so a mount has to be created *in*
//!   the logged-in user's session by a process running as them. Arranging that
//!   from a service is what needs LocalSystem, `SE_TCB_NAME` and a token swap.
//! - A systemd user unit already runs as the user, in their session, with their
//!   environment and their keys. There is nothing to arrange, so there is
//!   nothing to elevate for.
//!
//! A **system** unit (`/etc/systemd/system`, `User=%i`) buys exactly one thing
//! over this: it starts without anyone logging in. It costs root for every
//! operation, and it runs the process outside the user's session — no
//! `XDG_RUNTIME_DIR`, no session bus, and no `SSH_AUTH_SOCK`, which is fatal
//! for `ssh://` mounts. `docs/service.sh` writes one deliberately, because it
//! installs an agent on a headless box where nobody ever logs in and no mount
//! is involved. That is a different job from this one, and both are worth
//! having; see `docs/deployment/service.md`.
//!
//! The cost of the user unit is that it starts at **login** rather than at
//! boot, unless lingering is enabled for the account. That is said out loud by
//! `setup` and by `add` rather than papered over — see [`linger_note`].

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::instance::{service_name, Instance};

/// Where systemd looks for a user's own units.
///
/// `XDG_CONFIG_HOME` is honoured because this is systemd's search path, not
/// ours — writing to `~/.config` when the user has moved it would put the unit
/// somewhere systemd never reads.
fn unit_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => crate::config::home_dir().join(".config"),
    }
    .join("systemd")
    .join("user")
}

fn unit_name(id: &str) -> String {
    format!("{}.service", service_name(id))
}

fn unit_path(id: &str) -> PathBuf {
    unit_dir().join(unit_name(id))
}

// ------------------------------------------------------------------ the unit

/// Escape a value for a systemd unit file.
///
/// `%` introduces a specifier — `%h` is the home directory, `%i` the instance
/// name — so a literal one has to be doubled or systemd silently substitutes
/// something else. A config path containing `%` is unusual but a mount that
/// starts against the wrong file is not the kind of surprise to leave lying
/// around. Whitespace and quotes get the double-quoted form, since systemd
/// splits `ExecStart` on whitespace exactly like a shell would.
fn escape(value: &str) -> String {
    let escaped = value.replace('%', "%%");
    if escaped.is_empty() || escaped.contains([' ', '\t', '"', '\'', '\\', '\n']) {
        let inner = escaped.replace('\\', r"\\").replace('"', "\\\"");
        format!("\"{inner}\"")
    } else {
        escaped
    }
}

/// The unit an instance becomes.
///
/// Pure, and public to the module's tests, because this text *is* the feature:
/// everything else here is `systemctl` plumbing, and a wrong line in here fails
/// at boot rather than at the moment somebody is watching.
pub fn unit_text(id: &str, exe: &Path, instance: &Instance) -> String {
    let command = instance.command();
    let mut exec = escape(&exe.display().to_string());
    for arg in instance.argv() {
        exec.push(' ');
        exec.push_str(&escape(&arg));
    }

    // Hardening that would break the thing being hardened. `fusermount3` is
    // setuid root — it is how an unprivileged process is allowed to mount at
    // all — and `NoNewPrivileges=true` suppresses setuid, so a mounting unit
    // that sets it fails with a permission error nothing on screen explains.
    // Measured on systemd 259: a setuid binary run from a user unit reports
    // `euid=0` without the directive and the caller's own uid with it.
    //
    // An agent-only instance never calls fusermount, so it keeps the
    // restriction. This is the same split as the FUSE check in `add` — what an
    // instance needs follows from what it does, not from what its siblings do.
    // The comment on the mounting branch deliberately avoids naming the
    // directive: the unit for an instance that mounts must not contain it at
    // all, which is a thing a test can then check without qualification.
    let hardening = if instance.mounts_anything() {
        "# This instance mounts, so it must keep the setuid fusermount3 helper working.\n"
    } else {
        "# Nothing here mounts, so it never needs the setuid fusermount3 helper.\nNoNewPrivileges=true\n"
    };

    // Notably absent: `Wants=network-online.target`. That target does not exist
    // in the user manager — systemd loads the reference, never activates it,
    // and the unit starts anyway — so copying the line out of a system unit
    // would only look like an ordering guarantee. What actually covers a mount
    // racing the network is `Restart=`, below.
    format!(
        "# alloyfs-{id} — generated by `alloyfs service add {id}`.\n\
         #\n\
         # Rewritten on every `add`, so edits here do not survive. Nothing about\n\
         # WHAT gets mounted lives in this file: the unit names which part of the\n\
         # AlloyFS config to run, and `alloyfs` reads the rest at launch.\n\
         #\n\
         #   systemctl --user status {unit}\n\
         #   journalctl --user -u {unit} -f\n\
         \n\
         [Unit]\n\
         Description=AlloyFS {id} (alloyfs {command})\n\
         Documentation=https://alloy.okyle.dev/deployment/service\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         # on-failure, not always: `alloyfs start` already distinguishes the two\n\
         # cases in its exit code, and `always` threw that away. A config that\n\
         # describes nothing exits 0 and will never start anything however often\n\
         # it is retried, so it lands in `failed`... which is to say, it stops,\n\
         # having journalled why. A deliberate stop or a hand unmount is also a\n\
         # clean exit, and is not fought.\n\
         #\n\
         # This is now the OUTER of two safety nets. `alloyfs start` supervises\n\
         # each mount itself — a unit that dies is logged and restarted on a\n\
         # backoff ladder, without disturbing the mounts that are working — so\n\
         # the usual recovery never reaches systemd at all. What still does:\n\
         # the process dying outright, or a supervisor panicking.\n\
         #\n\
         # No StartLimit deliberately. It would stop the retry loop on an empty\n\
         # config, but at the price of capping recovery for a mount whose server\n\
         # is down for a few minutes — and that outage is exactly what `Restart`\n\
         # is here to survive.\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # alloyfs unmounts on SIGINT; systemd's default stop signal is SIGTERM,\n\
         # which it does not handle. Stopping the unit would then kill it with the\n\
         # filesystem still mounted, leaving a mountpoint whose every operation\n\
         # fails with ENOTCONN until somebody runs fusermount -u by hand.\n\
         KillSignal=SIGINT\n\
         TimeoutStopSec=30\n\
         # No sandboxing directives (PrivateTmp, ProtectHome, ProtectSystem...).\n\
         # Any one of them gives the unit a private mount namespace, and a mount\n\
         # made inside one is invisible to the shell that asked for it.\n\
         {hardening}\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        unit = unit_name(id),
    )
}

// --------------------------------------------------------------- systemctl

fn systemctl(args: &[&str]) -> anyhow::Result<Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| {
            anyhow::anyhow!(
                "cannot run systemctl ({e}).\n\
                 \n\
                 `alloyfs service` registers systemd user units. On a box that uses a \
                 different init, run `alloyfs start` under whatever supervisor it does use."
            )
        })
}

/// Run a `systemctl --user` verb and turn a non-zero exit into its stderr.
fn systemctl_ok(args: &[&str]) -> anyhow::Result<()> {
    let out = systemctl(args)?;
    anyhow::ensure!(
        out.status.success(),
        "systemctl --user {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

// ------------------------------------------------------------- registration

/// Write the unit, load it, and enable it for the next login.
pub fn create(id: &str, instance: &Instance) -> anyhow::Result<()> {
    // `current_exe` rather than a PATH lookup: systemd does not search a user's
    // PATH, so `ExecStart` has to be absolute. Resolving it to the binary that
    // is running also answers the question correctly when several are installed
    // — the unit then runs the one that registered it.
    let exe = std::env::current_exe()?;
    let dir = unit_dir();
    std::fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;
    let path = unit_path(id);
    std::fs::write(&path, unit_text(id, &exe, instance))
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;

    systemctl_ok(&["daemon-reload"])?;
    systemctl_ok(&["enable", &unit_name(id)])?;
    Ok(())
}

pub fn delete(id: &str) -> anyhow::Result<()> {
    let unit = unit_name(id);
    // Best-effort: an instance whose unit was already removed by hand still has
    // to be forgettable, or the definition is stranded.
    let _ = systemctl(&["disable", "--now", &unit]);
    let path = unit_path(id);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| anyhow::anyhow!("removing {}: {e}", path.display()))?;
    }
    systemctl_ok(&["daemon-reload"])?;
    // Without this a unit that was failing when it was removed keeps its failed
    // state in `systemctl --user list-units`, outliving the thing it describes.
    let _ = systemctl(&["reset-failed", &unit]);
    Ok(())
}

pub fn start(id: &str) -> anyhow::Result<()> {
    systemctl_ok(&["start", &unit_name(id)])
}

/// Stop it and wait.
///
/// No polling loop, unlike the Windows side: `systemctl stop` blocks until the
/// job finishes, so `restart` cannot race its own stop.
pub fn stop(id: &str) -> anyhow::Result<()> {
    systemctl_ok(&["stop", &unit_name(id)])
}

/// A word for `service list`. Never fails: an unregistered or unreadable unit
/// is information, not an error.
/// Why a unit is in the state it is in, when systemd has anything to say.
///
/// `state` answers one word, which is all a table column should hold — and
/// for a unit whose child keeps dying that word is "running", because the
/// supervisor is up and the CHILD is failing. systemd already tracks the
/// rest and answers it in the same `show` call: how it last ended, what
/// the main process exited with, and how many times it has been restarted.
/// All of it was being asked for and discarded.
///
/// `None` when there is nothing worth saying, so a healthy row stays
/// unannotated.
pub fn detail(id: &str) -> Option<String> {
    let out = systemctl(&[
        "show",
        &unit_name(id),
        "--property=Result",
        "--property=ExecMainStatus",
        "--property=NRestarts",
        "--property=StatusText",
    ])
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let mut parts = Vec::new();
    // "success" is the resting value; anything else names how it ended.
    match field("Result=").as_str() {
        "" | "success" => {}
        other => parts.push(format!("result: {other}")),
    }
    match field("ExecMainStatus=").as_str() {
        "" | "0" => {}
        code => parts.push(format!("last exit: {code}")),
    }
    match field("NRestarts=").as_str() {
        "" | "0" => {}
        n => parts.push(format!("{n} restart(s)")),
    }
    let status_text = field("StatusText=");
    if !status_text.is_empty() {
        parts.push(status_text);
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub fn state(id: &str) -> String {
    let Ok(out) = systemctl(&[
        "show",
        &unit_name(id),
        "--property=LoadState",
        "--property=ActiveState",
    ]) else {
        return "?".into();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_default()
            .to_string()
    };
    // A definition can outlive its unit — the same case `unregistered` covers on
    // Windows — and `show` answers for a unit that does not exist rather than
    // failing, so LoadState is the only thing that distinguishes the two.
    if field("LoadState=") != "loaded" {
        return "unregistered".into();
    }
    match field("ActiveState=").as_str() {
        "active" => "running".into(),
        "activating" => "starting".into(),
        "deactivating" => "stopping".into(),
        "failed" => "failed".into(),
        "inactive" => "stopped".into(),
        // No ActiveState at all means systemctl answered with something this
        // does not understand, which is not the same as a state it does.
        "" => "?".into(),
        // Anything systemd grows later shows through under its own name rather
        // than being flattened into "?".
        other => other.to_string(),
    }
}

// ------------------------------------------------------------------- checks

/// Refuse to run as somebody else.
///
/// The mirror image of the Windows elevation check, and for the mirror-image
/// reason. `sudo alloyfs service add` would write into **root's**
/// `~/.config/systemd/user` and talk to root's user manager, registering a
/// mount for an account that never logs in — and doing it quietly, because
/// every individual step succeeds.
///
/// Only `sudo` is refused, not root itself: a box administered as root has a
/// legitimate reason to register root's own units, and there is no second
/// account to point at.
pub fn preflight() -> anyhow::Result<()> {
    if let Ok(user) = std::env::var("SUDO_USER") {
        anyhow::bail!(
            "run this as {user}, without sudo.\n\
             \n\
             `alloyfs service` registers a systemd USER unit: it needs no root, and under \
             sudo it would register one for root instead of for {user} — a mount belonging \
             to an account that never logs in.\n\
             \n\
             The installer at https://alloy.okyle.dev/service.sh is the one that wants sudo. \
             It writes a system-wide unit for an agent on a machine with no interactive user."
        );
    }
    Ok(())
}

/// Is there a systemd user manager to talk to at all?
pub fn verify_supervisor() -> anyhow::Result<()> {
    let out = systemctl(&["is-system-running"])?;
    // `degraded` is a non-zero exit and a perfectly usable manager — some other
    // unit of the user's has failed, which is not this command's business. Only
    // a bus that cannot be reached matters here.
    let stderr = String::from_utf8_lossy(&out.stderr);
    anyhow::ensure!(
        !stderr.contains("Failed to connect"),
        "no systemd user manager for this session ({}).\n\
         \n\
         This usually means the login was not a real session — `sudo -u`, `su`, or a \
         container without one — so XDG_RUNTIME_DIR is unset and there is no user bus. \
         Log in as the account that will run the mount, or start its manager with \
         `loginctl enable-linger <user>`.",
        stderr.trim()
    );
    Ok(())
}

/// Refuse early when FUSE is not available.
///
/// The counterpart of the WinFsp check: without it a mount fails at boot, in a
/// unit nobody is watching, and the only trace is a journal entry.
pub fn verify_backend() -> anyhow::Result<()> {
    anyhow::ensure!(
        Path::new("/dev/fuse").exists(),
        "/dev/fuse does not exist — a FUSE mount cannot work without it.\n\
         Load the module with `sudo modprobe fuse`, or install the AlloyFS kernel \
         module and mount with `--backend kernel`."
    );
    let helper = [
        "/usr/bin/fusermount3",
        "/bin/fusermount3",
        "/usr/bin/fusermount",
        "/bin/fusermount",
    ]
    .iter()
    .any(|p| Path::new(p).exists());
    anyhow::ensure!(
        helper,
        "no fusermount3 helper found — an unprivileged mount needs it.\n\
         Install it with `sudo apt install fuse3` (or the equivalent for this distro)."
    );
    Ok(())
}

/// The instance store holds pointers to what this account's units run, so it is
/// the account's own business and nobody else's.
///
/// `0700` rather than an ACL: there is no Windows-style question of which
/// privileged group may write here, because nothing privileged reads it.
pub fn prepare_store(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| anyhow::anyhow!("restricting {}: {e}", dir.display()))
}

/// What to say about lingering, if anything.
///
/// This is the one place a user unit behaves differently from the Windows
/// service it mirrors: without lingering the user manager starts at login and
/// stops at logout, so `enable` means "at next login", not "at boot". On a
/// laptop that is usually what was wanted. On a headless box reached over SSH
/// it is not, and the failure is silent — the drive is simply not there.
pub fn linger_note() -> Option<String> {
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).ok()?;
    if linger_enabled(&user) {
        return None;
    }
    Some(format!(
        "note: lingering is off for {user}, so units start at LOGIN, not at boot,\n\
         \x20     and stop at logout. For a machine that should mount before anyone\n\
         \x20     logs in — a server, or anything reached over SSH:\n\
         \x20       sudo loginctl enable-linger {user}"
    ))
}

fn linger_enabled(user: &str) -> bool {
    // The marker file is systemd's own storage for this, and reading it needs
    // neither a bus nor loginctl — which matters because the case worth
    // reporting is exactly the one where the session machinery is thin.
    if Path::new("/var/lib/systemd/linger").join(user).exists() {
        return true;
    }
    Command::new("loginctl")
        .args(["show-user", user, "--property=Linger", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::service::Scope;

    fn text(instance: &Instance) -> String {
        unit_text("work", Path::new("/usr/local/bin/alloyfs"), instance)
    }

    /// The unit has to launch the same command `service list` prints, spelled
    /// absolutely — systemd searches no PATH, so a bare `alloyfs` here is a
    /// unit that fails at boot.
    #[test]
    fn the_unit_runs_the_command_the_instance_names() {
        let unit = text(&Instance::Mount {
            name: "work".into(),
            config: None,
        });
        assert!(
            unit.contains("ExecStart=/usr/local/bin/alloyfs mount work\n"),
            "{unit}"
        );
        assert!(unit.contains("WantedBy=default.target"), "{unit}");

        let unit = text(&Instance::Start {
            config: Some(PathBuf::from("/etc/alloyfs.yml")),
            scope: Scope::Mounts,
        });
        assert!(
            unit.contains("ExecStart=/usr/local/bin/alloyfs start --mounts-only --config /etc/alloyfs.yml\n"),
            "{unit}"
        );
    }

    /// systemd expands `%h`, `%i` and friends inside a unit file, so a literal
    /// percent has to be doubled or the service starts against a path nobody
    /// wrote. A space needs the quoted form for the same class of reason.
    #[test]
    fn a_path_systemd_would_rewrite_is_escaped() {
        let unit = text(&Instance::Start {
            config: Some(PathBuf::from("/home/a b/100%/alloyfs.yml")),
            scope: Scope::All,
        });
        assert!(
            unit.contains(r#"ExecStart=/usr/local/bin/alloyfs start --config "/home/a b/100%%/alloyfs.yml""#),
            "{unit}"
        );
    }

    /// `NoNewPrivileges` suppresses setuid, and `fusermount3` is setuid root —
    /// so the directive is exactly wrong for anything that mounts, and right
    /// for anything that does not.
    #[test]
    fn only_a_non_mounting_instance_forbids_new_privileges() {
        let mounting = text(&Instance::Mount {
            name: "work".into(),
            config: None,
        });
        assert!(!mounting.contains("NoNewPrivileges"), "{mounting}");

        let agent = text(&Instance::Start {
            config: None,
            scope: Scope::Server,
        });
        assert!(agent.contains("NoNewPrivileges=true"), "{agent}");
    }

    /// A stop that does not run the unmount path leaves a mountpoint that
    /// answers ENOTCONN to everything, so this line is load-bearing rather
    /// than tidy.
    #[test]
    fn stopping_the_unit_sends_the_signal_alloyfs_unmounts_on() {
        let unit = text(&Instance::Start {
            config: None,
            scope: Scope::All,
        });
        assert!(unit.contains("KillSignal=SIGINT"), "{unit}");
    }

    /// Copying `Wants=network-online.target` out of a system unit produces a
    /// line that reads as an ordering guarantee and is not one: the target does
    /// not exist in the user manager.
    #[test]
    fn the_unit_claims_no_ordering_the_user_manager_cannot_provide() {
        let unit = text(&Instance::Start {
            config: None,
            scope: Scope::All,
        });
        assert!(!unit.contains("Wants=network-online.target"), "{unit}");
        assert!(unit.contains("Restart=on-failure"), "{unit}");
    }
}
