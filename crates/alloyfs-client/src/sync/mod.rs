//! Bidirectional directory sync: a REAL local directory kept equal to a
//! server export. Exists because Linux cannot deliver inotify for remote
//! changes through any mounted filesystem (fsnotify only fires from local
//! VFS syscalls) — but changes a sync engine APPLIES to a real directory
//! are local VFS syscalls, so watchers see genuine, full-fidelity events.
//!
//! Shape: remote events (the same stream the mounts use) are applied as real
//! file operations; a local notify watcher (sharing the agent's coalescer)
//! pushes local edits back. A baseline manifest enables three-way
//! reconciliation at startup/reconnect; conflicts resolve last-writer-wins
//! with the loser preserved as a `.sync-conflict-*` file. See each module's
//! header for the invariants (mtime mirroring is the load-bearing one).

mod engine;
mod manifest;
mod reconcile;
mod transfer;

pub use engine::SyncEngine;

use std::path::PathBuf;

use crate::remote_fs::Dialer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Newer mtime wins; ties go to the server. Loser always preserved.
    Newer,
    /// Server always wins; local copy preserved on divergence.
    Remote,
    /// Local always wins; server copy preserved on divergence.
    Local,
}

impl std::str::FromStr for ConflictPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "newer" => Ok(Self::Newer),
            "remote" => Ok(Self::Remote),
            "local" => Ok(Self::Local),
            other => Err(format!("unknown conflict policy {other:?} (newer|remote|local)")),
        }
    }
}

pub struct SyncOptions {
    /// Client-side excludes, honored in BOTH directions.
    pub excludes: Vec<String>,
    /// Where the baseline manifest lives (default_data_dir()/sync/...).
    pub data_dir: PathBuf,
    /// Stable identity for this (server, export, local dir) triple.
    pub sync_key: String,
    /// Local coalescer debounce.
    pub debounce: std::time::Duration,
    pub conflict_policy: ConflictPolicy,
    /// Reconnect dialer (same type the mounts use).
    pub dialer: Option<Dialer>,
    /// Reconcile once and return instead of running watchers (CLI --one-shot).
    pub one_shot: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            excludes: Vec::new(),
            data_dir: PathBuf::new(),
            sync_key: "sync".into(),
            debounce: std::time::Duration::from_millis(250),
            conflict_policy: ConflictPolicy::Newer,
            dialer: None,
            one_shot: false,
        }
    }
}

/// Live counters — tests assert on them, `sync` logs them on shutdown.
#[derive(Default)]
pub struct SyncStats {
    pub pulls: std::sync::atomic::AtomicU64,
    pub pushes: std::sync::atomic::AtomicU64,
    pub deletes_local: std::sync::atomic::AtomicU64,
    pub deletes_remote: std::sync::atomic::AtomicU64,
    pub conflicts: std::sync::atomic::AtomicU64,
    /// Ops accepted but not yet fully processed (0 = quiescent).
    pub pending: std::sync::atomic::AtomicU64,
}

/// Paths the engine itself creates and must never sync or watch:
/// in-flight download staging files.
pub(crate) fn is_internal(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with(".ds-tmp-"))
}
