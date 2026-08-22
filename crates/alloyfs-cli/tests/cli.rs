//! Integration tests that RUN the binary.
//!
//! The crate had none, and the gap had a shape: every `#[cfg(test)]` block
//! inside it tests a pure helper, so the parts people actually meet — the
//! help text, the error messages, what a command writes to disk — were
//! untested by construction. That is how `init` came to write a config the
//! next command rewrote, and how a table in the docs came to list a
//! command that does not exist.
//!
//! No new dependency: cargo hands an integration test the path to the
//! binary it built, and the standard library can run it.
//!
//! Every test runs with HOME and USERPROFILE pointed at a fresh temporary
//! directory. That is not tidiness — config discovery walks the real
//! per-user location, so without it these would read (and could write)
//! whatever is on the machine running them, and the results would depend
//! on whose laptop it was.

use std::path::Path;
use std::process::{Command, Output};

fn alloyfs(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_alloyfs"))
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        // Quiet: these assert on stdout, and tracing writes to stderr.
        .env("RUST_LOG", "error")
        .output()
        .expect("the binary runs")
}

fn out(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn tmp() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("tempdir")
}

/// The commands the docs promise are the commands that exist.
///
/// The CLI reference drifted far enough to list a `cache` subcommand that
/// shows cache contents (it only clears) and a top-level `clear` (there is
/// none), while omitting `tree` and `bulk` entirely. A help-text check is
/// what makes that drift visible.
#[test]
fn help_lists_every_command() {
    let dir = tmp();
    let o = alloyfs(dir.path(), &["--help"]);
    assert!(o.status.success(), "{}", out(&o));
    let text = out(&o);
    for expected in [
        "serve", "mount", "start", "service", "sync", "cache", "events", "ping", "bench", "stress", "tree",
        "bulk", "init", "update", "logs", "doctor", "config",
    ] {
        assert!(
            text.contains(expected),
            "`{expected}` missing from --help:\n{text}"
        );
    }
}

/// An unreachable server names the address and the first thing to check.
///
/// This used to be a bare `os error 10061` with no host, no port and no
/// hint — the most-hit failure in the tool wearing its least useful
/// message.
#[test]
fn an_unreachable_server_says_where_and_what_to_check() {
    let dir = tmp();
    // Port 1 is reserved and never listening; no timeout risk.
    let o = alloyfs(dir.path(), &["ping", "tcp://127.0.0.1:1/nothing"]);
    assert!(!o.status.success(), "connecting to port 1 must fail");
    let text = out(&o);
    assert!(text.contains("127.0.0.1:1"), "the address must appear:\n{text}");
    assert!(
        text.contains("alloyfs serve"),
        "it should say what to check:\n{text}"
    );
}

/// Config discovery does not CREATE a config, and says so usefully.
///
/// It used to write a starter file as a side effect of looking for one,
/// which under an elevated `service add` landed in the Administrator's
/// profile. The check that it creates nothing belongs here rather than in
/// a unit test, because the behaviour is about the real filesystem.
#[test]
fn a_missing_config_creates_nothing_and_points_at_init() {
    let dir = tmp();
    let o = alloyfs(dir.path(), &["config", "validate"]);
    assert!(!o.status.success());
    let text = out(&o);
    assert!(text.contains("alloyfs init"), "{text}");

    let created: Vec<_> = walk(dir.path());
    assert!(
        created.is_empty(),
        "looking for a config must not write one, found: {created:?}"
    );
}

/// `init` writes a config that the loader accepts unchanged.
///
/// The regression this guards is subtle from the outside and obvious from
/// the inside: init wrote the pre-v3 layout, so the next command upgraded
/// the file in place, left a .bak, and discarded the comments init had
/// just written. End to end is the only place that shows up.
#[test]
fn init_writes_a_config_that_survives_being_loaded() {
    let dir = tmp();
    let o = alloyfs(dir.path(), &["init"]);
    assert!(o.status.success(), "{}", out(&o));
    let config = dir.path().join("alloyfs.yml");
    let written = std::fs::read_to_string(&config).expect("init wrote a config");

    let o = alloyfs(dir.path(), &["config", "validate"]);
    assert!(o.status.success(), "{}", out(&o));
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        written,
        "loading the config rewrote it — init is producing a shape the loader upgrades"
    );
    assert!(
        !config.with_extension("yml.bak").exists(),
        "an upgrade happened: the .bak is the evidence"
    );
}

/// Asking for a log that does not exist says what does.
#[test]
fn an_unknown_log_name_is_told_what_exists() {
    let dir = tmp();
    let o = alloyfs(dir.path(), &["logs", "nosuchlog"]);
    assert!(!o.status.success());
    let text = out(&o);
    assert!(text.contains("nosuchlog"), "{text}");
    assert!(
        text.contains("serve") || text.contains("Nothing has been logged"),
        "it should say where logs come from:\n{text}"
    );
}

/// A rollback with nothing to restore explains itself rather than failing
/// blankly — the moment someone reaches for it is the moment they can
/// least afford a bare error.
#[test]
fn a_rollback_with_no_saved_binary_says_why() {
    let dir = tmp();
    let o = alloyfs(dir.path(), &["update", "--rollback"]);
    assert!(!o.status.success());
    let text = out(&o);
    assert!(text.contains("nothing to roll back to"), "{text}");
    assert!(text.contains("alloyfs update"), "{text}");
}

/// `doctor` reports a table and names the driver, whatever the verdict.
#[test]
fn doctor_reports_the_checks_it_ran() {
    let dir = tmp();
    let o = alloyfs(dir.path(), &["doctor"]);
    let text = out(&o);
    for expected in ["version", "filesystem driver", "config", "logs"] {
        assert!(text.contains(expected), "`{expected}` missing:\n{text}");
    }
}

/// Completions generate for every shell, and know the current commands.
///
/// They come from the same clap definition the binary parses with, so this
/// is really checking that the generator still runs and that nothing has
/// made the command tree unrepresentable — a hand-written script would
/// need a second list kept in step instead.
#[test]
fn completions_generate_for_every_shell() {
    let dir = tmp();
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let o = alloyfs(dir.path(), &["completions", shell]);
        assert!(o.status.success(), "{shell}: {}", out(&o));
        let script = String::from_utf8_lossy(&o.stdout);
        assert!(script.len() > 200, "{shell} produced almost nothing:\n{script}");
        // A command added this week, to catch a generator wired to a stale
        // definition rather than the live one.
        assert!(script.contains("doctor"), "{shell} does not know `doctor`");
    }
}

/// Every file under `root`, so a test can assert that nothing was created.
fn walk(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path.strip_prefix(root).unwrap_or(&path).display().to_string());
            }
        }
    }
    found
}
