use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

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
mod lock_ranges;
mod warm;

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

pub struct RemoteFs {
    conn: RwLock<Arc<MuxConnection>>,
    pub(crate) rt: tokio::runtime::Handle,
    pub ino: crate::InodeTable,
    pub root_attr: Attr,
    attr_cache: DashMap<u64, (Attr, Instant)>,
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
            attr_epoch: AtomicU64::new(0),
            rewarmed: AtomicU64::new(0),
            settle_failures: AtomicU64::new(0),
            dir_cache: DashMap::new(),
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

    pub fn getattr(&self, ino: u64) -> Result<Attr, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            // Local stat is ~µs; bypass the TTL cache entirely.
            return self.overlay_ref().getattr(&path);
        }
        if let Some(hit) = self.attr_cache.get(&ino) {
            let (attr, when) = *hit;
            if when.elapsed() < self.attr_ttl() {
                return Ok(attr);
            }
        }
        // The WinFsp backend resolves names through getattr rather than
        // lookup, so the negative half of the listing cache has to be
        // consulted here too — a missing name allocates a fresh ino, lands
        // here with no attr cached, and used to pay a round trip to be told
        // "no" every single time.
        if let Some((parent, name)) = path.split() {
            if let Some(pino) = self.ino.ino_of(&parent) {
                if let Some(hit) = self.dir_cache.get(&pino) {
                    let (entries, when) = &*hit;
                    if when.elapsed() < self.dir_ttl()
                        && entries
                            .binary_search_by(|(n, _, _)| n.as_str().cmp(name))
                            .is_err()
                    {
                        tracing::debug!(path = %path, "getattr: NEGATIVE from live listing");
                        return Err(ErrorCode::NotFound.into());
                    }
                }
            }
            // The warm tier answers BOTH halves where the live listing above
            // answers only the negative. The positive matters here because
            // warm listings have no TTL: an attr-cache expiry must not push a
            // stat of a token-proven entry back onto the wire, or a remount
            // would go cold again five seconds after it went warm.
            if let Some(w) = self.warm.get(&parent) {
                let found = w
                    .binary_search_by(|(n, _)| n.as_str().cmp(name))
                    .ok()
                    .map(|i| w[i].1);
                drop(w);
                return match found {
                    Some(attr) => {
                        self.cache_attr(ino, attr);
                        Ok(attr)
                    }
                    None => {
                        tracing::debug!(path = %path, "getattr: NEGATIVE from warm tier");
                        Err(ErrorCode::NotFound.into())
                    }
                };
            }
        }
        let attr = expect_resp!(self.call(Request::Getattr { path })?, Response::Attr(attr) => attr);
        self.cache_attr(ino, attr);
        Ok(attr)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Result<(u64, Attr), FsError> {
        let dir = self.path_of(parent)?;
        let path = dir.join(name);
        if self.is_overlay(&path) {
            let attr = self.overlay_ref().getattr(&path)?;
            let ino = self.ino.get_or_alloc(path);
            return Ok((ino, attr));
        }
        // A live listing of the parent answers this in BOTH directions. The
        // positive case is what saves the FUSE/kernel backends from one round
        // trip per entry after every readdir; the negative case is the bigger
        // one — the listing is complete, so a name it lacks does not exist,
        // and Windows and every resolver probe missing names relentlessly.
        // Overlay names never reach here (routed above), so absence from the
        // remote listing is the whole answer.
        if let Some(hit) = self.dir_cache.get(&parent) {
            let (entries, when) = &*hit;
            if when.elapsed() < self.dir_ttl() {
                return match entries.binary_search_by(|(n, _, _)| n.as_str().cmp(name)) {
                    Ok(i) => {
                        let (_, ino, attr) = &entries[i];
                        Ok((*ino, *attr))
                    }
                    Err(_) => Err(ErrorCode::NotFound.into()),
                };
            }
        }
        // The warm tier gives the same two answers when no live listing
        // does. Same completeness claim, different proof: the tree token the
        // walker verified at mount, kept honest by events instead of a TTL.
        // Feeding the attr cache on the way out is what lets the open that
        // follows this lookup take the lazy no-server path.
        if let Some(w) = self.warm.get(&dir) {
            let found = w
                .binary_search_by(|(n, _)| n.as_str().cmp(name))
                .ok()
                .map(|i| w[i].1);
            drop(w);
            return match found {
                Some(attr) => {
                    let ino = self.ino.get_or_alloc(path);
                    self.cache_attr(ino, attr);
                    Ok((ino, attr))
                }
                None => Err(ErrorCode::NotFound.into()),
            };
        }
        let attr =
            expect_resp!(self.call(Request::Getattr { path: path.clone() })?, Response::Attr(attr) => attr);
        let ino = self.ino.get_or_alloc(path);
        self.cache_attr(ino, attr);
        Ok((ino, attr))
    }

    /// Full listing. With an overlay: remote entries minus shadowed names,
    /// plus local overlay children of this directory.
    pub fn readdir(&self, ino: u64) -> Result<Vec<(String, u64, Attr)>, FsError> {
        let dir = self.path_of(ino)?;
        if self.is_overlay(&dir) {
            let mut out = Vec::new();
            for (name, attr) in self.overlay_ref().readdir_children(&dir) {
                let child_ino = self.ino.get_or_alloc(dir.join(&name));
                out.push((name, child_ino, attr));
            }
            return Ok(out);
        }
        // Serve the remote half from cache while it is live. The overlay half
        // is merged fresh on every call — it is a local read, so caching it
        // would save microseconds and buy an invalidation problem.
        if let Some(hit) = self.dir_cache.get(&ino) {
            let (entries, when) = &*hit;
            if when.elapsed() < self.dir_ttl() {
                let mut out = entries.clone();
                drop(hit);
                self.merge_overlay_children(&dir, &mut out);
                return Ok(out);
            }
        }
        // The warm tier: a complete listing restored from the metadata
        // snapshot, token-proven at mount and event-busted from then on.
        // Filtered and merged exactly like a wire listing, because the
        // snapshot stores the SERVER's view raw — exclude patterns are client
        // configuration and may differ between the mount that wrote it and
        // this one, so routing belongs to serve time, not save time.
        if let Some(w) = self.warm.get(&dir) {
            let listing = w.clone();
            drop(w);
            let mut out = Vec::with_capacity(listing.len());
            for (name, attr) in listing {
                let child = dir.join(&name);
                if self.shadowed_by_overlay(&child) {
                    continue;
                }
                let child_ino = self.ino.get_or_alloc(child);
                self.cache_attr(child_ino, attr);
                out.push((name, child_ino, attr));
            }
            self.merge_overlay_children(&dir, &mut out);
            return Ok(out);
        }
        // A WIRE listing must not describe a state older than what this
        // client already acknowledged. The write batcher never trips this —
        // it only acks against a live complete listing, which the branches
        // above would have served — but a batched SETATTR acks against a
        // cached attr alone, so a cold listing fetched here could still show
        // the pre-chmod mode. Whatever is pending goes out first; it was due
        // within FLUSH_AGE anyway, and cold listings are already round-trip
        // priced.
        if self.batch.as_ref().is_some_and(|b| !b.is_empty()) {
            self.flush_batch();
        }
        // Captured BEFORE the first page goes out. Paging is several round
        // trips, and anything that invalidates a listing in that window makes
        // what comes back a description of the past.
        let epoch_at_start = self.dir_epoch.load(Ordering::Acquire);
        let mut remote = Vec::new();
        let mut cursor = 0u64;
        loop {
            let (entries, next_cursor) = expect_resp!(
                self.call(Request::Readdir { path: dir.clone(), cursor })?,
                Response::Dir { entries, next_cursor } => (entries, next_cursor)
            );
            for e in entries {
                let child = dir.join(&e.name);
                if self.shadowed_by_overlay(&child) {
                    continue;
                }
                let child_ino = self.ino.get_or_alloc(child);
                self.cache_attr(child_ino, e.attr);
                remote.push((e.name, child_ino, e.attr));
            }
            match next_cursor {
                Some(c) => cursor = c,
                None => break,
            }
        }
        // Cached listings are kept name-sorted (byte order): every lookup
        // against them binary-searches, and patch_parent_dir splices with
        // partition_point on the same assumption. Modern agents serve pages
        // sorted already, so this is a near-free merge pass — but the
        // invariant is enforced HERE, not trusted to the peer.
        remote.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        // Only cache a listing that still describes the present. A mutation
        // during the fetch means this result may already be wrong, and a wrong
        // COMPLETE listing does not merely go stale — it answers NotFound for
        // a file that exists. Skipping the insert costs one uncached readdir.
        if self.dir_epoch.load(Ordering::Acquire) == epoch_at_start {
            self.dir_cache.insert(ino, (remote.clone(), Instant::now()));
        }
        let mut out = remote;
        self.merge_overlay_children(&dir, &mut out);
        Ok(out)
    }

    /// Append this directory's overlay children — always read live, never
    /// cached; see the field comment on `dir_cache`.
    fn merge_overlay_children(&self, dir: &RelPath, out: &mut DirListing) {
        if let Some(ov) = &self.overlay {
            for (name, attr) in ov.readdir_children(dir) {
                let child = dir.join(&name);
                if self.lives_in_overlay(&child) {
                    let child_ino = self.ino.get_or_alloc(child);
                    out.push((name, child_ino, attr));
                }
            }
        }
    }

    pub fn open(&self, ino: u64, flags: OpenFlags) -> Result<(u64, Attr), FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            return self.overlay_ref().open(&path, flags);
        }
        // A path the batcher still owes the server has no remote truth to
        // open yet; the queue lands first, and its failures surface here
        // rather than as a mystery later.
        if self.batch.as_ref().is_some_and(|b| b.involves(&path)) {
            self.barrier_for(&path)?;
        }
        // Read-only, and the cache already holds this file at the version the
        // last listing reported? Then the server has nothing to add. Skipping
        // the round trip here is what makes browsing a remote tree bearable:
        // `ls`, git and Explorer's property handlers all open files they never
        // read a byte of, and each of those opens was costing a full RTT.
        //
        // The freshness test is the SAME one the answer would have been fed
        // through — `fresh_for` compares size, mtime and version — applied to
        // the attribute the readdir already cached instead of to one fetched
        // again. That attribute is at most one attr-TTL old and the event stream
        // invalidates it sooner, which is the freshness contract every other
        // read on this mount already runs under.
        //
        // Writes, truncation, append and O_EXCL all still go to the server: a
        // cached blob says what the file WAS, which is no basis for changing
        // it.
        if flags.read && !flags.write && !flags.truncate && !flags.append && !flags.excl {
            if let Some(attr) = self.cached_attr_fresh(ino) {
                if self.cache.as_ref().is_some_and(|c| c.fresh_for(&path, &attr)) {
                    let fh = LAZY_FH_BIT | self.next_lazy_fh.fetch_add(1, Ordering::Relaxed);
                    self.open_files.insert(
                        fh,
                        OpenState {
                            path,
                            flags,
                            server_fh: AtomicU64::new(NO_SERVER_FH),
                            cache_ok: AtomicBool::new(true),
                            wrote: AtomicBool::new(false),
                            ra: ReadAhead::new(),
                            lock: std::sync::Mutex::new(Vec::new()),
                            poisoned: AtomicBool::new(false),
                            pending_new: None,
                            blob: std::sync::RwLock::new(None),
                            version: AtomicU64::new(attr.version),
                        },
                    );
                    return Ok((fh, attr));
                }
            }
        }
        // v9+: a plain read-open carries the head of the file back with the
        // handle, folding the open+first-read pair — the residual per-file
        // cost after ReadMany — into one exchange. Only when the auto-cache
        // will not already answer the read locally (cache_ok below), and only
        // for opens that will read: head bytes for a write-only or truncating
        // open would be fetched to be thrown away. `len == 0` spells exactly
        // that on the wire.
        let wants_head = flags.read && !flags.truncate;
        let (fh, attr, head) = if self.conn().proto >= 9 {
            let len = if wants_head { DATA_CHUNK } else { 0 };
            let resp = self.call(Request::OpenRead {
                path: path.clone(),
                flags,
                len,
            })?;
            expect_resp!(resp, Response::OpenedData { fh, attr, data } => (fh, attr, Some(data)))
        } else {
            let resp = self.call(Request::Open {
                path: path.clone(),
                flags,
            })?;
            expect_resp!(resp, Response::Opened { fh, attr } => (fh, attr, None))
        };
        debug_assert!(fh & OVERLAY_FH_BIT == 0, "server fh collides with overlay bit");
        self.cache_attr(ino, attr);
        let cache_ok = self.cache.as_ref().is_some_and(|c| c.fresh_for(&path, &attr));
        let ra = ReadAhead::new();
        // Plant the head where the first read will look. Retained block 0 is
        // exactly what a post-open read at offset 0 consults, so a small file
        // is now open+read in one round trip and read locally after. An empty
        // head is NOT planted: an empty retained block would answer reads at
        // offset 0 with EOF for a file that merely declined to send bytes.
        if let Some(data) = head {
            if !data.is_empty() && !cache_ok {
                ra.retain(0, data);
            }
        }
        self.open_files.insert(
            fh,
            OpenState {
                path,
                flags,
                server_fh: AtomicU64::new(fh),
                cache_ok: AtomicBool::new(cache_ok),
                wrote: AtomicBool::new(false),
                ra,
                lock: std::sync::Mutex::new(Vec::new()),
                poisoned: AtomicBool::new(false),
                pending_new: None,
                blob: std::sync::RwLock::new(None),
                version: AtomicU64::new(attr.version),
            },
        );
        Ok((fh, attr))
    }

    /// Fetch one whole DATA_CHUNK-aligned block on `conn` (async).
    async fn fetch_block(conn: Arc<MuxConnection>, server_fh: u64, block: u64) -> Option<Bytes> {
        let offset = block * DATA_CHUNK as u64;
        match conn
            .request(Request::Read {
                fh: server_fh,
                offset,
                len: DATA_CHUNK,
            })
            .await
        {
            Ok(Ok(Response::Data(data))) => Some(data),
            _ => None,
        }
    }

    /// v11+: change the Windows attribute bits (Hidden/System) on `ino`.
    ///
    /// Masked intents, resolved server-side; the reply's attrs re-patch the
    /// caches so the next stat and the parent's listing tell the new truth.
    /// Overlay-routed paths apply locally through their own metadata (the
    /// overlay file IS the file). A pre-v11 server keeps the historical
    /// accepted-and-dropped behaviour — the request is never sent.
    pub fn set_win_attrs(&self, ino: u64, set: u32, clear: u32) -> Result<Attr, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            // The overlay's local file carries real NTFS bits already;
            // nothing to send anywhere. Serve current attrs back.
            return self.overlay_ref().getattr(&path);
        }
        if self.conn().proto < 11 {
            // The old contract, kept exactly: acknowledged, unpersisted.
            return self.getattr(ino);
        }
        if self.batch.as_ref().is_some_and(|b| b.involves(&path)) {
            self.barrier_for(&path)?;
        }
        let attr = expect_resp!(
            self.call(Request::SetWinAttrs {
                path: path.clone(),
                set,
                clear,
            })?,
            Response::Attr(attr) => attr
        );
        self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        Ok(attr)
    }

    /// Materialize the OPEN pending-new file at `path`, if one exists — the
    /// prologue for operations that need the server to KNOW the path while
    /// its handle is still accumulating locally (rename endpoints). A
    /// no-op for everything else.
    fn materialize_open_pending(&self, path: &RelPath) -> Result<(), FsError> {
        let fh = self
            .open_files
            .iter()
            .find_map(|e| (e.value().path == *path && e.value().pending_new.is_some()).then(|| *e.key()));
        match fh {
            Some(fh) => self.materialize_pending(fh),
            None => Ok(()),
        }
    }

    /// A pending NEW file outgrew the batcher — a non-sequential write, or
    /// size past the cap. Everything queued before it flushes first (order),
    /// then the file takes the classic path: a server create, the buffered
    /// bytes pushed through ordinary writes, and the handle rebound to the
    /// server's. Write-through resumes for this fh from here.
    fn materialize_pending(&self, fh: u64) -> Result<(), FsError> {
        self.flush_batch();
        let (path, buf, mode, cancelled) = {
            let Some(mut e) = self.open_files.get_mut(&fh) else {
                return Err(ErrorCode::BadHandle.into());
            };
            let Some(pending) = e.pending_new.take() else {
                return Ok(());
            };
            let p = pending.into_inner().unwrap();
            (e.path.clone(), p.data, p.mode, p.cancelled)
        };
        if cancelled {
            // Unlinked while open, then written past the cap: the data has
            // nowhere durable to go — POSIX would keep it in the dead inode,
            // which this client cannot fabricate remotely. EIO over silence.
            return Err(ErrorCode::Io.into());
        }
        if let Some(batch) = &self.batch {
            batch.forget(&path);
        }
        let (sfh, _attr) = expect_resp!(
            self.call(Request::Create {
                path: path.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..OpenFlags::default()
                },
                mode,
            })?,
            Response::Opened { fh, attr } => (fh, attr)
        );
        let mut off = 0u64;
        for chunk in buf.chunks(DATA_CHUNK as usize) {
            expect_resp!(
                self.call(Request::Write {
                    fh: sfh,
                    offset: off,
                    data: Bytes::copy_from_slice(chunk),
                    expect_version: None,
                })?,
                Response::Written { .. } | Response::WrittenAttr { .. } => ()
            );
            off += chunk.len() as u64;
        }
        if let Some(e) = self.open_files.get(&fh) {
            e.server_fh.store(sfh, Ordering::Release);
        }
        Ok(())
    }

    /// Seal a pending NEW file into the batcher queue, leaving the handle
    /// alive. Returns the path when something was sealed. Shared by release
    /// (which then drops the handle) and fsync (which then barriers).
    pub fn seal_pending(&self, fh: u64) -> Option<RelPath> {
        let mut e = self.open_files.get_mut(&fh)?;
        let pending = e.pending_new.take()?;
        let path = e.path.clone();
        drop(e);
        let p = pending.into_inner().unwrap();
        if p.cancelled {
            if let Some(b) = &self.batch {
                b.forget(&path);
            }
            return None;
        }
        let batch = self.batch.as_ref()?;
        batch.push(PendingOp::Write {
            path: path.clone(),
            mode: p.mode,
            data: p.data,
        });
        // Queued claim taken; the unsealed one retires.
        batch.forget(&path);
        if batch.wants_flush() {
            self.flush_batch();
        }
        Some(path)
    }

    /// `read` that fills the caller's buffer directly on the WARM paths —
    /// a pending file's local bytes and the mapped blob copy straight into
    /// the buffer the kernel handed the mount, skipping the Vec every warm
    /// read otherwise allocates and copies through. Everything else falls
    /// back to [`Self::read`]: the cold paths are round-trip-dominated and
    /// one extra copy is invisible there. Returns bytes written; 0 at EOF.
    pub fn read_into(&self, fh: u64, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        if fh & OVERLAY_FH_BIT == 0 {
            self.check_poisoned(fh)?;
            if let Some(state) = self.open_files.get(&fh) {
                if let Some(pending) = &state.pending_new {
                    let p = pending.lock().unwrap();
                    let start = (offset as usize).min(p.data.len());
                    let end = (offset.saturating_add(buf.len() as u64) as usize).min(p.data.len());
                    buf[..end - start].copy_from_slice(&p.data[start..end]);
                    return Ok(end - start);
                }
                if self.cache.is_some() && state.cache_ok.load(Ordering::Relaxed) {
                    {
                        let held = state.blob.read().unwrap();
                        if let Some(b) = held.as_ref() {
                            return Ok(b.read_into(offset, buf));
                        }
                    }
                    // No retained mapping yet: map now, serve, retain — the
                    // same lazy path `read` takes. A vanished blob falls
                    // through to the network exactly as it does there.
                    if let Some(cache) = &self.cache {
                        if let Some(b) = cache.map_blob(&state.path) {
                            let n = b.read_into(offset, buf);
                            *state.blob.write().unwrap() = Some(b);
                            return Ok(n);
                        }
                    }
                    state.cache_ok.store(false, Ordering::Relaxed);
                    *state.blob.write().unwrap() = None;
                }
            }
            // The shard guard is out of scope here — `read` re-acquires and
            // may take handles out (documented as unsafe under a held guard).
        }
        let data = self.read(fh, offset, buf.len() as u32)?;
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return self.overlay_ref().read(fh, offset, size);
        }
        self.check_poisoned(fh)?;
        // A pending NEW file's truth is its local buffer.
        if let Some(state) = self.open_files.get(&fh) {
            if let Some(pending) = &state.pending_new {
                let p = pending.lock().unwrap();
                let start = (offset as usize).min(p.data.len());
                let end = (offset.saturating_add(size as u64) as usize).min(p.data.len());
                return Ok(p.data[start..end].to_vec());
            }
        }
        // Auto-cache fast path: serve from the local blob when fresh, through
        // a mapping retained on the fh — see the `blob` field for why.
        if let (Some(cache), Some(state)) = (&self.cache, self.open_files.get(&fh)) {
            if state.cache_ok.load(Ordering::Relaxed) {
                let served = {
                    let held = state.blob.read().unwrap();
                    held.as_ref().map(|b| b.read(offset, size))
                };
                if let Some(data) = served {
                    return Ok(data);
                }
                if let Some(b) = cache.map_blob(&state.path) {
                    let data = b.read(offset, size);
                    *state.blob.write().unwrap() = Some(b);
                    return Ok(data);
                }
                // Blob vanished (eviction race): fall through to the network.
                state.cache_ok.store(false, Ordering::Relaxed);
                *state.blob.write().unwrap() = None;
            }
        }
        // Past the cache, so this read genuinely needs the server. If the open
        // never took a handle out, take one now — this is the eviction-race and
        // partial-blob path, not the common one, and it must not be reached
        // while holding a shard guard.
        if self.server_fh(fh) == NO_SERVER_FH {
            self.server_fh_for_io(fh)?;
        }
        let Some(state) = self.open_files.get(&fh) else {
            return self.read_blocks_direct(fh, offset, size); // untracked fh (walker)
        };
        let server_fh = state.server_fh.load(Ordering::Acquire);
        let prefetch = state.ra.observe(offset, size);
        tracing::trace!(fh, offset, size, prefetch, "mount read");

        // Serve [offset, offset+size) from DATA_CHUNK-aligned blocks:
        // retained copies first (sub-chunk re-reads), then prefetched ones,
        // then concurrent fetches for whatever is left.
        let chunk = DATA_CHUNK as u64;
        let first_block = ReadAhead::block_of(offset);
        // saturating: an absurd offset from a direct-IO caller would otherwise
        // wrap, leaving last_block < first_block and an empty block range that
        // reads as a successful empty result rather than a refusal.
        let last_block = ReadAhead::block_of(offset.saturating_add(size.max(1) as u64 - 1));

        // Top up the window BEFORE waiting on this read's own blocks: the
        // prefetches ride the same connection while we block, so the pipe
        // stays full instead of draining once per kernel read.
        //
        // Bounded at EOF. `missing`'s second argument exists for exactly this
        // and was being handed u64::MAX, which makes its `.min()` a no-op — so
        // a sequential read near the tail of a file fired a full window of
        // requests PAST the end. They cost round trips, and their empty
        // results were then retained, seeding precisely the short blocks that
        // go on to answer a later read as a false EOF.
        //
        // A cold attr cache falls back to unbounded rather than guessing. This
        // bound is a prefetch hint: too low only forgoes readahead, and too
        // high is what the code did before.
        let eof_block_exclusive = self
            .ino
            .ino_of(&state.path)
            .and_then(|ino| self.attr_cache.get(&ino).map(|hit| hit.0.size))
            .map(|size| ReadAhead::block_of(size.saturating_sub(1)) + 1)
            .unwrap_or(u64::MAX);
        if prefetch {
            // Multi-stream: a long cold stream stripes its window across the
            // pool's extra connections — same window depth, more congestion
            // windows under it (see stream_pool.rs). Engaging is async: the
            // first rounds ride the primary while the pool dials, and an
            // empty `lanes` costs nothing. Unknown size (u64::MAX) never
            // engages — no point dialing for what may be two blocks.
            let lanes = match &self.stream_pool {
                Some(pool)
                    if eof_block_exclusive != u64::MAX
                        && eof_block_exclusive.saturating_sub(first_block)
                            >= crate::stream_pool::MIN_BLOCKS_AHEAD =>
                {
                    pool.lanes(&self.rt)
                }
                _ => Vec::new(),
            };
            let total_lanes = lanes.len() as u64 + 1;
            for b in state.ra.missing(last_block + 1, eof_block_exclusive) {
                let lane = (b % total_lanes) as usize;
                let task = if lane == 0 {
                    self.rt.spawn(Self::fetch_block(self.conn(), server_fh, b))
                } else {
                    let entry = lanes[lane - 1].clone();
                    self.rt.spawn(entry.fetch_block(state.path.clone(), b))
                };
                state.ra.put(b, task);
            }
        }

        use std::sync::atomic::Ordering::Relaxed;
        let mut ready: HashMap<u64, Bytes> = HashMap::new();
        let mut need: Vec<u64> = Vec::new();
        for b in first_block..=last_block {
            if let Some(data) = state.ra.retained(b) {
                state.ra.stats.retained_hits.fetch_add(1, Relaxed);
                ready.insert(b, data);
                continue;
            }
            match state.ra.take(b) {
                Some(task) => match self.rt.block_on(task) {
                    Ok(Some(data)) => {
                        state.ra.stats.window_hits.fetch_add(1, Relaxed);
                        ready.insert(b, data);
                    }
                    _ => need.push(b), // prefetch failed (old conn?) — refetch
                },
                None => need.push(b),
            }
        }
        if !need.is_empty() {
            state.ra.stats.sync_fetches.fetch_add(need.len() as u64, Relaxed);
            let conn = self.conn();
            let fetched = self.rt.block_on(async {
                futures::future::join_all(
                    need.iter()
                        .map(|&b| Self::fetch_block(conn.clone(), server_fh, b)),
                )
                .await
            });
            for (b, data) in need.iter().zip(fetched) {
                match data {
                    Some(d) => {
                        ready.insert(*b, d);
                    }
                    None => return Err(ErrorCode::Io.into()),
                }
            }
        }
        // Assemble contiguous bytes from first_block onward; a short block is
        // EOF and ends the file.
        let mut assembled: Vec<u8> = Vec::with_capacity(((last_block - first_block + 1) * chunk) as usize);
        for b in first_block..=last_block {
            // Every block in the range was fetched above or the read already
            // failed — but this runs on a mount dispatcher thread, where a
            // broken invariant must become EIO, never a panic.
            let Some(data) = ready.get(&b) else {
                tracing::error!(block = b, "readahead invariant broken: block missing");
                return Err(ErrorCode::Io.into());
            };
            assembled.extend_from_slice(data);
            if data.len() < DATA_CHUNK as usize {
                break; // EOF inside this block
            }
        }
        let skip = (offset - first_block * chunk) as usize;
        let out = if skip >= assembled.len() {
            Vec::new() // read past EOF
        } else {
            assembled[skip..(skip + size as usize).min(assembled.len())].to_vec()
        };

        // Retain this read's blocks: the next sub-chunk kernel read of the
        // same 128 KiB block must not pay a fresh RTT for bytes we had.
        for (b, data) in ready {
            state.ra.retain(b, data);
        }
        Ok(out)
    }

    /// The pre-readahead read path, kept for fhs we don't track (the cache
    /// walker opens raw server fhs that never enter `open_files`).
    fn read_blocks_direct(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        let end = offset + size as u64;
        let chunks: Vec<(u64, u32)> = {
            let mut v = Vec::new();
            let mut pos = offset;
            while pos < end {
                let want = ((end - pos) as u32).min(DATA_CHUNK);
                v.push((pos, want));
                pos += want as u64;
            }
            v
        };
        let conn = self.conn();
        let responses = self.rt.block_on(async {
            futures::future::join_all(chunks.iter().map(|&(pos, want)| {
                conn.request(Request::Read {
                    fh,
                    offset: pos,
                    len: want,
                })
            }))
            .await
        });
        let mut out = Vec::with_capacity(size as usize);
        for (resp, &(_, want)) in responses.into_iter().zip(&chunks) {
            let chunk = expect_resp!(resp??, Response::Data(chunk) => chunk);
            let got = chunk.len() as u32;
            out.extend_from_slice(&chunk);
            if got < want {
                break; // EOF inside this chunk
            }
        }
        Ok(out)
    }

    // --------------------------------------------------------------- writes

    /// Write, discarding the server's post-write attributes. `write_at` is
    /// the same call for backends that can use them.
    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, FsError> {
        self.write_at(fh, offset, data).map(|(n, _)| n)
    }

    /// Write, handing back the file's attributes as the server saw them
    /// immediately afterwards — when the negotiated protocol carried them.
    ///
    /// `None` means "they did not come with the reply": a v4-or-older server,
    /// or a write routed to the local overlay. Callers that fill a kernel
    /// stat structure right after a write want the `Some` case; that is the
    /// Getattr round-trip which used to follow every write.
    ///
    /// Either way the attribute cache is left CORRECT for this path — either
    /// refreshed from the reply or, with nothing to refresh it with, dropped.
    pub fn write_at(&self, fh: u64, offset: u64, data: &[u8]) -> Result<(u32, Option<Attr>), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return self.overlay_ref().write(fh, offset, data).map(|n| (n, None));
        }
        self.check_poisoned(fh)?;
        // A pending NEW file accumulates locally while the writes stay
        // sequential and small; the first write that breaks either rule
        // materializes it onto the classic path below.
        if let Some(state) = self.open_files.get(&fh) {
            if let Some(pending) = &state.pending_new {
                let appended = {
                    let mut p = pending.lock().unwrap();
                    if offset as usize == p.data.len() && p.data.len() + data.len() <= PENDING_FILE_MAX {
                        p.data.extend_from_slice(data);
                        Some(p.data.len() as u64)
                    } else {
                        None
                    }
                };
                let path = state.path.clone();
                drop(state);
                match appended {
                    Some(size) => {
                        let ino = self.ino.ino_of(&path);
                        let now = std::time::SystemTime::now();
                        let base = ino.and_then(|i| self.cached_attr_fresh(i));
                        let attr = Attr {
                            size,
                            mtime: now,
                            ..base.unwrap_or(Attr {
                                kind: alloyfs_proto::FileKind::File,
                                size,
                                mtime: now,
                                ctime: now,
                                mode: 0o666,
                                version: 0,
                            })
                        };
                        if let Some(ino) = ino {
                            self.cache_attr(ino, attr);
                        }
                        return Ok((data.len() as u32, Some(attr)));
                    }
                    None => self.materialize_pending(fh)?,
                }
            }
        }
        let server_fh = self.server_fh_for_io(fh)?;
        // The version this write is allowed to overwrite. `None` when the flag
        // is off (the server then never checks) or when we never learned one.
        //
        // It has to be threaded through the chunk loop rather than read once:
        // our OWN write bumps the server's version, so a large write sending
        // the same expectation for every chunk would conflict with itself
        // after the first one.
        let mut expect = if self.detect_conflicts {
            self.open_files
                .get(&fh)
                .map(|s| s.version.load(Ordering::Relaxed))
                .filter(|v| *v != 0)
        } else {
            None
        };
        // The freshest reply's attributes describe the file as it now
        // stands; whichever branch below runs leaves them here for the
        // shared cache maintenance after it.
        let mut fresh: Option<Attr> = None;
        if expect.is_none() && data.len() > DATA_CHUNK as usize {
            // No expectation to thread means the chunks are independent:
            // one fh, disjoint ranges. Sending them one blocking RTT at a
            // time priced a 1 MiB cache-manager write at 8 serial round
            // trips while the read side kept a 32-block window in flight —
            // this is the write side getting the same treatment, with the
            // cold-read fallback's join_all idiom. The failure shape moves
            // with it, deliberately: a connection dying mid-write can now
            // leave holes where the serial loop left a clean prefix. A
            // torn write-through was always unspecified; --detect-conflicts
            // keeps the serial loop below.
            let conn = self.conn();
            let replies = self.rt.block_on(async {
                futures::future::join_all(data.chunks(DATA_CHUNK as usize).enumerate().map(|(i, chunk)| {
                    conn.request(Request::Write {
                        fh: server_fh,
                        offset: offset + (i as u64) * u64::from(DATA_CHUNK),
                        data: Bytes::copy_from_slice(chunk),
                        expect_version: None,
                    })
                }))
                .await
            });
            // The server may process the chunks in any order, but versions
            // are monotonic per path — the highest version marks the reply
            // that observed the youngest state among ours. One guard on its
            // attrs: an earlier-processed chunk's size must never be served
            // as final, so size is raised to at least this write's own end
            // (concurrent growth by another writer can only agree or
            // exceed; nothing here truncates).
            let mut latest_version = 0u64;
            for (resp, chunk) in replies.into_iter().zip(data.chunks(DATA_CHUNK as usize)) {
                let (n, new_version, attr) = match resp?? {
                    Response::Written { n, new_version, .. } => (n, new_version, None),
                    Response::WrittenAttr { n, attr } => (n, attr.version, Some(attr)),
                    other => {
                        tracing::error!(?other, "unexpected response variant");
                        return Err(ErrorCode::Io.into());
                    }
                };
                if (n as usize) < chunk.len() {
                    // The agent writes whole chunks or errors; a short count
                    // would leave a mid-buffer hole no retry here can see.
                    tracing::error!(fh, n, want = chunk.len(), "server short-wrote a chunk");
                    return Err(ErrorCode::Io.into());
                }
                if new_version > latest_version {
                    latest_version = new_version;
                    fresh = attr;
                }
            }
            fresh = fresh.map(|mut a| {
                a.size = a.size.max(offset + data.len() as u64);
                a
            });
            if let Some(state) = self.open_files.get(&fh) {
                state.version.store(latest_version, Ordering::Relaxed);
            }
        } else {
            let mut pos = 0usize;
            while pos < data.len() {
                let chunk = &data[pos..(pos + DATA_CHUNK as usize).min(data.len())];
                let written = match self.call(Request::Write {
                    fh: server_fh,
                    offset: offset + pos as u64,
                    data: Bytes::copy_from_slice(chunk),
                    expect_version: expect,
                }) {
                    Ok(resp) => resp,
                    // A conflict is a refusal now, not a flag on a write that
                    // has already happened: nothing was written for this
                    // chunk. Earlier chunks of a large write may have landed,
                    // which is the same partial-write hazard any interrupted
                    // write-through has — worth logging the offset so it is
                    // diagnosable.
                    Err(FsError::Remote(ErrorCode::Conflict)) => {
                        tracing::warn!(
                            fh,
                            offset = offset + pos as u64,
                            bytes_already_written = pos,
                            "refused: the file changed on another machine (--detect-conflicts)"
                        );
                        return Err(ErrorCode::Conflict.into());
                    }
                    Err(e) => return Err(e),
                };
                // Both shapes are legal: v5+ servers answer with the
                // attributes, everything older with the byte count and
                // version alone. The version means the same thing in both —
                // `Attr::version` IS what `Written::new_version` carried.
                let (n, new_version) = match written {
                    Response::Written { n, new_version, .. } => {
                        fresh = None;
                        (n, new_version)
                    }
                    Response::WrittenAttr { n, attr } => {
                        let version = attr.version;
                        fresh = Some(attr);
                        (n, version)
                    }
                    other => {
                        tracing::error!(?other, "unexpected response variant");
                        return Err(ErrorCode::Io.into());
                    }
                };
                if let Some(state) = self.open_files.get(&fh) {
                    state.version.store(new_version, Ordering::Relaxed);
                }
                if expect.is_some() {
                    expect = Some(new_version);
                }
                pos += n as usize;
                if n == 0 {
                    return Err(ErrorCode::Io.into());
                }
            }
        }
        // Our own writes never come back as events (server strips
        // self-origin), so cache coherence is synchronous, right here.
        if let Some(state) = self.open_files.get(&fh) {
            state.wrote.store(true, Ordering::Relaxed);
            self.mark_path_written(
                &state.path,
                fresh.zip(self.ino.ino_of(&state.path)).map(|(a, i)| (i, a)),
            );
            // Size, mtime and version all just changed. With the reply
            // carrying them the cached attr is REPLACED rather than dropped,
            // and the next stat of this file — which every mount does
            // immediately, to fill the write's own reply — is a memory hit
            // instead of a Getattr. Without them the entry has to go: serving
            // a pre-write size would be worse than paying for the round-trip.
            match (fresh, self.ino.ino_of(&state.path)) {
                (Some(attr), Some(ino)) => self.cache_attr(ino, attr),
                (None, Some(ino)) => {
                    self.invalidate_attr(ino);
                }
                (_, None) => {} // never stat'ed through this mount: nothing cached
            }
        }
        Ok((data.len() as u32, fresh))
    }

    /// Drain the write batcher to the server and apply every outcome:
    /// server attrs re-patch what the optimistic ack guessed, refusals
    /// restore the caches to the server's truth and land in the damage
    /// ledger. No-op without a batcher.
    ///
    /// This is the BARRIER primitive — fsync, flush, rename, unmount, a
    /// cold listing and the lock ops all promise, by returning, that
    /// everything queued before them is on the server. It therefore may
    /// NOT skip on an empty queue: a concurrent flush (the age flusher)
    /// empties the queue the instant it drains, a good while before those
    /// bytes reach the wire, so `is_empty()` reads true during precisely
    /// the window a barrier exists to cover. Taking the flush lock — which
    /// `flush_with` holds across its send — is what makes the promise
    /// true; an uncontended lock costs nothing next to the syscall or
    /// round trip every one of these callers is already making.
    pub(crate) fn flush_batch(&self) {
        let Some(batch) = &self.batch else { return };
        batch.flush_with(
            |req| match self.call(req) {
                Ok(resp) => Ok(resp),
                Err(FsError::Remote(code)) => Err(code),
                Err(FsError::Transport(_)) => Err(ErrorCode::Io),
            },
            |op, outcome, last| match (op, outcome) {
                // Patch only as the path's LAST claim: an older write's attrs
                // landing over a newer acknowledged remove re-inserted files
                // the application had deleted (measured — see flush_with).
                (PendingOp::Write { path, .. }, Ok(Some(attr))) if last => {
                    let ino = self.ino.get_or_alloc(path.clone());
                    self.patch_parent_dir(path, ListingPatch::Upsert(ino, *attr));
                    self.cache_attr(ino, *attr);
                }
                // The server's post-setattr attrs replace the merged local
                // echo — the true server mtime granularity, version, and
                // win bits land here.
                (PendingOp::Setattr { path, .. }, Ok(Some(attr))) if last => {
                    if let Some(ino) = self.ino.ino_of(path) {
                        self.patch_parent_dir(path, ListingPatch::Upsert(ino, *attr));
                        self.cache_attr(ino, *attr);
                    }
                }
                (PendingOp::Setattr { .. }, Ok(_)) => {} // superseded, or no attrs
                (PendingOp::Write { .. }, Ok(_)) => {}   // superseded by a newer op
                (PendingOp::Remove { .. }, Ok(_)) => {}  // patched at enqueue
                (PendingOp::Write { path, .. }, Err(_))
                | (PendingOp::Remove { path, .. }, Err(_))
                | (PendingOp::Setattr { path, .. }, Err(_)) => {
                    self.settle_failures.fetch_add(1, Ordering::Relaxed);
                    if last {
                        // The optimistic ack promised something the server
                        // refused: the caches stop vouching for this path.
                        // With a NEWER op still queued, its settle decides
                        // instead — this outcome is already history.
                        self.invalidate_parent_dir(path);
                        if let Some(ino) = self.ino.ino_of(path) {
                            self.invalidate_attr(ino);
                        }
                    }
                }
            },
        );
    }

    /// Flush the batcher and report what broke for `path` — the fsync
    /// promise: returning Ok means the server has everything this path was
    /// ever acknowledged for.
    pub(crate) fn barrier_for(&self, path: &RelPath) -> Result<(), FsError> {
        let Some(batch) = &self.batch else {
            return Ok(());
        };
        self.flush_batch();
        match batch.take_damage(path) {
            Some(code) => Err(code.into()),
            None => Ok(()),
        }
    }

    /// Does a COMPLETE cached listing of `dir` decide whether `name` exists?
    /// `None` when no listing can vouch — the caller must ask the server.
    fn knows_child_exists(&self, parent: u64, dir: &RelPath, name: &str) -> Option<bool> {
        if let Some(hit) = self.dir_cache.get(&parent) {
            let (entries, when) = &*hit;
            if when.elapsed() < self.dir_ttl() {
                return Some(entries.binary_search_by(|(n, _, _)| n.as_str().cmp(name)).is_ok());
            }
        }
        self.warm
            .get(dir)
            .map(|w| w.binary_search_by(|(n, _)| n.as_str().cmp(name)).is_ok())
    }

    /// Does a COMPLETE cached listing prove `dir` is empty? Removing a
    /// directory optimistically is only honest when the server could not
    /// answer NotEmpty.
    fn knows_dir_empty(&self, path: &RelPath) -> bool {
        if let Some(ino) = self.ino.ino_of(path) {
            if let Some(hit) = self.dir_cache.get(&ino) {
                let (entries, when) = &*hit;
                if when.elapsed() < self.dir_ttl() {
                    return entries.is_empty();
                }
            }
        }
        self.warm.get(path).map(|w| w.is_empty()).unwrap_or(false)
    }

    pub fn create(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        flags: OpenFlags,
    ) -> Result<(u64, u64, Attr), FsError> {
        let dir = self.path_of(parent)?;
        let path = dir.join(name);
        if self.is_overlay(&path) {
            let (fh, attr) = self.overlay_ref().create(&path, flags, mode)?;
            let ino = self.ino.get_or_alloc(path);
            return Ok((ino, fh, attr));
        }
        // The batched fast path: a NEW file acknowledged locally, its bytes
        // headed for one WriteMany entry at release. Engaged only when a
        // COMPLETE cached listing can answer the existence question the
        // server would have — deciding excl on a guess would invent files.
        if let Some(batch) = &self.batch {
            match self.knows_child_exists(parent, &dir, name) {
                Some(true) if flags.excl => return Err(ErrorCode::AlreadyExists.into()),
                Some(false) => {
                    let now = std::time::SystemTime::now();
                    let attr = Attr {
                        kind: alloyfs_proto::FileKind::File,
                        size: 0,
                        mtime: now,
                        ctime: now,
                        mode,
                        version: 0,
                    };
                    let ino = self.ino.get_or_alloc(path.clone());
                    // Claim before patch, for the same settle race remove()
                    // documents.
                    batch.pending_open(&path);
                    self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
                    self.cache_attr(ino, attr);
                    let fh = LAZY_FH_BIT | self.next_lazy_fh.fetch_add(1, Ordering::Relaxed);
                    self.open_files.insert(
                        fh,
                        OpenState {
                            path,
                            flags,
                            server_fh: AtomicU64::new(NO_SERVER_FH),
                            cache_ok: AtomicBool::new(false),
                            wrote: AtomicBool::new(true),
                            ra: ReadAhead::new(),
                            lock: std::sync::Mutex::new(Vec::new()),
                            poisoned: AtomicBool::new(false),
                            blob: std::sync::RwLock::new(None),
                            pending_new: Some(std::sync::Mutex::new(PendingNew {
                                data: Vec::new(),
                                mode,
                                cancelled: false,
                            })),
                            version: AtomicU64::new(0),
                        },
                    );
                    return Ok((ino, fh, attr));
                }
                // Exists (without excl) or unknowable: the classic exchange
                // below answers both correctly.
                _ => {}
            }
        }
        let (fh, attr) = expect_resp!(
            self.call(Request::Create { path: path.clone(), flags, mode })?,
            Response::Opened { fh, attr } => (fh, attr)
        );
        let ino = self.ino.get_or_alloc(path.clone());
        // Self-origin events are stripped server-side, so the pump will never
        // tell us about our own create — the listing is corrected here, with
        // the reply's own attributes. Patched rather than busted: the next
        // create's existence probe stays local (see patch_parent_dir).
        self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        self.open_files.insert(
            fh,
            OpenState {
                path,
                flags,
                server_fh: AtomicU64::new(fh),
                cache_ok: AtomicBool::new(false),
                wrote: AtomicBool::new(true),
                ra: ReadAhead::new(),
                lock: std::sync::Mutex::new(Vec::new()),
                poisoned: AtomicBool::new(false),
                pending_new: None,
                blob: std::sync::RwLock::new(None),
                version: AtomicU64::new(attr.version),
            },
        );
        Ok((ino, fh, attr))
    }

    pub fn mkdir(&self, parent: u64, name: &str, mode: u32) -> Result<(u64, Attr), FsError> {
        let dir = self.path_of(parent)?;
        let path = dir.join(name);
        if self.is_overlay(&path) {
            let attr = self.overlay_ref().mkdir(&path)?;
            let ino = self.ino.get_or_alloc(path);
            return Ok((ino, attr));
        }
        let attr = expect_resp!(self.call(Request::Mkdir { path: path.clone(), mode })?, Response::Attr(attr) => attr);
        let ino = self.ino.get_or_alloc(path.clone());
        self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        // A just-created directory is COMPLETE and empty — seed that listing,
        // because completeness is what lets a create burst into the new
        // directory answer its own existence probes (and take the batched
        // path) without a wire readdir first.
        self.dir_cache.insert(ino, (Vec::new(), Instant::now()));
        Ok((ino, attr))
    }

    pub fn unlink(&self, parent: u64, name: &str) -> Result<(), FsError> {
        self.remove(parent, name, false)
    }

    pub fn rmdir(&self, parent: u64, name: &str) -> Result<(), FsError> {
        self.remove(parent, name, true)
    }

    fn remove(&self, parent: u64, name: &str, dir: bool) -> Result<(), FsError> {
        let parent_path = self.path_of(parent)?;
        let path = parent_path.join(name);
        if self.is_overlay(&path) {
            return if dir {
                self.overlay_ref().rmdir(&path)
            } else {
                self.overlay_ref().unlink(&path)
            };
        }
        // The batched fast path: a removal a complete listing can vouch for
        // acknowledges locally and rides RemoveMany. Directories need their
        // own emptiness proven too, or the server's NotEmpty would arrive
        // after this ack already said gone.
        if let Some(batch) = &self.batch {
            if self.knows_child_exists(parent, &parent_path, name) == Some(true)
                && (!dir || self.knows_dir_empty(&path))
            {
                // A pending-new handle on this path dies with the name: its
                // release must enqueue nothing (the server never heard of
                // the file), and if nothing was ever queued for it, neither
                // is this removal.
                let mut never_reached_server = false;
                for e in self.open_files.iter() {
                    if e.path == path {
                        if let Some(p) = &e.pending_new {
                            p.lock().unwrap().cancelled = true;
                            never_reached_server = true;
                        }
                    }
                }
                if never_reached_server {
                    batch.forget(&path);
                }
                // Claim BEFORE patch: a concurrent flush settling this
                // path's older write computes "am I the last claim" — if the
                // local patch lands in the gap before the claim registers,
                // that settle answers yes and resurrects the entry this
                // removal just erased. Measured as sporadic AlreadyExists on
                // recreates under the age flusher. A file created and deleted
                // entirely inside the ack window enqueues nothing: the server
                // owes nothing and hears nothing.
                let enqueue = !(never_reached_server && batch.queued_count(&path) == 0);
                if enqueue {
                    batch.push(PendingOp::Remove {
                        path: path.clone(),
                        dir,
                    });
                }
                self.patch_parent_dir(&path, ListingPatch::Remove);
                self.bust_warm(&path);
                if let Some(ino) = self.ino.ino_of(&path) {
                    self.invalidate_attr(ino);
                }
                if let Some(cache) = &self.cache {
                    cache.remove(&path);
                }
                if enqueue && batch.wants_flush() {
                    self.flush_batch();
                }
                return Ok(());
            }
        }
        let req = if dir {
            Request::Rmdir { path: path.clone() }
        } else {
            Request::Unlink { path: path.clone() }
        };
        expect_resp!(self.call(req)?, Response::Ok => ());
        self.patch_parent_dir(&path, ListingPatch::Remove);
        // A removed DIRECTORY's own warm listing has to go too: the path can
        // be re-created, and get_or_alloc would hand the new directory its
        // predecessor's listing. (No-op for files — they never key the map.)
        self.bust_warm(&path);
        if let Some(ino) = self.ino.ino_of(&path) {
            self.invalidate_attr(ino);
        }
        if let Some(cache) = &self.cache {
            cache.remove(&path);
        }
        Ok(())
    }

    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        replace: bool,
    ) -> Result<(), FsError> {
        let from = self.path_of(parent)?.join(name);
        let to = self.path_of(newparent)?.join(newname);
        // A rename can touch names whose truth is still queued; ordering
        // requires the queue lands first, and any damage on either endpoint
        // belongs to this operation.
        //
        // Queued is not enough, though: an OPEN pending-new file exists only
        // on its handle — it seals into the queue at first close, so the
        // barrier below cannot land it. bun's atomic save is exactly that
        // shape: write .lock-*.tmp, rename it over the target WHILE THE
        // HANDLE IS STILL OPEN (the POSIX rename-while-open this mount
        // advertises support for), close after. The server had never heard
        // of the tmp, answered NotFound, and bun reported ENOENT for a file
        // it had just written — reproduced 100% with `bun init` on the
        // mount, stranded tmp and all. Materialize open-pending endpoints
        // first; the barrier then handles the queued world as before.
        if self.batch.is_some() {
            self.materialize_open_pending(&from)?;
            self.materialize_open_pending(&to)?;
            self.barrier_for(&from)?;
            self.barrier_for(&to)?;
        }
        match (self.is_overlay(&from), self.is_overlay(&to)) {
            (true, true) => {
                self.overlay_ref().rename(&from, &to, replace)?;
                self.ino.rename(&from, &to);
                Ok(())
            }
            (false, false) => {
                expect_resp!(
                    self.call(Request::Rename { from: from.clone(), to: to.clone(), replace })?,
                    Response::Ok => ()
                );
                // Both listings changed: one lost an entry, one gained it. The
                // renamed directory's OWN cached listing survives — its ino is
                // stable and the names inside it did not move. The warm tier
                // is path-keyed, so it gets no such stability: everything
                // under both old and new paths is forgotten.
                self.invalidate_parent_dir(&from);
                self.invalidate_parent_dir(&to);
                self.warm_forget_subtree(&from);
                self.warm_forget_subtree(&to);
                self.ino.rename(&from, &to);
                if let Some(cache) = &self.cache {
                    cache.rename(&from, &to);
                }
                Ok(())
            }
            // Across the boundary: EXDEV — tools fall back to copy+delete,
            // and each individual op then routes to the right side.
            _ => Err(ErrorCode::CrossDevice.into()),
        }
    }

    pub fn setattr(
        &self,
        ino: u64,
        size: Option<u64>,
        mtime: Option<std::time::SystemTime>,
        mode: Option<u32>,
    ) -> Result<Attr, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            return self.overlay_ref().setattr(&path, size, mtime, mode);
        }
        // v10 batched metadata-only setattr — the archive-extraction shape,
        // a timestamp restore per file, each of which was a full round trip.
        // Size stays write-through (truncation is data, not metadata), and
        // the ack needs a cached attr to merge into: with nothing cached
        // there is nothing honest to answer with, so that case (and an
        // unsealed pending-new file, whose path the server cannot know yet)
        // takes the classic path below.
        if size.is_none() && (mtime.is_some() || mode.is_some()) {
            if let Some(batch) = &self.batch {
                if self.conn().proto >= 10 && !batch.has_open_pending(&path) {
                    if let Some(hit) = self.attr_cache.get(&ino) {
                        let (mut attr, _) = *hit;
                        drop(hit);
                        if let Some(mt) = mtime {
                            attr.mtime = mt;
                        }
                        if let Some(md) = mode {
                            // The win bits ride the high mode bits (v11) and a
                            // kernel chmod never carries them: keep ours, take
                            // the caller's permission bits.
                            attr.mode = (attr.mode & alloyfs_proto::MODE_WIN_MASK)
                                | (md & !alloyfs_proto::MODE_WIN_MASK);
                        }
                        batch.push_setattr(&path, mtime, mode);
                        if batch.wants_flush() {
                            self.flush_batch();
                        }
                        self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
                        self.cache_attr(ino, attr);
                        return Ok(attr);
                    }
                }
            }
        }
        let attr = expect_resp!(
            self.call(Request::Setattr { path: path.clone(), size, mtime, mode })?,
            Response::Attr(attr) => attr
        );
        self.finish_setattr(ino, &path, size, attr)
    }

    /// `setattr` with the readonly bit resolved SERVER-side (v9+).
    ///
    /// Callers that want "make it readonly / writable" without dictating a
    /// full mode — WinFsp's attribute mapping — used to fetch the current
    /// mode and send a computed one: a round trip, plus a race against any
    /// chmod landing between the two. Against a pre-v9 agent this does
    /// exactly that old dance as the fallback, so the caller no longer has to.
    pub fn setattr_readonly(
        &self,
        ino: u64,
        size: Option<u64>,
        mtime: Option<std::time::SystemTime>,
        readonly: bool,
    ) -> Result<Attr, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            let cur = self.overlay_ref().getattr(&path)?;
            let mode = if readonly {
                cur.mode & !0o222
            } else {
                cur.mode | 0o200
            };
            return self.overlay_ref().setattr(&path, size, mtime, Some(mode));
        }
        if self.conn().proto >= 9 {
            let attr = expect_resp!(
                self.call(Request::Setattr2 {
                    path: path.clone(),
                    size,
                    mtime,
                    mode: None,
                    readonly: Some(readonly),
                })?,
                Response::Attr(attr) => attr
            );
            return self.finish_setattr(ino, &path, size, attr);
        }
        // Pre-v9: the read-modify-write this method exists to retire, kept as
        // the compatibility path — claims exactly what the caller always got.
        let cur = self.getattr(ino)?;
        let mode = if readonly {
            cur.mode & !0o222
        } else {
            cur.mode | 0o200
        };
        self.setattr(ino, size, mtime, Some(mode))
    }

    /// The shared tail of every setattr flavour: cache upkeep in one place so
    /// the two wire paths cannot drift.
    fn finish_setattr(
        &self,
        ino: u64,
        path: &RelPath,
        size: Option<u64>,
        attr: Attr,
    ) -> Result<Attr, FsError> {
        // The parent's cached listing carries this entry's attributes; an
        // explicit metadata change is patched into it from the reply.
        self.patch_parent_dir(path, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        if size.is_some() {
            // The patch above already corrected the listings; passing the
            // attrs again keeps this from busting the warm tier it fixed.
            self.mark_path_written(path, Some((ino, attr)));
        }
        Ok(attr)
    }

    pub fn link(&self, target_ino: u64, newparent: u64, newname: &str) -> Result<(u64, Attr), FsError> {
        let target = self.path_of(target_ino)?;
        let dir = self.path_of(newparent)?;
        let link = dir.join(newname);
        match (self.is_overlay(&target), self.is_overlay(&link)) {
            (true, true) => {
                let attr = self.overlay_ref().link(&target, &link)?;
                let ino = self.ino.get_or_alloc(link);
                Ok((ino, attr))
            }
            (false, false) => {
                let attr = expect_resp!(self.call(Request::Link { target, link: link.clone() })?, Response::Attr(attr) => attr);
                let ino = self.ino.get_or_alloc(link.clone());
                self.patch_parent_dir(&link, ListingPatch::Upsert(ino, attr));
                self.cache_attr(ino, attr);
                Ok((ino, attr))
            }
            _ => Err(ErrorCode::CrossDevice.into()),
        }
    }

    /// Rewrite a symlink target that points back into this mount so the
    /// server can store it, leaving anything else untouched.
    ///
    /// Tooling writes absolute targets by habit, and the absolute form is
    /// always local: PowerShell's `New-Item -Target "real.txt"` resolves to
    /// `Y:\dir\real.txt` before the syscall is made, and `ln -s "$(pwd)/x"`
    /// produces `/mnt/alloy/dir/x`. Neither means anything to the server,
    /// which would refuse both as paths outside the export — a correct check
    /// with a useless outcome, since the user meant a path inside it.
    ///
    /// So a target under this mount's own root becomes one relative to the
    /// link's directory, which is what was meant and what stays correct if
    /// the export is later mounted somewhere else. A target absolute
    /// SOMEWHERE ELSE (`C:\Windows`, `/etc`, a UNC share) is passed through
    /// and the server refuses it — those genuinely do leave the export.
    ///
    /// This lives here rather than in a backend because every backend has the
    /// problem; it was found on Windows only because that is where symlinks
    /// were tested first.
    fn localize_target(&self, target: &str, link_dir: &RelPath) -> String {
        match &self.mount_root {
            Some(root) => localize_symlink_target(root, target, link_dir),
            None => target.to_string(),
        }
    }
    /// Create a symlink at `parent/name` pointing at `target`.
    ///
    /// Unlike `link`, only the LINK's location decides where this goes: the
    /// target is opaque text that may not resolve to anything yet, so there is
    /// no second path to route on. A link created in the overlay stays local;
    /// one created on the server is sent there and validated against the
    /// export boundary by the agent.
    pub fn symlink(&self, parent: u64, name: &str, target: &str) -> Result<(u64, Attr), FsError> {
        let dir = self.path_of(parent)?;
        let link = dir.join(name);
        // Before anything else: a target pointing back into this mount is
        // written as an absolute local path by most tooling, and means
        // nothing to the server.
        let target = self.localize_target(target, &dir);
        if self.is_overlay(&link) {
            let attr = self.overlay_ref().symlink(&target, &link)?;
            let ino = self.ino.get_or_alloc(link);
            return Ok((ino, attr));
        }
        self.require_proto(4, "symlink")?;
        let attr = expect_resp!(
            self.call(Request::Symlink {
                target,
                link: link.clone(),
            })?,
            Response::Attr(attr) => attr
        );
        let ino = self.ino.get_or_alloc(link.clone());
        self.patch_parent_dir(&link, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        Ok((ino, attr))
    }

    /// A symlink's target, verbatim as stored.
    pub fn readlink(&self, ino: u64) -> Result<String, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            return self.overlay_ref().readlink(&path);
        }
        self.require_proto(4, "readlink")?;
        Ok(expect_resp!(
            self.call(Request::ReadLink { path })?,
            Response::Target(t) => t
        ))
    }

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
            self.open_files.remove(&fh);
            return;
        }
        let server_fh = self.server_fh(fh);
        if let Some((_, state)) = self.open_files.remove(&fh) {
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

    pub fn statfs(&self) -> Result<(u32, u64, u64), FsError> {
        let out = expect_resp!(
            self.call(Request::Statfs)?,
            Response::Statfs { block_size, blocks, blocks_free } => (block_size, blocks, blocks_free)
        );
        Ok(out)
    }
}
