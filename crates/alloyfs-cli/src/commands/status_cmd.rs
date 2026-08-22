//! `alloyfs status` — what every mount on this machine is doing.

/// One row per mount, or the JSON documents themselves.
///
/// Reads the snapshots each running mount publishes (see `crate::status`).
/// Age is shown rather than hidden: a snapshot is only as current as the last
/// time its process wrote one, and a process that stopped writing leaves a
/// file that would otherwise read as live.
pub fn run(json: bool) -> anyhow::Result<()> {
    let snaps = crate::status::read_all();
    if json {
        println!("{}", serde_json::to_string_pretty(&snaps)?);
        return Ok(());
    }
    if snaps.is_empty() {
        println!("no mounts are running.");
        println!("  alloyfs mount <url> <mountpoint>   or   alloyfs start");
        return Ok(());
    }
    println!("NAME             AT               PROTO  COMPR  UPTIME    CACHE          STATE");
    for s in &snaps {
        let uptime = s
            .written_at
            .duration_since(s.started_at)
            .map(|d| human_duration(d.as_secs()))
            .unwrap_or_else(|_| "?".into());
        let cache = match (s.cache_files, s.cache_bytes) {
            (Some(f), Some(b)) => format!("{f} files {}", human_bytes(b)),
            _ => "off".into(),
        };
        // The state column is the point of the age field: a snapshot nobody
        // has refreshed describes a process that is gone or wedged, and
        // printing its counters as though they were current would be a lie
        // the reader has no way to catch.
        let state = if s.is_stale() {
            format!("STALE ({} ago)", human_duration(s.age().as_secs()))
        } else {
            "running".into()
        };
        println!(
            "{:<16} {:<16} {:<6} {:<6} {:<9} {:<14} {}",
            truncate(&s.name, 16),
            truncate(&s.mountpoint, 16),
            format!("v{}", s.proto),
            s.compression,
            uptime,
            cache,
            state
        );
        println!("                 {}", s.url);
        // Counters worth a line only when they say something. A mount with
        // nothing re-warmed, nothing refused and no extra streams is the
        // normal case, and printing three zeros for it buries the one row
        // that does have a number.
        let mut notes = Vec::new();
        if s.warm_dirs > 0 {
            notes.push(format!("{} warm dirs", s.warm_dirs));
        }
        if s.open_handles > 0 {
            notes.push(format!("{} open", s.open_handles));
        }
        if s.rewarmed_paths > 0 {
            notes.push(format!("{} re-warmed", s.rewarmed_paths));
        }
        if s.stream_conns > 0 {
            notes.push(format!("{} stream conns", s.stream_conns));
        }
        if s.batch_settle_failures > 0 {
            // The one counter that is bad news rather than trivia: a batched
            // mutation the server refused after this client already
            // acknowledged it locally.
            notes.push(format!("{} SETTLE FAILURES", s.batch_settle_failures));
        }
        if !notes.is_empty() {
            println!("                 {}", notes.join(", "));
        }
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn human_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h{}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{}h", s / 86400, (s % 86400) / 3600),
    }
}

fn human_bytes(b: u64) -> String {
    const K: u64 = 1024;
    match b {
        n if n < K => format!("{n}B"),
        n if n < K * K => format!("{:.0}K", n as f64 / K as f64),
        n if n < K * K * K => format!("{:.1}M", n as f64 / (K * K) as f64),
        n => format!("{:.1}G", n as f64 / (K * K * K) as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_durations() {
        assert_eq!(human_duration(9), "9s");
        assert_eq!(human_duration(90), "1m30s");
        assert_eq!(human_duration(3700), "1h1m");
        assert_eq!(human_duration(90_000), "1d1h");
    }

    #[test]
    fn bytes_read_as_bytes() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(2048), "2K");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0M");
    }

    /// Truncation must not split a multi-byte character, which is the way a
    /// byte-wise `&s[..n]` would panic on a mountpoint containing one.
    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("short", 16), "short");
        let wide = "ααααααααααααααααααα";
        let cut = truncate(wide, 8);
        assert_eq!(cut.chars().count(), 8, "8 characters, ellipsis included");
    }
}
