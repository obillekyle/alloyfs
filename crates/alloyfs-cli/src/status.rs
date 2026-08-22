//! What a running mount is doing, written where another process can read it.
//!
//! Every counter this client keeps was invisible from outside: re-warmed
//! paths, batcher settle failures, stream-pool connections, warm directory
//! count, auto-cache occupancy, the protocol version and whether the session
//! settled on zstd or lz4. They existed, tests asserted on them, and an
//! operator asking "is this mount healthy" had no way to see any of it.
//!
//! **A file, not a socket.** A status endpoint would need a port or a named
//! pipe, a decision about who may connect, and a story for a mount running as
//! a service under another account — all to answer a question nobody asks
//! more than a few times a minute. A snapshot file next to the log costs a
//! few hundred bytes every [`INTERVAL`], needs no permissions beyond the ones
//! the log already needs, and has one property a socket cannot match: it
//! survives the process. A mount that died at 3am leaves its last known state
//! behind, which is exactly when somebody wants it.
//!
//! The cost is staleness, so staleness is reported rather than hidden: every
//! snapshot carries the instant it was written, and `alloyfs status` prints
//! the age. A file older than a few intervals is a process that is gone or
//! wedged, and it should read that way.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use alloyfs_client::RemoteFs;
use serde::{Deserialize, Serialize};

/// How often a live mount rewrites its snapshot.
///
/// Five seconds: frequent enough that `alloyfs status` describes now rather
/// than a minute ago, rare enough to be invisible — a few hundred bytes to a
/// file the OS is already caching.
pub const INTERVAL: Duration = Duration::from_secs(5);

/// Past this multiple of [`INTERVAL`] a snapshot describes a process that is
/// no longer writing, and `status` says so instead of reporting its numbers
/// as current.
const STALE_AFTER: u32 = 4;

/// `~/.alloyfs/status`.
pub fn dir() -> PathBuf {
    crate::config::app_dir().join("status")
}

fn path_for(name: &str) -> PathBuf {
    dir().join(format!("{name}.json"))
}

/// One mount's snapshot. Serde-shaped because `--json` hands it straight out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub url: String,
    pub mountpoint: String,
    /// Wall-clock instant this was written. A reader compares it against its
    /// own clock to decide whether the process is still alive — which is why
    /// it is a `SystemTime` and not a monotonic one.
    pub written_at: SystemTime,
    pub started_at: SystemTime,
    pub proto: u16,
    /// What the session actually settled on, which nothing could report after
    /// the mount printed it once at startup. Owned rather than `&'static str`
    /// so the type can be deserialized from a file that outlives nothing.
    pub compression: String,
    pub warm_dirs: usize,
    pub open_handles: usize,
    pub rewarmed_paths: u64,
    pub batch_settle_failures: u64,
    pub stream_conns: usize,
    /// Files in the auto-cache and the bytes they occupy; `None` without one.
    pub cache_files: Option<usize>,
    pub cache_bytes: Option<u64>,
}

impl Snapshot {
    pub fn age(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.written_at)
            .unwrap_or_default()
    }

    /// Has the writer stopped? See [`STALE_AFTER`].
    pub fn is_stale(&self) -> bool {
        self.age() > INTERVAL * STALE_AFTER
    }
}

/// Take a snapshot of `fs` right now.
pub fn capture(
    fs: &Arc<RemoteFs>,
    name: &str,
    url: &str,
    mountpoint: &str,
    started_at: SystemTime,
) -> Snapshot {
    let conn = fs.conn();
    let (cache_files, cache_bytes) = match fs.cache_stats() {
        Some((files, bytes)) => (Some(files), Some(bytes)),
        None => (None, None),
    };
    Snapshot {
        name: name.to_string(),
        url: url.to_string(),
        mountpoint: mountpoint.to_string(),
        written_at: SystemTime::now(),
        started_at,
        proto: conn.proto,
        // Reported as what is actually happening, not as what was configured:
        // zstd only rides v13+ sessions whose sender opted in, and below that
        // the same frames go out as lz4.
        compression: if conn.zstd_enabled() {
            "zstd"
        } else if conn.proto >= 3 {
            "lz4"
        } else {
            "none"
        }
        .to_string(),
        warm_dirs: fs.warm_dirs(),
        open_handles: fs.open_handle_count(),
        rewarmed_paths: fs.rewarmed_paths(),
        batch_settle_failures: fs.batch_settle_failures(),
        stream_conns: fs.stream_conns_established(),
        cache_files,
        cache_bytes,
    }
}

/// Write one snapshot, replacing whatever was there.
///
/// Through a temporary file and a rename, so a reader never sees half a
/// document — the write is small enough that a torn read would be unlikely
/// rather than impossible, and "unlikely" is the kind of bug that surfaces
/// once a month with no way to reproduce it.
///
/// Best-effort throughout: a mount must not fail because it could not write
/// its own telemetry. Failures are traced at debug and dropped.
pub fn write(snap: &Snapshot) {
    let dir = dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let final_path = path_for(&snap.name);
    let tmp = dir.join(format!(".{}.tmp", snap.name));
    let Ok(json) = serde_json::to_vec_pretty(snap) else {
        return;
    };
    if let Err(e) = std::fs::write(&tmp, &json) {
        tracing::debug!(error = %e, "could not write the status snapshot");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &final_path) {
        tracing::debug!(error = %e, "could not install the status snapshot");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Remove this mount's snapshot. Called on a clean unmount, so a drive that
/// went away deliberately does not linger as a stale row.
pub fn clear(name: &str) {
    let _ = std::fs::remove_file(path_for(name));
}

/// Every snapshot on this machine, newest first.
pub fn read_all() -> Vec<Snapshot> {
    let Ok(rd) = std::fs::read_dir(dir()) else {
        return Vec::new();
    };
    let mut out: Vec<Snapshot> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice(&b).ok())
        .collect();
    // Newest first, so the mount that reported most recently leads.
    out.sort_by_key(|s| std::cmp::Reverse(s.written_at));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, age: Duration) -> Snapshot {
        Snapshot {
            name: name.into(),
            url: "tcp://h:7440/x".into(),
            mountpoint: "Q:".into(),
            written_at: SystemTime::now() - age,
            started_at: SystemTime::now() - age - Duration::from_secs(60),
            proto: 14,
            compression: "zstd".into(),
            warm_dirs: 3,
            open_handles: 1,
            rewarmed_paths: 2,
            batch_settle_failures: 0,
            stream_conns: 4,
            cache_files: Some(10),
            cache_bytes: Some(4096),
        }
    }

    /// A snapshot must survive the trip through the file, because that trip
    /// is the whole mechanism: the writer and the reader are different
    /// processes and share nothing else.
    #[test]
    fn a_snapshot_round_trips_through_json() {
        let snap = sample("round-trip", Duration::from_secs(0));
        let bytes = serde_json::to_vec(&snap).expect("serializes");
        let back: Snapshot = serde_json::from_slice(&bytes).expect("deserializes");
        assert_eq!(back.name, snap.name);
        assert_eq!(back.proto, snap.proto);
        assert_eq!(back.compression, snap.compression);
        assert_eq!(back.cache_bytes, snap.cache_bytes);
        assert_eq!(back.written_at, snap.written_at, "the age depends on this");
    }

    /// Staleness is the one judgement `status` makes, and getting it backwards
    /// would report a dead mount's counters as current — which is worse than
    /// reporting nothing, because it is believable.
    #[test]
    fn a_snapshot_nobody_refreshed_reads_as_stale() {
        assert!(
            !sample("fresh", Duration::from_secs(1)).is_stale(),
            "a snapshot written a second ago is live"
        );
        assert!(
            !sample("recent", INTERVAL).is_stale(),
            "one interval late is still live — a tick can be late"
        );
        assert!(
            sample("gone", INTERVAL * (STALE_AFTER + 1)).is_stale(),
            "past the grace period, the writer has stopped"
        );
    }
}
