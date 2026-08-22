//! `alloyfs doctor` — check the things that stop a drive from working,
//! before anyone has to guess which one it was.
//!
//! Every probe here already existed. What did not exist was a way to RUN
//! them: the filesystem-driver check only fired from `service add`, the
//! supervisor check only from `service setup`, and the rest were error
//! paths nobody reaches deliberately. Someone whose drive is missing had
//! to know which command happened to test which thing.
//!
//! Local checks only, and fast. Whether a server is reachable is a real
//! question and it belongs to `alloyfs ping <url>`, which already answers
//! it with a round-trip time — running network probes here would make the
//! command slow and hang on exactly the machines that need it most.

use std::fmt::Write as _;

/// One check's outcome. `Warn` is for a real observation that is not
/// necessarily wrong — no config yet on a fresh install, no services
/// registered on a machine that mounts by hand.
enum Verdict {
    Ok(String),
    Warn(String),
    Fail(String),
}

impl Verdict {
    fn mark(&self) -> &'static str {
        match self {
            Verdict::Ok(_) => "ok  ",
            Verdict::Warn(_) => "warn",
            Verdict::Fail(_) => "FAIL",
        }
    }
    fn note(&self) -> &str {
        match self {
            Verdict::Ok(s) | Verdict::Warn(s) | Verdict::Fail(s) => s,
        }
    }
}

/// Flatten an `anyhow` chain onto one line: a check's note is a row in a
/// table, and a multi-line error would break the shape. The full text is
/// still there, just joined.
fn one_line(e: &anyhow::Error) -> String {
    let mut out = String::new();
    for (i, cause) in e.chain().enumerate() {
        let text = cause.to_string();
        let text = text.trim().replace('\n', " ");
        if i > 0 {
            let _ = write!(out, ": ");
        }
        let _ = write!(out, "{text}");
    }
    out
}

fn from_result(r: anyhow::Result<()>, ok: impl Into<String>) -> Verdict {
    match r {
        Ok(()) => Verdict::Ok(ok.into()),
        Err(e) => Verdict::Fail(one_line(&e)),
    }
}

pub fn run() -> anyhow::Result<()> {
    let mut checks: Vec<(&str, Verdict)> = Vec::new();

    checks.push((
        "version",
        Verdict::Ok(format!(
            "{} (wire protocol {})",
            env!("CARGO_PKG_VERSION"),
            alloyfs_proto::PROTO_RANGE
        )),
    ));

    // The filesystem driver: WinFsp or FUSE. The single most common reason
    // a mount cannot start, and the check that already knew how to say so
    // with an install URL.
    checks.push((
        "filesystem driver",
        from_result(
            super::service::verify_backend(),
            if cfg!(windows) {
                "WinFsp present"
            } else {
                "FUSE present"
            },
        ),
    ));

    // The service supervisor: the SCM on Windows, the systemd USER bus on
    // Linux. A machine can mount by hand without it, so this is worth
    // reporting either way rather than being fatal.
    checks.push((
        "service supervisor",
        match super::service::verify_supervisor_check() {
            Ok(()) => Verdict::Ok(if cfg!(windows) {
                "service control manager reachable".into()
            } else {
                "systemd user bus reachable".into()
            }),
            Err(e) => Verdict::Warn(one_line(&e)),
        },
    ));

    // Config: which file is in force. `load_with_path` runs the same
    // discovery every other command does, so what this reports is what
    // they will actually read — including the parse, since a config that
    // does not load is the failure people hit after editing one.
    checks.push(("config", config_check()));

    // Where the long-running commands write, and whether anything is
    // there — the first question after "my drive vanished".
    checks.push(("logs", logs_check()));

    // Registered instances, with whatever state the platform reports.
    checks.push(("services", services_check()));

    // Linux only in practice: without linger a user unit does not start
    // until the user logs in, which is the classic "it works until I
    // reboot" report.
    if let Some(note) = super::service::linger_note_check() {
        checks.push(("boot start", Verdict::Warn(note.replace('\n', " "))));
    }

    let width = checks.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, verdict) in &checks {
        println!("{}  {name:<width$}  {}", verdict.mark(), verdict.note());
    }

    let failures = checks
        .iter()
        .filter(|(_, v)| matches!(v, Verdict::Fail(_)))
        .count();
    println!();
    if failures == 0 {
        println!("nothing broken here.");
        println!("  For a server that will not answer: alloyfs ping <url>");
        Ok(())
    } else {
        println!("{failures} check(s) failed.");
        // A non-zero exit so this is usable in a script, without anyhow
        // decorating an ordinary reported outcome as an error.
        std::process::exit(1);
    }
}

fn config_check() -> Verdict {
    match crate::config::load_with_path(None) {
        Ok((Some(path), cfg)) => {
            let exports = cfg
                .server
                .as_ref()
                .and_then(|s| s.exports.as_ref())
                .map(|e| e.len())
                .unwrap_or(0);
            let mounts = cfg
                .client
                .as_ref()
                .map(|c| c.resolved_mounts().len())
                .unwrap_or(0);
            Verdict::Ok(format!(
                "{} ({exports} export(s), {mounts} mount(s))",
                path.display()
            ))
        }
        Ok((None, _)) => {
            Verdict::Warn("no config found; `alloyfs init` writes one, or pass --config".to_string())
        }
        Err(e) => Verdict::Fail(one_line(&e)),
    }
}

fn logs_check() -> Verdict {
    let dir = crate::logfile::dir();
    let count = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
                .count()
        })
        .unwrap_or(0);
    if count == 0 {
        Verdict::Warn(format!(
            "{} (empty — a log appears once serve/mount/start runs)",
            dir.display()
        ))
    } else {
        Verdict::Ok(format!("{} ({count} file(s)) — alloyfs logs", dir.display()))
    }
}

fn services_check() -> Verdict {
    match super::service::instance_summary() {
        Ok(summary) if summary.is_empty() => {
            Verdict::Warn("none registered (mounting by hand is fine)".to_string())
        }
        Ok(summary) => Verdict::Ok(summary),
        Err(e) => Verdict::Warn(one_line(&e)),
    }
}
