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
const REPO: &str = "obillekyle/alloyfs";

/// The previous binary, kept beside the current one so a rollback needs no
/// network. The Windows installer keeps its own `.old` — which is the
/// binary a RUNNING service is still executing from, and is deleted at the
/// start of the next install — so this is deliberately a separate copy
/// under our own control, and it exists on both platforms.
fn previous_path(exe: &std::path::Path) -> std::path::PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".prev");
    exe.with_file_name(name)
}

/// Semver precedence, enough for the shapes this project ships: `X.Y.Z`
/// and `X.Y.Z-alpha.N`.
///
/// Needed because "the tags differ" is NOT "there is an update", and the
/// difference is not academic here: GitHub's `releases/latest` deliberately
/// skips prereleases, so on a machine running an alpha it answers with the
/// last STABLE — which is older. Comparing strings reported that as an
/// available update and would have talked people into downgrading.
///
/// Precedence follows the spec: numeric fields first, then a release
/// outranks any prerelease of the same numbers, then prerelease
/// identifiers compare numerically when both are numeric and lexically
/// otherwise.
fn is_newer(candidate: &str, current: &str) -> bool {
    fn parse(v: &str) -> (Vec<u64>, Option<String>) {
        let v = v.trim().trim_start_matches('v');
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, Some(pre.to_string())),
            None => (v, None),
        };
        let nums = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        (nums, pre)
    }
    let (a_num, a_pre) = parse(candidate);
    let (b_num, b_pre) = parse(current);
    if a_num != b_num {
        return a_num > b_num;
    }
    match (a_pre, b_pre) {
        (None, None) => false,
        (None, Some(_)) => true,  // a release beats its own prereleases
        (Some(_), None) => false, // ...and a prerelease never beats the release
        (Some(a), Some(b)) => {
            for (x, y) in a.split('.').zip(b.split('.')) {
                if x == y {
                    continue;
                }
                return match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x > y,
                    _ => x > y,
                };
            }
            a.split('.').count() > b.split('.').count()
        }
    }
}

/// Ask the release API what the newest tag is, through the platform's own
/// downloader.
///
/// Shelling out rather than linking a TLS stack is the same choice the rest
/// of this module makes, and for the same reason: one HTTP implementation
/// per platform, already present, instead of a second one compiled into a
/// filesystem binary for the sake of a rarely-used command. The URL is the
/// one both installers already use.
fn fetch_tag(path: &str) -> anyhow::Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/{path}");
    #[cfg(windows)]
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "(Invoke-WebRequest -Uri '{url}' -Headers @{{'User-Agent'='alloyfs'}} \
                 -UseBasicParsing).Content"
            ),
        ])
        .output();
    #[cfg(unix)]
    let out = Command::new("curl")
        .args(["-fsSL", "-H", "User-Agent: alloyfs", &url])
        .output();

    let out = out.map_err(|e| anyhow::anyhow!("could not reach the release API: {e}"))?;
    anyhow::ensure!(
        out.status.success(),
        "the release API request failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let body: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow::anyhow!("the release API answered something that is not JSON: {e}"))?;
    // `releases/latest` answers an object; `releases?per_page=1` answers a
    // one-element array. Take the tag from either shape.
    let obj = match &body {
        serde_json::Value::Array(items) => items
            .first()
            .ok_or_else(|| anyhow::anyhow!("the release API answered an empty list"))?,
        other => other,
    };
    obj.get("tag_name")
        .and_then(|t| t.as_str())
        .map(|t| t.to_string())
        .ok_or_else(|| anyhow::anyhow!("the release API answered no tag_name"))
}

/// `--check`: say whether a newer release exists, and answer in the exit
/// code so a script or a cron job can act on it without parsing text.
///
/// 0 means up to date, 1 means an update is available. Both are ordinary
/// outcomes — neither is an error, which is why this prints and exits
/// rather than returning `Err` and having anyhow decorate a normal answer
/// as a failure.
pub fn check() -> anyhow::Result<()> {
    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    // Both, because they answer different questions and can disagree by a
    // lot: `releases/latest` is the newest STABLE (prereleases excluded by
    // GitHub), while the first page of `releases` is the newest thing
    // published at all. On a machine running an alpha the stable one is
    // usually OLDER, which is exactly the trap this reports its way out of.
    let stable = fetch_tag("releases/latest")?;
    let newest = fetch_tag("releases?per_page=1")?;

    println!("current: {current}");
    println!("stable:  {stable}");
    if newest != stable {
        println!("newest:  {newest}  (prerelease)");
    }

    let target = if is_newer(&newest, &stable) {
        &newest
    } else {
        &stable
    };
    if is_newer(target, &current) {
        println!("\nan update is available: alloyfs update {target}");
        std::process::exit(1);
    }
    if is_newer(&current, target) {
        println!("\nthis build is newer than anything published.");
    } else {
        println!("\nup to date.");
    }
    Ok(())
}

/// `--rollback`: put back the binary this machine was running before the
/// last update, with no network involved.
///
/// The point is the case where the network is exactly what you cannot use:
/// a release that mounts nothing, on a machine whose drive is the thing
/// that broke. Rolling back by re-downloading needs a working connection
/// AND for you to remember the previous tag.
pub fn rollback() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let prev = previous_path(&exe);
    anyhow::ensure!(
        prev.exists(),
        "nothing to roll back to: {} does not exist.\n\n  \
         A copy is kept there by `alloyfs update`, so there is one only after \
         an update has run on this machine.\n  \
         To go back to a specific release instead: alloyfs update <tag>",
        prev.display()
    );

    // Rename the live binary aside rather than deleting it: Windows refuses
    // to delete a running image but permits renaming one, and this process
    // IS that image. The same move works on unix, where the rename is on
    // the directory entry and the running inode is untouched.
    let aside = exe.with_extension("rolled-back");
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(&exe, &aside)
        .map_err(|e| anyhow::anyhow!("could not move the current binary aside: {e}"))?;
    if let Err(e) = std::fs::rename(&prev, &exe) {
        // Put it back rather than leaving the machine with no alloyfs at all.
        let _ = std::fs::rename(&aside, &exe);
        anyhow::bail!("could not restore {}: {e}", prev.display());
    }

    let restored = Command::new(&exe)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "(unknown version)".to_string());
    println!("rolled back to {restored}");
    println!("  was:  {}", env!("CARGO_PKG_VERSION"));
    println!("  kept: {}", aside.display());
    println!("\nRestart anything still running the previous binary:");
    println!("  alloyfs service restart <id>");
    Ok(())
}

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

    // The installers honor ALLOYFS_INSTALL; without it they place the binary
    // at their platform default — which is NOT necessarily where THIS binary
    // lives. An update that lands beside a stale copy still winning on PATH
    // is worse than no update (a sudo run on the azure box put the new
    // binary in root's ~/.local/bin exactly this way). The running binary's
    // own directory is the one destination that is right by definition.
    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf));

    println!("current: {}", env!("CARGO_PKG_VERSION"));
    match &version {
        Some(v) => println!("target:  {v}"),
        None => println!("target:  the latest release"),
    }
    if let Some(dir) = &install_dir {
        println!("into:    {}", dir.display());
    }

    let (program, args) = installer_command(version.as_deref());

    if dry_run {
        let env_note = install_dir
            .as_ref()
            .map(|d| format!("ALLOYFS_INSTALL={} ", d.display()))
            .unwrap_or_default();
        println!("\nwould run:\n  {env_note}{program} {}", args.join(" "));
        return Ok(());
    }

    // Keep a copy of what is being replaced, so `--rollback` needs no
    // network. Best-effort: a machine that cannot write beside its own
    // binary should still be able to update, it just cannot roll back
    // offline afterwards — and saying so is better than either failing the
    // update or silently promising a rollback that will not be there.
    if let Ok(exe) = std::env::current_exe() {
        let prev = previous_path(&exe);
        match std::fs::copy(&exe, &prev) {
            Ok(_) => println!("keeping the current binary at {}", prev.display()),
            Err(e) => tracing::warn!(error = %e, path = %prev.display(),
                "could not keep a rollback copy; `alloyfs update --rollback` will have nothing to restore"),
        }
    }

    println!("\nrunning the installer from {BASE}\n");
    let mut cmd = Command::new(&program);
    cmd.args(&args);
    if let Some(dir) = &install_dir {
        // Through the environment, not the command string: the child shells
        // inherit it, and a path never has to survive quoting.
        cmd.env("ALLOYFS_INSTALL", dir);
    }
    let status = cmd
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The comparison that decides whether to tell someone to update.
    ///
    /// The case that motivated it: `releases/latest` skips prereleases, so
    /// a machine on 1.0.0-alpha.62 is told the latest is 0.7.0. String
    /// inequality read that as "an update is available" and would have
    /// talked people into downgrading.
    #[test]
    fn version_precedence_does_not_recommend_a_downgrade() {
        assert!(!is_newer("v0.7.0", "v1.0.0-alpha.62"), "the exact live case");
        assert!(is_newer("v1.0.0-alpha.62", "v0.7.0"));

        // Numeric fields first.
        assert!(is_newer("v1.2.0", "v1.1.9"));
        assert!(is_newer("v2.0.0", "v1.99.99"));
        assert!(!is_newer("v1.0.0", "v1.0.0"));

        // A release outranks its own prereleases, in both directions.
        assert!(is_newer("v1.0.0", "v1.0.0-alpha.1"));
        assert!(!is_newer("v1.0.0-alpha.1", "v1.0.0"));

        // Prerelease identifiers compare numerically, not as text — the
        // trap that makes alpha.9 look newer than alpha.62.
        assert!(is_newer("v1.0.0-alpha.62", "v1.0.0-alpha.9"));
        assert!(!is_newer("v1.0.0-alpha.9", "v1.0.0-alpha.62"));
        assert!(is_newer("v1.0.0-beta.1", "v1.0.0-alpha.99"));

        // A `v` prefix is optional on either side.
        assert!(is_newer("1.0.1", "v1.0.0"));
    }
}
