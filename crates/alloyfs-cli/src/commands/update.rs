//! `alloyfs update` — re-run the official installer.
//!
//! Deliberately a wrapper rather than a self-updater. Downloading, verifying
//! and placing a binary on PATH is fiddly in ways that differ per platform: a
//! running executable cannot be overwritten on Windows, PATH lives in the
//! registry there and in a shell profile here, and the download has to be
//! checked for being an HTML error page. All of that already exists, once, in
//! `docs/install.sh` and `docs/install.ps1`.
//!
//! Reimplementing it in Rust would mean two copies that drift, plus a TLS
//! stack linked into a filesystem binary for the sake of one rarely-used
//! command. Handing off keeps one implementation and one place to fix.

use std::process::Command;

const BASE: &str = "https://alloy.okyle.dev";

/// What to install. A channel is just a tag prefix, so this stays honest about
/// the fact that there is exactly one release stream today.
pub fn run(channel: Option<String>, dry_run: bool) -> anyhow::Result<()> {
    let version = match channel.as_deref() {
        None | Some("stable") | Some("latest") => None,
        // A literal tag: `alloyfs update v0.1.1` pins, which is what you want
        // when rolling back.
        Some(v) if v.starts_with('v') => Some(v.to_string()),
        Some(other) => anyhow::bail!(
            "unknown channel {other:?}. Use `stable` (the default), or a tag \
             like `v0.1.1` to install a specific release."
        ),
    };

    println!("current: {}", env!("CARGO_PKG_VERSION"));
    match &version {
        Some(v) => println!("target:  {v}"),
        None => println!("target:  the latest release"),
    }

    let (program, args) = installer_command(version.as_deref());

    if dry_run {
        println!("\nwould run:\n  {program} {}", args.join(" "));
        return Ok(());
    }

    println!("\nrunning the installer from {BASE}\n");
    let status = Command::new(&program)
        .args(&args)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run {program}: {e}"))?;

    if !status.success() {
        anyhow::bail!(
            "the installer failed. Run it by hand to see why:\n  {program} {}",
            args.join(" ")
        );
    }
    Ok(())
}

/// The platform's own downloader piped into its own shell — the same one-liner
/// the docs tell people to paste, so there is only one path to keep working.
#[cfg(windows)]
fn installer_command(version: Option<&str>) -> (String, Vec<String>) {
    let set = version
        .map(|v| format!("$env:ALLOYFS_VERSION='{v}'; "))
        .unwrap_or_default();
    (
        "powershell".into(),
        vec![
            "-NoProfile".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-Command".into(),
            format!("{set}irm {BASE}/install.ps1 | iex"),
        ],
    )
}

#[cfg(unix)]
fn installer_command(version: Option<&str>) -> (String, Vec<String>) {
    let set = version
        .map(|v| format!("ALLOYFS_VERSION={v} "))
        .unwrap_or_default();
    (
        "sh".into(),
        vec![
            "-c".into(),
            format!("{set}curl -fsSL {BASE}/install.sh | {set}sh"),
        ],
    )
}
