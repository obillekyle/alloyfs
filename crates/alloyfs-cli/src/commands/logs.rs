//! `alloyfs logs` — read what the long-running commands wrote.
//!
//! The counterpart to `logfile`: `serve`, `mount`, `start` and a Windows
//! service instance all tee their tracing output to
//! `~/.alloyfs/logs/<name>.log`, and this reads it back without asking
//! anyone to know that path.

use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

/// How often `--follow` looks for new bytes. Fast enough to feel live,
/// slow enough that following a quiet log is free.
const POLL: Duration = Duration::from_millis(300);

pub async fn run(name: Option<String>, follow: bool, lines: usize) -> anyhow::Result<()> {
    let Some(name) = name else {
        return list();
    };
    let path = crate::logfile::path_for(&name);
    if !path.exists() {
        let available = names();
        anyhow::bail!(
            "no log named {name:?} in {}.{}",
            crate::logfile::dir().display(),
            if available.is_empty() {
                "\n\nNothing has been logged yet — a log appears once `alloyfs serve`, \
                 `mount` or `start` runs."
                    .to_string()
            } else {
                format!("\n\nThere is: {}", available.join(", "))
            }
        );
    }

    let mut pos = print_tail(&path, lines)?;
    if !follow {
        return Ok(());
    }
    // journalctl also has the systemd instance's stdout, but the file is
    // the one place BOTH platforms keep it, so follow that and say so.
    eprintln!("--- following {} (Ctrl-C to stop) ---", path.display());
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!();
                return Ok(());
            }
            _ = tokio::time::sleep(POLL) => {}
        }
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len < pos {
            // Rotated (or truncated) under us: the bytes we were waiting
            // for are in `.log.1` now, and the live file starts again.
            eprintln!("--- rotated ---");
            pos = 0;
        }
        if len > pos {
            let mut file = std::fs::File::open(&path)?;
            file.seek(SeekFrom::Start(pos))?;
            let mut fresh = Vec::new();
            file.read_to_end(&mut fresh)?;
            pos += fresh.len() as u64;
            let mut out = std::io::stdout().lock();
            out.write_all(&fresh)?;
            out.flush()?;
        }
    }
}

/// Print the last `lines` lines; return the byte offset we have consumed.
fn print_tail(path: &std::path::Path, lines: usize) -> anyhow::Result<u64> {
    let body = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&body);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    let mut out = std::io::stdout().lock();
    for line in &all[start..] {
        writeln!(out, "{line}")?;
    }
    out.flush()?;
    Ok(body.len() as u64)
}

/// Log names present on this machine, newest activity first.
fn names() -> Vec<String> {
    let mut found: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(crate::logfile::dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            // `.log` only: `.log.1` is the same name's history, not a
            // second thing to offer.
            if path.extension()?.to_str()? != "log" {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            let when = entry.metadata().ok()?.modified().ok()?;
            Some((when, name))
        })
        .collect();
    // Newest activity first: what someone is debugging is what just moved.
    found.sort_by_key(|(when, _)| std::cmp::Reverse(*when));
    found.into_iter().map(|(_, name)| name).collect()
}

fn list() -> anyhow::Result<()> {
    let dir = crate::logfile::dir();
    let names = names();
    if names.is_empty() {
        println!("no logs in {}", dir.display());
        println!();
        println!("  A log appears once `alloyfs serve`, `mount` or `start` runs —");
        println!("  those are the commands that run long enough to have a story.");
        return Ok(());
    }
    println!("{}", dir.display());
    println!();
    for name in &names {
        let path = crate::logfile::path_for(name);
        let (size, age) = match std::fs::metadata(&path) {
            Ok(md) => (
                md.len(),
                md.modified()
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| age(d.as_secs()))
                    .unwrap_or_else(|| "?".into()),
            ),
            Err(_) => (0, "?".into()),
        };
        println!("  {name:<24} {:>9}  {age} ago", human(size));
    }
    println!();
    println!("  alloyfs logs <name> [-f]");
    Ok(())
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_ages_read_like_a_person_wrote_them() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2048), "2.0 KiB");
        assert_eq!(human(9 * 1024 * 1024), "9.0 MiB");
        assert_eq!(age(5), "5s");
        assert_eq!(age(90), "1m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(172_800), "2d");
    }

    #[test]
    fn the_tail_is_the_last_n_lines_and_consumes_the_whole_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("t.log");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        // The returned offset is what --follow resumes from: it must cover
        // every byte, or the first poll re-prints the tail.
        assert_eq!(print_tail(&path, 2).unwrap(), 14);
    }
}
