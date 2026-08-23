//! The client's filesystem: one mounted export, seen through one connection.
//!
//! Split into submodules because this was 2,835 lines in a single `impl`
//! block, and finding a method meant grep rather than structure. The struct
//! and its lifetime live here; the method groups live next door as further
//! `impl RemoteFs` blocks. Nothing changed visibility — a child module can
//! already see its parent's private fields, which is what makes the move free.
//!
//! - [`meta`] — getattr, lookup, readdir
//! - [`handles`] — the open-handle table
//! - [`read`] — the read path
//! - [`write`] — writes and the settle batcher
//! - [`mutate`] — create, remove, rename, setattr
//!
//! What stays here is what does not belong to any one of those: attaching and
//! shutting down, the accessors status reports through, cache invalidation,
//! the overlay predicates, and the protocol gating every group calls.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use alloyfs_proto::{Attr, ErrorCode, OpenFlags, RelPath, Request, Response, DATA_CHUNK};
use alloyfs_transport::{MuxConnection, TransportError};
use bytes::Bytes;
use dashmap::DashMap;

use crate::autocache::AutoCache;
use crate::error::FsError;
use crate::metacache::MetaCache;
use crate::options::{ClientOptions, Dialer};
use crate::overlay::{Overlay, OVERLAY_FH_BIT};
use crate::readahead::ReadAhead;
use crate::symlink::localize_symlink_target;

/// Unwrap one expected Response variant; anything else is a protocol-level
/// surprise that logs and becomes Io. Collapses the five-line match this
/// crate used to repeat at every RPC call site.
macro_rules! expect_resp {
    ($call:expr, $pat:pat => $out:expr) => {
        match $call {
            $pat => $out,
            other => {
                tracing::error!(?other, "unexpected response variant");
                return Err(ErrorCode::Io.into());
            }
        }
    };
}

// Declared AFTER the macro on purpose: `macro_rules!` scope is textual, and
// the children use `expect_resp!` too.
mod attach;
mod handles;
mod lock_ranges;
mod meta;
mod mutate;
mod read;
mod warm;
mod write;

use attach::{build_auto_cache, build_overlay, negotiate_defaults, spawn_background_tasks, Negotiated};
use lock_ranges::HeldRange;
use warm::ListingPatch;

use crate::batcher::{Batcher, PendingNew, PendingOp, PENDING_FILE_MAX};

/// One directory's cached remote listing: (name, ino, attr) per entry.
type DirListing = Vec<(String, u64, Attr)>;

/// `OpenState::server_fh` while no server handle has been taken out yet.
///
/// A read-only open of a file the auto-cache already holds at the right
/// version needs nothing from the server: the blob answers the reads, and the
/// attribute that proves the blob current came from the readdir that listed
/// the directory. Opening anyway cost a round trip to be told what was already
/// known, and it is the dominant cost of browsing a remote mount — measured at
/// 60 ms RTT, `ls -la` over 19 files issued 44 requests and 25 of them were
/// opens.
///
/// `u64::MAX` rather than 0 because 0 is a perfectly good server handle.
const NO_SERVER_FH: u64 = u64::MAX;

/// Block size, total blocks, free blocks — what [`RemoteFs::statfs`] reports.
type SpaceInfo = (u32, u64, u64);

/// How long a free-space answer is served from memory. See [`RemoteFs::statfs`].
///
/// Two seconds: long enough that a file manager's capacity bar, or `df` in a
/// loop, stops generating traffic, and short enough that free space still
/// visibly moves while a large copy is running — which is the one moment
/// anybody actually watches it.
const STATFS_TTL: Duration = Duration::from_secs(2);

/// Marks a handle the CLIENT invented because the open never reached the
/// server. Distinct from `OVERLAY_FH_BIT` (1 << 63), and above anything the
/// server's own small counter will reach.
pub(crate) const LAZY_FH_BIT: u64 = 1 << 62;

/// Client-side bookkeeping for one open remote handle. Keyed by the fh the
/// KERNEL holds (stable across reconnects); `server_fh` is what the current
/// server session knows and is rewritten by the reconnect supervisor.
pub(crate) struct OpenState {
    pub path: RelPath,
    pub flags: OpenFlags,
    /// The current session's handle, or [`NO_SERVER_FH`] when the open was
    /// answered entirely from cache and the server was never told. Anything
    /// that genuinely needs the server goes through
    /// [`RemoteFs::server_fh_for_io`], which takes the handle out at that
    /// point; the common case (reads that the blob satisfies) never does.
    pub server_fh: AtomicU64,
    /// May reads on this fh be served from the auto-cache blob?
    pub cache_ok: AtomicBool,
    /// Has this handle already asked the cache to warm its path? Reads are
    /// the hot path and a demand is only useful once, so the ask is guarded
    /// here rather than by hashing the path on every read.
    pub warm_asked: AtomicBool,
    /// Blocks this handle fetched, accumulating into a cache blob. See
    /// `WarmFill` — the read pays for its own warm, so nothing is fetched
    /// twice.
    pub warm_fill: std::sync::Mutex<Option<crate::autocache::WarmFill>>,
    /// Did any write happen through this fh (⇒ re-fetch on release)?
    pub wrote: AtomicBool,
    /// Sequential prefetch window for this handle.
    pub ra: ReadAhead,
    /// The advisory locks this fh currently holds (client mirror of server
    /// state) — what the reconnect supervisor replays.
    ///
    /// A list rather than one kind, because v7 locks byte ranges: a handle can
    /// hold a read lock on one range and a write lock on another at the same
    /// time, which is exactly what SQLite does. Replaying only the last one
    /// taken would restore less than the server had and call it success.
    pub lock: std::sync::Mutex<Vec<HeldRange>>,
    /// This handle's idea of the file's size, for bounding readahead.
    ///
    /// Kept here because the alternative was two map lookups on EVERY
    /// read — hash the path to find the ino, then hash the ino to find the
    /// attr — for a number the open already had in hand. Worse, when the
    /// attr entry had since been invalidated the lookup fell back to
    /// "unknown", and unknown does not merely widen the window: it turns
    /// OFF stream-pool striping for the rest of the handle's life, because
    /// the pool refuses to dial for a file whose length it cannot see.
    ///
    /// A prefetch HINT, not a fact. Our own writes advance it; a remote
    /// change may leave it short, which costs readahead and nothing else —
    /// the same trade the comment at the read site already documents.
    pub size: AtomicU64,
    /// The mtime this handle opened at, in nanoseconds since the epoch.
    /// Carried so a completed warm fill can be committed with attrs that
    /// MATCH what the server said — `fresh_for` compares mtime, and a blob
    /// committed with a made-up one would be refused at every later open,
    /// which would look exactly like the cache not working.
    pub mtime_ns: AtomicU64,
    /// Set when a reconnect could not restore this handle's lock (or the
    /// handle itself, if it held one). A poisoned handle fails read/write/
    /// lock/flush with EIO — mutual exclusion may have been broken and the
    /// application must find out; release still works.
    pub poisoned: AtomicBool,
    /// A batched NEW file still accumulating locally; None on every handle
    /// the server knows about. See batcher.rs.
    pub pending_new: Option<std::sync::Mutex<crate::batcher::PendingNew>>,
    /// The auto-cache blob, retained across this handle's reads. Opening
    /// the blob PER READ cost a file open, a path build, and a close on
    /// every cached 64 K — measured as most of the gap between a warm
    /// random read (222 µs p50) and rclone's (126 µs), four hundred times
    /// over on a small-file sweep. What is retained is a MAPPING
    /// ([`crate::autocache::Blob`]): a warm read is a memcpy out of the
    /// page cache, not a positional-read syscall into a zeroed buffer.
    /// Every path that flips `cache_ok` off drops it too, both because the
    /// blob may be replaced and because a mapping blocks eviction's
    /// remove on Windows, exactly as the open handle it replaced did.
    pub blob: std::sync::RwLock<Option<crate::autocache::Blob>>,
    /// Server version this handle last saw, for --detect-conflicts. Seeded at
    /// open, advanced by our own writes. 0 = unknown, which never conflicts:
    /// refusing a write because we never learned a version would be a bug
    /// wearing the clothes of a safety feature.
    pub version: AtomicU64,
}

/// The mount's callback for a batch of events: what turns an invalidation
/// into a kernel-cache eviction. Shared rather than owned by the pump, so the
/// canary can push synthetic events down the same route.
pub(crate) type EventSink = std::sync::Arc<dyn Fn(&[alloyfs_proto::FsEvent]) + Send + Sync>;

pub struct RemoteFs {
    conn: RwLock<Arc<MuxConnection>>,
    pub(crate) rt: tokio::runtime::Handle,
    pub ino: crate::InodeTable,
    pub root_attr: Attr,
    attr_cache: DashMap<u64, (Attr, Instant)>,
    /// Where the rotating canary sample resumes each round.
    canary_cursor: AtomicU64,
    /// The mount's event callback, kept so paths the canary finds stale can be
    /// pushed through the same notifier a real server event uses.
    pub(crate) event_sink: std::sync::OnceLock<EventSink>,
    /// Complete REMOTE listing per directory ino, good for [`DIR_TTL_PUSH`]/[`DIR_TTL_POLL`] (pump-health-dependent) or
    /// until an event touches a child. One structure, three answers:
    ///
    /// - a repeat readdir is local (Explorer re-enumerates on focus, F5 and
    ///   every navigation — measured 62.9 ms per listing without this);
    /// - a lookup of a name PRESENT in a live listing is local, which is what
    ///   kills the FUSE/kernel per-entry LOOKUP storm after a readdir;
    /// - a lookup of a name ABSENT from a live listing is a local `NotFound`,
    ///   because the listing is complete. Windows probes missing names
    ///   pathologically (resolver walks, `desktop.ini`, `AutoRun.inf`), and
    ///   each one was a full round trip forever. The excludes accidentally
    ///   proved the fix: `desktop.ini` is in `LOCAL_ARTIFACTS`, routes to the
    ///   overlay, and answers in 0.6 ms while every other missing name paid
    ///   62 ms.
    ///
    /// REMOTE entries only — overlay children are merged live on every serve,
    /// so local overlay activity never needs to invalidate this, and lookups
    /// for overlay-routed names branch away before ever consulting it.
    dir_cache: DashMap<u64, (DirListing, Instant)>,
    /// The last free-space answer, and when it was fetched. See `statfs`.
    statfs_cache: std::sync::Mutex<Option<(SpaceInfo, Instant)>>,
    /// Bumped by every attr invalidation. An in-flight bulk re-warm
    /// (events.rs) compares it before seeding, so a `GetattrMany` reply can
    /// never re-install attributes over a newer invalidation. Global on
    /// purpose, the same trade `dir_epoch` documents below: an unrelated
    /// event only costs one discarded re-warm, the safe direction.
    pub(crate) attr_epoch: AtomicU64,
    /// Paths the event pump's bulk re-warm has re-seeded — observability
    /// for tests and diagnostics, like `requests_sent` on the mux.
    pub(crate) rewarmed: AtomicU64,
    /// Batched mutations the server refused (per-entry Err at settle).
    /// Damage is reported at barriers, but a failure nobody barriers on
    /// was only a dropped tracing line — and tests run without a
    /// subscriber, which hid a WSL-gate flake behind "file missing" with
    /// no cause. Now every settle failure counts, and the batcher tests
    /// assert this stays zero.
    pub(crate) settle_failures: AtomicU64,
    /// Bumped by every listing invalidation, so a `readdir` that started
    /// before a mutation cannot install its now-stale result after it.
    ///
    /// `readdir` fetches pages over several round trips and then inserts. The
    /// invalidations are a bare `remove`, so the two interleave: mutate,
    /// invalidate, then an in-flight fetch lands a pre-mutation listing and
    /// stamps it fresh. That is worse than staleness — a listing is treated as
    /// COMPLETE, so `lookup` answers a hard NotFound for a file that exists
    /// (and keeps answering it for the rest of the listing TTL). Comparing this before
    /// and after the fetch closes it. Deliberately global rather than
    /// per-directory: a mutation elsewhere only costs one skipped insert,
    /// which is the safe direction to be wrong in.
    dir_epoch: AtomicU64,
    /// Directory listings restored from the on-disk metadata snapshot
    /// (metacache.rs), keyed by PATH because they outlive any one process's
    /// ino numbering. Consulted where `dir_cache` misses, and it gives the
    /// same three answers — repeat listings, positive lookups, and the hard
    /// negative for absent names — because a warm listing is complete by the
    /// same token proof that lets the auto-cache serve blobs without
    /// re-validation.
    ///
    /// No TTL, deliberately. The listing TTL exists because a live listing's
    /// freshness is a bet on the event stream staying up; a warm listing's
    /// freshness is the tree token the walker verified at mount. It stays
    /// until an invalidation removes it, so every path that busts `dir_cache`
    /// must bust this too — and the paths that exist because events were
    /// MISSED (`invalidate_all` on lag, resync, reconnect) clear it outright.
    warm: DashMap<RelPath, Vec<(String, Attr)>>,
    /// Bumped by every warm-tier invalidation. The snapshot install at mount
    /// captures it BEFORE the token exchange and re-checks before installing,
    /// so listings read from disk cannot land after a mutation that would
    /// have busted them — the in-flight-install bug `dir_epoch` closed for
    /// readdir, at mount scope. A separate counter because plain writes bust
    /// warm (no TTL bounds a stale size there, unlike `dir_cache`) and must
    /// not churn the readdir-insert guard.
    warm_epoch: AtomicU64,
    /// The on-disk metadata snapshot. Present only when the auto-cache is:
    /// without a walker there is no tree token, and without the token there
    /// is no proof to serve the snapshot by.
    meta: Option<MetaCache>,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) cache: Option<Arc<AutoCache>>,
    pub(crate) open_files: DashMap<u64, OpenState>,
    /// Which handles are open on each path — `open_files` read the other way
    /// round.
    ///
    /// Three hot operations ask "what is open on THIS path": an incoming
    /// event about it, a rename (twice, for both ends), and a batched unlink.
    /// Each used to walk every open handle to find out, so a change storm on
    /// a mount with many files open cost handles × events for answers that
    /// were almost always "none".
    ///
    /// Maintained only by `track_open` and `untrack_open`, which are the only
    /// two functions permitted to touch `open_files`'s membership. Neither
    /// ever holds a guard on both maps at once, so the pair cannot deadlock
    /// against itself. `OpenState::path` is never rewritten in place — a
    /// rename leaves an open handle pointing at the name it was opened under
    /// — so no entry here ever needs retargeting.
    open_by_path: DashMap<RelPath, Vec<u64>>,
    export: String,
    pub(crate) dialer: Option<Dialer>,
    /// Extra data connections for cold sequential streams; None unless a
    /// dialer exists and stream_conns > 0. See stream_pool.rs.
    pub(crate) stream_pool: Option<Arc<crate::stream_pool::StreamPool>>,
    /// Bumped after every successful reconnect; the event pump watches it.
    conn_epoch: tokio::sync::watch::Sender<u64>,
    /// Highest event seq the pump has applied — reconnect resubscribes here.
    pub(crate) last_event_seq: AtomicU64,
    /// The tree token `Attach2` carried, spent by the walker's first skip
    /// check (take-once: 0 after). Mount-time only — anything later must ask
    /// the wire, because this describes the export as of attach.
    pub(crate) attach_tree_token: AtomicU64,
    /// Is the event pump subscribed and keeping up RIGHT NOW? Selects between
    /// the push and poll TTLs above. False until the first successful
    /// subscribe — a mount that never starts a pump keeps today's 5 s
    /// behaviour — and false again on lag, stream close, or while waiting out
    /// a reconnect. Set only by the pump (events.rs).
    pub(crate) pump_healthy: std::sync::atomic::AtomicBool,
    /// Send `expect_version` with writes and refuse to clobber (opt-in).
    detect_conflicts: bool,
    /// Local mountpoint, for symlink target rewriting. See `localize_target`.
    mount_root: Option<String>,
    /// Counter for handles the client invents when an open is answered from
    /// cache. Combined with [`LAZY_FH_BIT`] so it cannot collide with a server
    /// handle or with the overlay's.
    next_lazy_fh: AtomicU64,
    /// The v10 write batcher — `Some` when the session speaks v10 and the
    /// mount did not opt out with `--write-through`. See batcher.rs for the
    /// exact ack-early contract and its barriers.
    pub(crate) batch: Option<Batcher>,
}

impl RemoteFs {
    pub async fn attach(conn: Arc<MuxConnection>, export: &str) -> Result<Arc<Self>, FsError> {
        Self::attach_with(conn, export, ClientOptions::default()).await
    }

    /// Attach with overlay/auto-cache/reconnect options. Spawns the cache
    /// walker, fetcher, manifest flusher, and reconnect supervisor as
    /// configured.
    pub async fn attach_with(
        conn: Arc<MuxConnection>,
        export: &str,
        opts: ClientOptions,
    ) -> Result<Arc<Self>, FsError> {
        let proto = conn.proto;
        // v9+: attach, mount defaults and the tree token arrive in ONE
        // exchange. The three used to be sequential round trips — attach,
        // then ask for defaults, then (in the walker) ask for the token —
        // pure protocol ceremony before a mount could serve anything, and on
        // a 60 ms link the difference between one exchange and three is the
        // difference a user feels at mount time.
        //
        // The defaults ride into `negotiate_defaults` as a locally
        // synthesized `MountDefaults` response, so the negotiation logic —
        // union lists, explicit-zero rules, all of it pinned by its own unit
        // tests — runs unchanged on either wire shape.
        let (root_attr, prefetched, attach_token) = if conn.proto >= 9 {
            let resp = conn
                .request(Request::Attach2 {
                    export: export.into(),
                })
                .await??;
            expect_resp!(resp, Response::Attached2 {
                root_attr, exclude, pin, auto_cache_max, auto_cache_budget, tree_token, ..
            } => (
                root_attr,
                Some(Response::MountDefaults { exclude, pin, auto_cache_max, auto_cache_budget }),
                tree_token,
            ))
        } else {
            let resp = conn
                .request(Request::Attach {
                    export: export.into(),
                })
                .await??;
            expect_resp!(resp, Response::AttachOk { root_attr, .. } => (root_attr, None, 0))
        };

        let Negotiated {
            opts,
            auto_cache_max,
            auto_cache_budget,
        } = negotiate_defaults(opts, conn.proto, || async {
            match prefetched {
                Some(resp) => Some(resp),
                None => conn.request(Request::MountDefaults).await.ok()?.ok(),
            }
        })
        .await;

        let overlay = build_overlay(&opts)?;
        let (cache, fetch_rx, meta) = build_auto_cache(&opts, auto_cache_max, auto_cache_budget)?;

        let (epoch_tx, _) = tokio::sync::watch::channel(0u64);
        let fs = Arc::new(Self {
            conn: RwLock::new(conn),
            rt: tokio::runtime::Handle::current(),
            ino: crate::InodeTable::new(),
            root_attr,
            attr_cache: DashMap::new(),
            canary_cursor: AtomicU64::new(0),
            event_sink: std::sync::OnceLock::new(),
            attr_epoch: AtomicU64::new(0),
            rewarmed: AtomicU64::new(0),
            settle_failures: AtomicU64::new(0),
            dir_cache: DashMap::new(),
            open_by_path: DashMap::new(),
            statfs_cache: std::sync::Mutex::new(None),
            dir_epoch: AtomicU64::new(0),
            warm: DashMap::new(),
            warm_epoch: AtomicU64::new(0),
            meta,
            overlay,
            cache,
            open_files: DashMap::new(),
            export: export.to_string(),
            dialer: opts.dialer.clone(),
            stream_pool: match (&opts.dialer, opts.stream_conns) {
                (Some(d), n) if n > 0 => Some(crate::stream_pool::StreamPool::new(
                    d.clone(),
                    export.to_string(),
                    n,
                    &tokio::runtime::Handle::current(),
                )),
                _ => None,
            },
            conn_epoch: epoch_tx,
            last_event_seq: AtomicU64::new(0),
            pump_healthy: std::sync::atomic::AtomicBool::new(false),
            attach_tree_token: AtomicU64::new(attach_token),
            detect_conflicts: opts.detect_conflicts,
            mount_root: opts.mount_root.clone(),
            next_lazy_fh: AtomicU64::new(1),
            batch: (proto >= 10 && !opts.write_through).then(Batcher::new),
        });
        spawn_background_tasks(&fs, fetch_rx);
        Ok(fs)
    }

    /// Clean shutdown: persist the cache manifest. Call after unmount.
    pub fn shutdown(&self) {
        // Unmount is the last barrier there will ever be.
        self.flush_batch();
        if let Some(cache) = &self.cache {
            // The final write: the one flush that also persists which
            // entries were hot, so the next mount's evictions start
            // informed.
            cache.flush_manifest_final();
            let (n, bytes) = cache.stats();
            tracing::info!(entries = n, bytes, "auto-cache manifest flushed");
        }
    }

    /// The live connection (may change across reconnects — take a snapshot,
    /// never hold it across long waits).
    pub fn conn(&self) -> Arc<MuxConnection> {
        self.conn.read().unwrap().clone()
    }

    /// Connections the stream pool has established over its lifetime; 0
    /// without a pool. Observability: the loopback test pins engagement on
    /// it, and a diag can tell "pool never dialed" from "pool dialed and
    /// died" without packet captures.
    pub fn stream_conns_established(&self) -> usize {
        self.stream_pool.as_ref().map_or(0, |p| p.established())
    }

    /// Lanes live right now and the number configured — `(0, 0)` without a
    /// pool.
    ///
    /// The pair, not either half: `established` alone counts connections that
    /// may since have died, so a pool reporting 3 could be serving 1. A
    /// shortfall is a supported state (reads fall back to the primary), which
    /// is exactly why it has to be visible rather than inferred from
    /// throughput.
    pub fn stream_conns_live(&self) -> (usize, usize) {
        self.stream_pool.as_ref().map_or((0, 0), |p| p.live_and_target())
    }

    /// Files the auto-cache holds and the bytes they occupy, or `None` on a
    /// mount running without one.
    ///
    /// The numbers existed and were logged exactly once, at shutdown, which
    /// is the one moment nobody is looking. `alloyfs status` asks here.
    /// A few paths the kernel could currently be answering from cache, for the
    /// canary to re-check.
    ///
    /// ROTATING, not the first n. A fixed window would re-check the same
    /// handful forever and never notice a change anywhere else — for a
    /// detector, the difference between working and appearing to.
    ///
    /// Sampling rather than sweeping is deliberate: a pump that has stopped is
    /// wrong about everything, so a handful of paths finds it as surely as all
    /// of them, at one bulk stat instead of thousands.
    pub(crate) fn sample_cached_paths(&self, n: usize) -> Vec<RelPath> {
        let all: Vec<RelPath> = self
            .attr_cache
            .iter()
            .filter_map(|e| self.ino.path_of(*e.key()))
            .filter(|p| !p.is_root() && !self.is_overlay(p))
            .collect();
        if all.is_empty() {
            return Vec::new();
        }
        let start = self.canary_cursor.fetch_add(n as u64, Ordering::Relaxed) as usize % all.len();
        all.iter()
            .cycle()
            .skip(start)
            .take(n.min(all.len()))
            .cloned()
            .collect()
    }

    /// Does the server's attr disagree with what we hold for `path`?
    ///
    /// Only what a client would notice and act on: size, mtime, version. A
    /// difference means a change nobody told us about.
    pub(crate) fn cached_attr_differs(&self, path: &RelPath, server: &Attr) -> bool {
        let Some(ino) = self.ino.ino_of(path) else {
            return false; // not tracked; nothing of ours to be stale
        };
        let Some(hit) = self.attr_cache.get(&ino) else {
            return false;
        };
        let ours = hit.0;
        ours.size != server.size
            || ours.mtime != server.mtime
            // Version 0 on either side is "unknowable" — the same escape hatch
            // blob freshness uses. Treating it as a difference would make the
            // canary cry wolf on every export nobody writes through alloyfs.
            || (ours.version != 0 && server.version != 0 && ours.version != server.version)
    }

    /// Treat these paths as changed: drop them from our caches and push them
    /// through the mount's notifier, which is what evicts the kernel's copies.
    pub(crate) fn force_invalidate(&self, paths: &[RelPath]) {
        let batch = crate::canary::events_for(paths);
        self.apply_events(&batch);
        self.apply_events_to_cache(&batch);
        if let Some(sink) = self.event_sink.get() {
            sink(&batch);
        }
    }

    pub fn cache_stats(&self) -> Option<(usize, u64)> {
        self.cache.as_ref().map(|c| c.stats())
    }

    /// Paths the event pump's bulk re-warm has re-seeded so far. 0 forever
    /// below wire v12 — the pin the gating test uses.
    pub fn rewarmed_paths(&self) -> u64 {
        self.rewarmed.load(Ordering::Relaxed)
    }

    /// Batched mutations the server refused so far. The batcher tests pin
    /// this at zero — a refused entry is otherwise only a barrier report
    /// or a dropped tracing line. See the field.
    pub fn batch_settle_failures(&self) -> u64 {
        self.settle_failures.load(Ordering::Relaxed)
    }

    /// The reconnect epoch right now. Capture it BEFORE doing work whose
    /// failure you'll respond to with `conn_changed_since` — otherwise a
    /// supervisor bump that lands in between is silently missed.
    pub(crate) fn conn_epoch_now(&self) -> u64 {
        *self.conn_epoch.subscribe().borrow()
    }

    /// Resolves once the epoch has advanced PAST `since` (returns instantly
    /// when a reconnect already happened between the caller's capture and
    /// this call — that's the race this API shape exists to close).
    pub(crate) async fn conn_changed_since(&self, since: u64) {
        let mut rx = self.conn_epoch.subscribe();
        while *rx.borrow() <= since {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await; // no supervisor: never
            }
        }
    }

    /// One request on the current connection; if the connection died and the
    /// supervisor already swapped in a new one, IDEMPOTENT READS retry once.
    /// Mutations never retry (the original may have applied before the drop).
    fn call(&self, req: Request) -> Result<Response, FsError> {
        let conn = self.conn();
        let retryable = matches!(
            req,
            Request::Getattr { .. } | Request::Readdir { .. } | Request::Read { .. } | Request::Statfs
        );
        // Only the retryable variants need a spare copy. Cloning
        // unconditionally allocated a fresh RelPath String on EVERY
        // operation — including every write, whose payload is a Bytes that
        // would have been cloned along with it — to serve a retry the other
        // variants can never take.
        let saved = retryable.then(|| req.clone());
        let first = self.rt.block_on(conn.request(req));
        match first {
            Err(TransportError::Closed) if retryable => {
                let now = self.conn();
                if !Arc::ptr_eq(&conn, &now) && !now.is_closed() {
                    // `saved` is Some whenever `retryable` is.
                    let again = saved.expect("retryable requests keep a copy");
                    return Ok(self.rt.block_on(now.request(again))??);
                }
                Err(TransportError::Closed.into())
            }
            other => Ok(other??),
        }
    }

    /// Is this path routed to the local overlay?
    pub(crate) fn is_overlay(&self, path: &RelPath) -> bool {
        self.overlay.as_ref().is_some_and(|o| o.excluded(path))
    }

    /// Readdir intent name #1: a server entry with this name must NOT be
    /// listed — the overlay's copy is the only visible one on this client.
    fn shadowed_by_overlay(&self, child: &RelPath) -> bool {
        self.is_overlay(child)
    }

    /// Readdir intent name #2: an on-disk overlay child belongs in the
    /// listing (it matches the exclude patterns, so it routes local).
    fn lives_in_overlay(&self, child: &RelPath) -> bool {
        self.is_overlay(child)
    }

    fn overlay_ref(&self) -> &Overlay {
        self.overlay.as_ref().expect("overlay routing checked by caller")
    }

    fn path_of(&self, ino: u64) -> Result<RelPath, FsError> {
        self.ino.path_of(ino).ok_or_else(|| ErrorCode::NotFound.into())
    }

    /// Translate a kernel-visible fh to the current server session's fh.
    fn server_fh(&self, fh: u64) -> u64 {
        self.open_files
            .get(&fh)
            .map(|s| s.server_fh.load(Ordering::Acquire))
            .unwrap_or(fh)
    }

    /// The server handle for `fh`, opening the file on the server if this
    /// handle has so far been served entirely from cache.
    ///
    /// Every operation that genuinely needs the server calls this instead of
    /// [`Self::server_fh`]. Reads answered by the auto-cache return before
    /// reaching it, which is the case this exists to keep off the wire.
    fn server_fh_for_io(&self, fh: u64) -> Result<u64, FsError> {
        let existing = self.server_fh(fh);
        if existing != NO_SERVER_FH {
            return Ok(existing);
        }
        let (path, flags) = {
            let Some(state) = self.open_files.get(&fh) else {
                return Err(ErrorCode::BadHandle.into());
            };
            (state.path.clone(), state.flags)
        };
        // No lock held across the call. A DashMap guard cannot span a blocking
        // request without risking a deadlock against the same shard, so two
        // threads are allowed to race and the loser gives its handle back.
        // Racing costs one redundant open; holding a shard lock across a
        // network round trip would cost the mount.
        let (server_fh, attr) = expect_resp!(
            self.call(Request::Open { path, flags })?,
            Response::Opened { fh: server, attr } => (server, attr)
        );
        let Some(state) = self.open_files.get(&fh) else {
            // Released while we were opening: hand the handle straight back.
            let _ = self
                .rt
                .block_on(self.conn().send_oneway(Request::Release { fh: server_fh }));
            return Err(ErrorCode::BadHandle.into());
        };
        match state
            .server_fh
            .compare_exchange(NO_SERVER_FH, server_fh, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                state.version.store(attr.version, Ordering::Relaxed);
                Ok(server_fh)
            }
            // Someone else materialised first. Theirs is the handle every other
            // thread already sees, so ours is the one that has to go.
            Err(theirs) => {
                let _ = self
                    .rt
                    .block_on(self.conn().send_oneway(Request::Release { fh: server_fh }));
                Ok(theirs)
            }
        }
    }

    // ---------------------------------------------------------------- reads

    /// The session's negotiated wire protocol. Mount glue keys optional
    /// work off it — e.g. pre-v5 servers answer writes without attributes,
    /// so only their write path needs a stat up front.
    pub fn server_proto(&self) -> u16 {
        self.conn().proto
    }

    /// The kernel is done with this inode number: drop the table entry AND
    /// the per-ino caches. `InodeTable::forget` alone left `attr_cache` and
    /// `dir_cache` entries for the number unreachable but immortal — and
    /// the WinFsp backend forgets on every failed resolve, i.e. on every
    /// probe of a missing name, which Windows issues relentlessly. On a
    /// long-lived mount over a big export that was monotonic growth with
    /// no ceiling. Mounts call this instead of reaching for `.ino` — the
    /// table alone is not the whole story of an ino.
    pub fn forget(&self, ino: u64) {
        if ino == crate::inode::ROOT_INO {
            return;
        }
        self.attr_cache.remove(&ino);
        self.dir_cache.remove(&ino);
        self.ino.forget(ino);
    }

    /// Refuse an operation the negotiated protocol cannot carry.
    ///
    /// Sending a v4 variant to a v3 peer would not fail cleanly — postcard
    /// would decode the variant index as something else entirely, or as
    /// garbage. Better to say so.
    fn require_proto(&self, need: u16, what: &str) -> Result<(), FsError> {
        let have = self.conn().proto;
        if have < need {
            tracing::warn!(have, need, what, "the server is too old for this operation");
            return Err(ErrorCode::VersionMismatch.into());
        }
        Ok(())
    }

    /// EIO for handles whose lock (or reopen) was lost across a reconnect —
    /// mutual exclusion may have been broken and silence would hide it.
    fn check_poisoned(&self, fh: u64) -> Result<(), FsError> {
        match self.open_files.get(&fh) {
            Some(state) if state.poisoned.load(Ordering::Acquire) => Err(ErrorCode::Io.into()),
            _ => Ok(()),
        }
    }

    pub fn flush(&self, fh: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return self.overlay_ref().flush(fh);
        }
        self.check_poisoned(fh)?;
        // fsync's promise is server-side bytes. A pending file seals and
        // goes out NOW; any batched history on this path drains; and what
        // broke for this path is THIS call's error — an Ok from here means
        // the server has everything this path was ever acknowledged for.
        if let Some(state) = self.open_files.get(&fh) {
            let pending = state.pending_new.is_some();
            let path = state.path.clone();
            drop(state);
            if pending {
                self.seal_pending(fh);
                return self.barrier_for(&path);
            }
            if self.batch.as_ref().is_some_and(|b| b.involves(&path)) {
                self.barrier_for(&path)?;
            }
        }
        let server_fh = self.server_fh_for_io(fh)?;
        expect_resp!(self.call(Request::Flush { fh: server_fh })?, Response::Ok => ());
        Ok(())
    }

    pub fn release(&self, fh: u64) {
        if fh & OVERLAY_FH_BIT != 0 {
            self.overlay_ref().release(fh);
            return;
        }
        // A pending NEW file's close is where its one WriteMany entry is
        // born. The server never knew this handle, so nothing else releases.
        if self.open_files.get(&fh).is_some_and(|e| e.pending_new.is_some()) {
            self.seal_pending(fh);
            self.untrack_open(fh);
            return;
        }
        // A fill that never completed still holds real blocks. Keep them:
        // the sparse blob moves into place and the entry records which parts
        // of it are real, so the next open of this file resumes instead of
        // starting over. Discarding them is what made a scrubbing reader
        // pay full price on every pass.
        if let Some(cache) = &self.cache {
            if let Some(state) = self.open_files.get(&fh) {
                let mut guard = state.warm_fill.lock().unwrap();
                if let Some(fill) = guard.as_mut() {
                    let (size, version, on_disk) = (fill.size(), fill.version(), fill.on_disk());
                    if let Some(map) = fill.keep_partial(&cache.blob_final_path(&state.path)) {
                        let attr = alloyfs_proto::Attr {
                            kind: alloyfs_proto::FileKind::File,
                            size,
                            mtime: std::time::UNIX_EPOCH
                                + std::time::Duration::from_nanos(state.mtime_ns.load(Ordering::Relaxed)),
                            ctime: std::time::UNIX_EPOCH,
                            mode: 0o644,
                            version,
                        };
                        cache.commit_partial(&state.path, &attr, map, on_disk);
                        tracing::debug!(path = %state.path, on_disk, "partial fill kept");
                    }
                }
                *guard = None;
            }
        }
        let server_fh = self.server_fh(fh);
        if let Some(state) = self.untrack_open(fh) {
            // Pool sessions opened their own handles for this file's stream;
            // close them with it so long-lived pool connections don't
            // accumulate handles (and Windows-server share locks).
            if let Some(pool) = &self.stream_pool {
                pool.forget_path(&self.rt, &state.path);
            }
            if crate::readahead::Stats::enabled() {
                let s = &state.ra.stats;
                use std::sync::atomic::Ordering::Relaxed;
                tracing::info!(
                    path = %state.path,
                    window_hits = s.window_hits.load(Relaxed),
                    retained_hits = s.retained_hits.load(Relaxed),
                    sync_fetches = s.sync_fetches.load(Relaxed),
                    clears = s.clears.load(Relaxed),
                    tolerated_ooo = s.tolerated_ooo.load(Relaxed),
                    "read stats (ALLOYFS_READ_STATS)"
                );
            }
            state.ra.clear();
            if state.wrote.load(Ordering::Relaxed) {
                if let Some(cache) = &self.cache {
                    // The finished file may qualify for (re-)caching now.
                    cache.enqueue_refetch(state.path);
                }
            }
        }
        // Nothing to release when the open was answered from cache: the server
        // was never told this file was open, so it is holding nothing. This is
        // the whole point of the lazy handle — `ls` and Explorer open, look and
        // close again without the server ever hearing about it.
        if server_fh == NO_SERVER_FH {
            return;
        }
        // Fire-and-forget. The reply was already being discarded, but `call`
        // still blocked until it arrived — a full round trip on every close,
        // for an answer nobody read.
        //
        // What that cost: `ls -la` over a 60 ms link spent ~2 RTT per file,
        // one to open and one to wait out the release, and every open/close
        // heavy client pays it the same way — Explorer, git, bun. The same
        // listing against a loopback mount finished in 158 ms against 2293 ms
        // remote, which is how the round trips were identified as the cost
        // rather than anything the attribute cache could have helped with.
        //
        // Not retried on a dead connection, deliberately: a handle whose
        // connection is gone has already been released by the agent, which
        // drops a session's handles on disconnect and reclaims the rest when
        // the lease expires. Retrying would re-open that question for no gain.
        let _ = self
            .rt
            .block_on(self.conn().send_oneway(Request::Release { fh: server_fh }));
    }

    /// Block size, total blocks, free blocks — cached for [`STATFS_TTL`].
    ///
    /// Nothing above this caches free space. Linux does not cache statfs at
    /// all, so every `df`, every file-manager window that shows a capacity
    /// bar, and every `statvfs` in a build script was a full round trip made
    /// from inside a kernel callback; a file manager left open polls it on a
    /// timer forever. All three mount backends come through here, so one TTL
    /// covers them.
    ///
    /// Staleness is the right trade because free space on a shared export is
    /// advisory no matter how it is fetched: another client can consume the
    /// space between the reply and the caller acting on it. A few seconds of
    /// age adds nothing to a number that was never a promise.
    pub fn statfs(&self) -> Result<SpaceInfo, FsError> {
        if let Ok(guard) = self.statfs_cache.lock() {
            if let Some((out, when)) = &*guard {
                if when.elapsed() < STATFS_TTL {
                    return Ok(*out);
                }
            }
        }
        // The lock is NOT held across the call. Two threads arriving on a
        // cold cache both ask, which costs one redundant round trip; holding
        // it would instead park a kernel callback behind another callback's
        // network I/O.
        let out = expect_resp!(
            self.call(Request::Statfs)?,
            Response::Statfs { block_size, blocks, blocks_free } => (block_size, blocks, blocks_free)
        );
        if let Ok(mut guard) = self.statfs_cache.lock() {
            *guard = Some((out, Instant::now()));
        }
        Ok(out)
    }
}
