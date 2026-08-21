//! The write batcher: bounded write-back for NEW small files and removals.
//!
//! What it is: the client half of `WriteMany`/`RemoveMany` (v10). A burst of
//! small-file creates or deletes acknowledges locally, coalesces here for at
//! most a few milliseconds, and goes to the server as one exchange per batch
//! instead of two round trips per file — measured at 212 ms per created file
//! on a ~100 ms link before, which was the write-through floor and all of it
//! waiting.
//!
//! What it is NOT: a general write-back cache. Only two operations ever
//! acknowledge before the server has them, and both are chosen because their
//! failure modes are narrow and reportable:
//!
//! - a NEW file, written sequentially from zero, until its first release —
//!   the untar/`npm install`/build-artifact shape. Writes into EXISTING
//!   files keep full write-through semantics, which is what keeps every
//!   database's page writes on the old contract.
//! - a removal of an entry the client's complete listings can vouch for.
//!
//! The seatbelts, all load-bearing:
//!
//! - **Barriers are real.** `fsync`/`flush` on a pending file, taking any
//!   lock, renames, opening a pending path, and unmount all drain the queue
//!   and BLOCK until the server has answered. An application that syncs its
//!   data keeps exactly the durability it asked for.
//! - **Failures have an address.** Every entry answers individually
//!   (`ManyOutcome`); a refused entry is recorded per path in the damage
//!   ledger and reported at that path's next barrier — an `fsync` that
//!   returns success means the server has the bytes, full stop. Failures
//!   nobody ever asks about are logged loudly and the caches restored to
//!   the server's truth.
//! - **The window is milliseconds**, bounded by size, count and age — not a
//!   tunable eternity. A crash can lose at most what fit in it, and only
//!   for files nothing fsynced.
//!
//! Engaged only at v10+ against the negotiated session (an older peer takes
//! the classic per-operation path throughout) and switched off entirely by
//! `--write-through`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use alloyfs_proto::{Attr, ErrorCode, ManyRemove, ManyWrite, RelPath, Request, Response};
use dashmap::DashMap;

/// A pending file may grow to this many bytes before the write that crosses
/// the line materializes it onto the classic path. One `ReadMany` budget: the
/// batch framing is shared, and a file this size stops being "small".
pub(crate) const PENDING_FILE_MAX: usize = 768 * 1024;
/// Flush when the queue holds this many bytes...
const FLUSH_BYTES: u64 = 768 * 1024;
/// ...or this many operations...
const FLUSH_OPS: usize = 256;
/// ...or when the oldest has waited this long.
pub(crate) const FLUSH_AGE: std::time::Duration = std::time::Duration::from_millis(15);

pub(crate) enum PendingOp {
    Write {
        path: RelPath,
        mode: u32,
        data: Vec<u8>,
    },
    Remove {
        path: RelPath,
        dir: bool,
    },
    /// A metadata-only setattr (mtime/mode; never size — truncation changes
    /// data and stays write-through). These live in the per-path SIDE MAP,
    /// not the ordered queue: an extractor interleaves write/touch per file,
    /// and kind-alternation would cut every order-preserving run to length
    /// one. The map merges repeats per path and flushes AFTER the queue,
    /// which is correct because of two rules enforced at push time: a later
    /// write or remove on the same path DROPS the pending setattr (POSIX
    /// gives the write's own mtime anyway), and write-then-touch therefore
    /// always lands as WriteMany-then-SetattrMany in one flush.
    Setattr {
        path: RelPath,
        mtime: Option<std::time::SystemTime>,
        mode: Option<u32>,
    },
}

impl PendingOp {
    fn path(&self) -> &RelPath {
        match self {
            PendingOp::Write { path, .. }
            | PendingOp::Remove { path, .. }
            | PendingOp::Setattr { path, .. } => path,
        }
    }
    fn bytes(&self) -> u64 {
        match self {
            PendingOp::Write { data, .. } => data.len() as u64,
            PendingOp::Remove { .. } | PendingOp::Setattr { .. } => 0,
        }
    }
    /// Same-kind runs batch together; a kind change cuts the run.
    fn kind(&self) -> u8 {
        match self {
            PendingOp::Write { .. } => 0,
            PendingOp::Remove { .. } => 1,
            PendingOp::Setattr { .. } => 2,
        }
    }
}

pub(crate) struct Batcher {
    queue: Mutex<VecDeque<PendingOp>>,
    queued_bytes: AtomicU64,
    /// How many queued ops each path still has. A COUNTER, not a set — one
    /// path can hold several queued ops at once (write, remove, write again
    /// across a recreate cycle), and a set made the first completed op erase
    /// every later op's claim: barriers then skipped paths that still had
    /// work in the queue. Measured on the WAN bench as round 2 of a
    /// create/delete cycle failing 15 of its creates.
    queued: DashMap<RelPath, u32>,
    /// Files acknowledged at create but not yet sealed into the queue — they
    /// live on their open handles, and remote paths must barrier on them
    /// exactly as on queued ops.
    open_pending: DashMap<RelPath, ()>,
    /// Pending metadata-only setattrs, merged per path (see
    /// [`PendingOp::Setattr`] for why they are not in the ordered queue).
    /// Each path here holds exactly one claim in `queued`.
    setattrs: DashMap<RelPath, (Option<std::time::SystemTime>, Option<u32>)>,
    /// Per-path failures awaiting a barrier to claim them.
    damage: DashMap<RelPath, ErrorCode>,
    /// Serializes flushes: barriers and the age-flusher never interleave
    /// batches, which is what keeps queue order the wire order.
    flush_lock: Mutex<()>,
}

impl Batcher {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            queued_bytes: AtomicU64::new(0),
            queued: DashMap::new(),
            open_pending: DashMap::new(),
            setattrs: DashMap::new(),
            damage: DashMap::new(),
            flush_lock: Mutex::new(()),
        }
    }

    /// Is anything queued, unsealed, or unreported for `path`?
    pub fn involves(&self, path: &RelPath) -> bool {
        self.queued.get(path).map(|c| *c > 0).unwrap_or(false)
            || self.open_pending.contains_key(path)
            || self.damage.contains_key(path)
    }

    /// How many queued ops the path still has.
    pub fn queued_count(&self, path: &RelPath) -> u32 {
        self.queued.get(path).map(|c| *c).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock().unwrap().is_empty() && self.setattrs.is_empty()
    }

    /// True when the accumulated queue wants an immediate flush.
    pub fn wants_flush(&self) -> bool {
        self.queued_bytes.load(Ordering::Relaxed) >= FLUSH_BYTES
            || self.queue.lock().unwrap().len() >= FLUSH_OPS
            || self.setattrs.len() >= FLUSH_OPS
    }

    pub fn push(&self, op: PendingOp) {
        // A write or remove supersedes the path's pending setattr: the write
        // stamps its own server-side mtime (the POSIX answer for
        // touch-then-write), and a removed path has nothing to stamp.
        if self.setattrs.remove(op.path()).is_some() {
            self.surrender_claim(op.path());
        }
        *self.queued.entry(op.path().clone()).or_insert(0) += 1;
        self.queued_bytes.fetch_add(op.bytes(), Ordering::Relaxed);
        self.queue.lock().unwrap().push_back(op);
    }

    /// Queue a metadata-only setattr, merged per path — the second touch of
    /// one path replaces the first's fields rather than queueing again.
    pub fn push_setattr(&self, path: &RelPath, mtime: Option<std::time::SystemTime>, mode: Option<u32>) {
        let mut fresh = false;
        {
            let mut entry = self.setattrs.entry(path.clone()).or_insert_with(|| {
                fresh = true;
                (None, None)
            });
            if mtime.is_some() {
                entry.0 = mtime;
            }
            if mode.is_some() {
                entry.1 = mode;
            }
        }
        if fresh {
            *self.queued.entry(path.clone()).or_insert(0) += 1;
        }
    }

    /// Is there an unsealed pending-new file at `path`? Setattr batching
    /// must refuse those: the file exists only locally, so a flushed
    /// SetattrMany would race the seal and buy a NotFound.
    pub fn has_open_pending(&self, path: &RelPath) -> bool {
        self.open_pending.contains_key(path)
    }

    /// Drop one queued claim for `path` (the bookkeeping half of an op
    /// leaving the queue outside the flush path).
    fn surrender_claim(&self, path: &RelPath) {
        if let Some(mut c) = self.queued.get_mut(path) {
            *c = c.saturating_sub(1);
        }
        self.queued.remove_if(path, |_, c| *c == 0);
    }

    /// Claim (and clear) the recorded failure for `path`, if any — what a
    /// barrier on that path reports.
    pub fn take_damage(&self, path: &RelPath) -> Option<ErrorCode> {
        self.damage.remove(path).map(|(_, c)| c)
    }

    /// Note a path that so far exists only locally — a pending-new file not
    /// yet sealed into the queue. Remote paths must barrier on it just like
    /// on a queued op. Cleared by seal (which converts it to a queued claim)
    /// or by `forget`.
    pub fn pending_open(&self, path: &RelPath) {
        self.open_pending.insert(path.clone(), ());
    }

    /// The unsealed file's story ended without the server ever needing to
    /// hear it — deleted (or materialized) before anything was enqueued.
    pub fn forget(&self, path: &RelPath) {
        self.open_pending.remove(path);
    }

    /// Drain the queue to the server, in order, and apply the outcomes.
    ///
    /// `call` performs one request (the caller owns the connection and the
    /// runtime); `settle` receives each entry's outcome so the caller can
    /// re-patch caches with the server's attrs or restore truth on failure.
    /// Its third argument says whether this op was the path's LAST claim —
    /// queued or unsealed. Cache patches must be gated on it: an older op's
    /// outcome landing over a newer acknowledged one re-inserts entries the
    /// application already deleted. Measured on the WAN bench: a write's
    /// settle upserting the listing while that path's REMOVE sat queued
    /// behind it made round 2 of a create/delete cycle refuse 15 creates
    /// with AlreadyExists.
    ///
    /// A transport-level failure damages every entry of its batch: a bulk
    /// mutation may have applied before the connection died, so nothing here
    /// retries — the damage ledger reports, exactly as a failed entry would.
    pub fn flush_with(
        &self,
        mut call: impl FnMut(Request) -> Result<Response, ErrorCode>,
        mut settle: impl FnMut(&PendingOp, Result<&Option<Attr>, ErrorCode>, bool),
    ) {
        let _one_at_a_time = self.flush_lock.lock().unwrap();
        let mut drained: Vec<PendingOp> = {
            let mut q = self.queue.lock().unwrap();
            self.queued_bytes.store(0, Ordering::Relaxed);
            q.drain(..).collect()
        };
        // The merged setattrs flush AFTER the queue — the push rules make
        // that the only order they can need (a same-path write or remove
        // pushed later would have dropped them).
        {
            let keys: Vec<RelPath> = self.setattrs.iter().map(|e| e.key().clone()).collect();
            for path in keys {
                if let Some((_, (mtime, mode))) = self.setattrs.remove(&path) {
                    drained.push(PendingOp::Setattr { path, mtime, mode });
                }
            }
        }
        if drained.is_empty() {
            return;
        }

        // Consecutive ops of one kind form a batch; a kind change cuts it, so
        // the server applies everything in the order the application did it —
        // create w, delete w, create w again must land as exactly that.
        let mut i = 0;
        while i < drained.len() {
            let kind = drained[i].kind();
            let mut j = i;
            let mut run_bytes = 0u64;
            while j < drained.len()
                && drained[j].kind() == kind
                && (j - i) < FLUSH_OPS
                && run_bytes <= FLUSH_BYTES
            {
                run_bytes += drained[j].bytes();
                j += 1;
            }
            let run = &drained[i..j];
            let req = match kind {
                0 => Request::WriteMany {
                    files: run
                        .iter()
                        .map(|op| match op {
                            PendingOp::Write { path, mode, data } => ManyWrite {
                                path: path.clone(),
                                mode: *mode,
                                data: bytes::Bytes::from(data.clone()),
                            },
                            _ => unreachable!("run is writes"),
                        })
                        .collect(),
                },
                1 => Request::RemoveMany {
                    entries: run
                        .iter()
                        .map(|op| match op {
                            PendingOp::Remove { path, dir } => ManyRemove {
                                path: path.clone(),
                                dir: *dir,
                            },
                            _ => unreachable!("run is removes"),
                        })
                        .collect(),
                },
                _ => Request::SetattrMany {
                    entries: run
                        .iter()
                        .map(|op| match op {
                            PendingOp::Setattr { path, mtime, mode } => alloyfs_proto::ManySetattr {
                                path: path.clone(),
                                size: None,
                                mtime: *mtime,
                                mode: *mode,
                                readonly: None,
                            },
                            _ => unreachable!("run is setattrs"),
                        })
                        .collect(),
                },
            };
            // Each op surrenders its queued claim as its outcome settles;
            // "last" is what remains for the path AFTER this surrender —
            // zero claims and no unsealed handle means this outcome is the
            // path's newest truth and may patch caches.
            let done = |batcher: &Self, op: &PendingOp| -> bool {
                let remaining = {
                    let mut c = batcher.queued.entry(op.path().clone()).or_insert(1);
                    *c = c.saturating_sub(1);
                    *c
                };
                if remaining == 0 {
                    batcher.queued.remove_if(op.path(), |_, c| *c == 0);
                }
                remaining == 0 && !batcher.open_pending.contains_key(op.path())
            };
            match call(req) {
                Ok(Response::ManyOutcome(outcomes)) => {
                    for (op, outcome) in run.iter().zip(outcomes.iter()) {
                        let last = done(self, op);
                        match outcome {
                            Ok(attr) => settle(op, Ok(attr), last),
                            Err(code) => {
                                tracing::warn!(path = %op.path(), ?code, "batched mutation refused");
                                self.damage.insert(op.path().clone(), *code);
                                settle(op, Err(*code), last);
                            }
                        }
                    }
                    // A short or long reply would be a protocol bug; damage
                    // whatever got no verdict rather than guessing.
                    for op in run.iter().skip(outcomes.len()) {
                        let last = done(self, op);
                        self.damage.insert(op.path().clone(), ErrorCode::Io);
                        settle(op, Err(ErrorCode::Io), last);
                    }
                }
                Ok(other) => {
                    tracing::error!(?other, "unexpected bulk reply; damaging the batch");
                    for op in run {
                        let last = done(self, op);
                        self.damage.insert(op.path().clone(), ErrorCode::Io);
                        settle(op, Err(ErrorCode::Io), last);
                    }
                }
                Err(code) => {
                    tracing::warn!(?code, n = run.len(), "bulk mutation failed; damaging the batch");
                    for op in run {
                        let last = done(self, op);
                        self.damage.insert(op.path().clone(), code);
                        settle(op, Err(code), last);
                    }
                }
            }
            i = j;
        }
    }
}

/// A not-yet-materialized file: created locally, growing sequentially, headed
/// for one `WriteMany` entry at release. Lives on the fh's `OpenState`.
pub(crate) struct PendingNew {
    pub data: Vec<u8>,
    pub mode: u32,
    /// Set by an unlink that raced the open handle: the file is dead, release
    /// must enqueue nothing.
    pub cancelled: bool,
}
