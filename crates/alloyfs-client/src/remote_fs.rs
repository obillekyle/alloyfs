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
use crate::metacache::{MetaCache, MetaSnapshot};
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

/// How long a cached attribute may serve reads before we re-ask the server.
/// Event-driven invalidation is the primary freshness mechanism; this bounds
/// staleness if the event stream hiccups.
const ATTR_TTL: Duration = Duration::from_secs(5);

/// How long a cached directory listing may answer without re-asking.
///
/// Same value as [`ATTR_TTL`] and the same freshness contract: events are the
/// real invalidation, the TTL only bounds staleness if the stream hiccups.
/// Kept as its own constant because the two are tunable independently — a
/// listing going stale mis-sorts a directory, an attribute going stale
/// mis-serves a file, and those are not the same severity.
const DIR_TTL: Duration = Duration::from_secs(5);

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

/// One advisory lock a handle holds, in the client's mirror of server state.
///
/// `end` is exclusive, `u64::MAX` meaning "to the end of the file" — the same
/// convention the agent keeps, so nothing has to be reinterpreted when a
/// reconnect replays it. The wire spells that as `len == 0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HeldRange {
    pub owner: u64,
    pub kind: alloyfs_proto::LockKind,
    pub start: u64,
    pub end: u64,
}

impl HeldRange {
    /// Back to the wire's spelling.
    fn wire_len(&self) -> u64 {
        if self.end == u64::MAX {
            0
        } else {
            self.end - self.start
        }
    }
}

/// Exclusive end from the wire's `(start, len)`, where `len == 0` is to-EOF.
pub(crate) fn range_end(start: u64, len: u64) -> u64 {
    if len == 0 {
        u64::MAX
    } else {
        start.saturating_add(len)
    }
}

/// Record that `owner` holds `[start, end)` as `kind`, replacing whatever it
/// held there.
///
/// A trimmed mirror of the agent's carve-then-insert: only this handle's own
/// ranges matter here, since conflicts are the server's business. Keeping the
/// shapes the same is what lets a reconnect replay exactly what the server
/// has, rather than a superset that would claim more than was held.
pub(crate) fn record_lock(
    held: &mut Vec<HeldRange>,
    owner: u64,
    kind: alloyfs_proto::LockKind,
    start: u64,
    end: u64,
) {
    record_unlock(held, owner, start, end);
    held.push(HeldRange {
        owner,
        kind,
        start,
        end,
    });
}

/// Drop `[start, end)` from what `owner` holds, splitting anything it cuts
/// through.
pub(crate) fn record_unlock(held: &mut Vec<HeldRange>, owner: u64, start: u64, end: u64) {
    let mut fragments = Vec::new();
    held.retain(|h| {
        if h.owner != owner || !(h.start < end && start < h.end) {
            return true;
        }
        if h.start < start {
            fragments.push(HeldRange { end: start, ..*h });
        }
        if h.end > end {
            fragments.push(HeldRange { start: end, ..*h });
        }
        false
    });
    held.extend(fragments);
}

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
    /// Server version this handle last saw, for --detect-conflicts. Seeded at
    /// open, advanced by our own writes. 0 = unknown, which never conflicts:
    /// refusing a write because we never learned a version would be a bug
    /// wearing the clothes of a safety feature.
    pub version: AtomicU64,
}

pub struct RemoteFs {
    conn: RwLock<Arc<MuxConnection>>,
    rt: tokio::runtime::Handle,
    pub ino: crate::InodeTable,
    pub root_attr: Attr,
    attr_cache: DashMap<u64, (Attr, Instant)>,
    /// Complete REMOTE listing per directory ino, good for [`DIR_TTL`] or
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
    /// Bumped by every listing invalidation, so a `readdir` that started
    /// before a mutation cannot install its now-stale result after it.
    ///
    /// `readdir` fetches pages over several round trips and then inserts. The
    /// invalidations are a bare `remove`, so the two interleave: mutate,
    /// invalidate, then an in-flight fetch lands a pre-mutation listing and
    /// stamps it fresh. That is worse than staleness — a listing is treated as
    /// COMPLETE, so `lookup` answers a hard NotFound for a file that exists
    /// (and keeps answering it for the rest of DIR_TTL). Comparing this before
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
    /// No TTL, deliberately. DIR_TTL exists because a live listing's
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
    /// Bumped after every successful reconnect; the event pump watches it.
    conn_epoch: tokio::sync::watch::Sender<u64>,
    /// Highest event seq the pump has applied — reconnect resubscribes here.
    pub(crate) last_event_seq: AtomicU64,
    /// Send `expect_version` with writes and refuse to clobber (opt-in).
    detect_conflicts: bool,
    /// Local mountpoint, for symlink target rewriting. See `localize_target`.
    mount_root: Option<String>,
    /// Counter for handles the client invents when an open is answered from
    /// cache. Combined with [`LAZY_FH_BIT`] so it cannot collide with a server
    /// handle or with the overlay's.
    next_lazy_fh: AtomicU64,
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
        let root_attr = expect_resp!(
            conn.request(Request::Attach { export: export.into() }).await??,
            Response::AttachOk { root_attr, .. } => root_attr
        );

        let Negotiated {
            opts,
            auto_cache_max,
            auto_cache_budget,
        } = negotiate_defaults(opts, conn.proto, || async {
            conn.request(Request::MountDefaults).await.ok()?.ok()
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
            conn_epoch: epoch_tx,
            last_event_seq: AtomicU64::new(0),
            detect_conflicts: opts.detect_conflicts,
            mount_root: opts.mount_root.clone(),
            next_lazy_fh: AtomicU64::new(1),
        });
        spawn_background_tasks(&fs, fetch_rx);
        Ok(fs)
    }

    /// Clean shutdown: persist the cache manifest. Call after unmount.
    pub fn shutdown(&self) {
        if let Some(cache) = &self.cache {
            cache.flush_manifest();
            let (n, bytes) = cache.stats();
            tracing::info!(entries = n, bytes, "auto-cache manifest flushed");
        }
    }

    /// The live connection (may change across reconnects — take a snapshot,
    /// never hold it across long waits).
    pub fn conn(&self) -> Arc<MuxConnection> {
        self.conn.read().unwrap().clone()
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

    fn cache_attr(&self, ino: u64, attr: Attr) {
        self.attr_cache.insert(ino, (attr, Instant::now()));
    }

    pub fn invalidate_attr(&self, ino: u64) {
        self.attr_cache.remove(&ino);
    }

    pub fn invalidate_all(&self) {
        self.attr_cache.clear();
        self.dir_cache.clear();
        self.dir_epoch.fetch_add(1, Ordering::Release);
        // Every caller of this is a "we may have missed events" path — lag,
        // resync, reconnect — and missed events are the one thing the warm
        // tier has no TTL to grow out of. It goes entirely, and it stays
        // gone: nothing re-installs it until the next mount re-proves the
        // token.
        self.warm.clear();
        self.warm_epoch.fetch_add(1, Ordering::Release);
    }

    /// Drop the cached listing of `path`'s PARENT. Every mutation that adds,
    /// removes or reshapes an entry calls this with the child's path — the
    /// server strips self-origin events, so our own changes will never arrive
    /// through the pump and the busting has to be synchronous, right here.
    pub(crate) fn invalidate_parent_dir(&self, path: &RelPath) {
        if let Some((parent, _)) = path.split() {
            // The warm tier is path-keyed, so it must not wait on an ino:
            // a warm listing can exist for a directory this session has
            // never allocated a number for.
            self.bust_warm(&parent);
            if let Some(pino) = self.ino.ino_of(&parent) {
                self.dir_cache.remove(&pino);
                self.dir_epoch.fetch_add(1, Ordering::Release);
            }
        }
    }

    /// Drop one directory's warm listing, and note the mutation so a
    /// snapshot install in flight cannot land listings from before it.
    pub(crate) fn bust_warm(&self, dir: &RelPath) {
        self.warm.remove(dir);
        self.warm_epoch.fetch_add(1, Ordering::Release);
    }

    /// Drop the warm listings of `root` and everything under it. For renames:
    /// the warm tier is path-keyed, so a moved directory's own listing (and
    /// its descendants') sit under keys that no longer name anything. The
    /// ino-keyed `dir_cache` keeps a renamed directory's listing — its number
    /// is stable and the names inside did not move — but re-keying warm to
    /// chase that would buy little for the bookkeeping; one cold readdir
    /// after a rename is the cheaper trade.
    pub(crate) fn warm_forget_subtree(&self, root: &RelPath) {
        self.warm.remove(root);
        let prefix = format!("{}/", root.0);
        self.warm.retain(|k, _| !k.0.starts_with(&prefix));
        self.warm_epoch.fetch_add(1, Ordering::Release);
    }

    /// How many directories the warm tier currently answers for. Zero on any
    /// mount that did not adopt a snapshot; what the tests (and a curious
    /// operator) use to tell "walk skipped and metadata warm" from "walk
    /// skipped, metadata cold".
    pub fn warm_dirs(&self) -> usize {
        self.warm.len()
    }

    /// The warm-invalidation epoch right now. The walker captures it BEFORE
    /// asking the server for its tree token, for the same reason `readdir`
    /// captures `dir_epoch` before its first page: the install that follows
    /// a match must not land listings from before an invalidation that
    /// happened while the answer was in flight.
    pub(crate) fn warm_epoch_now(&self) -> u64 {
        self.warm_epoch.load(Ordering::Acquire)
    }

    /// Install the on-disk metadata snapshot into the warm tier, when it
    /// proves out: right token, and nothing invalidated a listing since
    /// `epoch_at_check` was captured. Returns how many directories landed.
    pub(crate) fn adopt_meta_snapshot(&self, token: u64, epoch_at_check: u64) -> usize {
        let Some(meta) = &self.meta else { return 0 };
        let Some(snap) = meta.load_for(token) else { return 0 };
        // Same rule as readdir's insert guard, same direction of error: a
        // mutation observed since the capture means these listings may
        // describe the past, and a skipped install costs a cold first
        // navigation while a stale one answers hard NotFound for a file
        // that exists.
        if self.warm_epoch.load(Ordering::Acquire) != epoch_at_check {
            tracing::debug!("a mutation raced the snapshot install; serving cold instead");
            return 0;
        }
        let n = snap.dirs.len();
        for (dir, listing) in snap.dirs {
            self.warm.insert(RelPath(dir), listing);
        }
        n
    }

    /// Persist a fresh metadata snapshot (walker, on a completed pass).
    pub(crate) fn save_meta_snapshot(&self, snap: &MetaSnapshot) {
        if let Some(meta) = &self.meta {
            meta.save(snap);
        }
    }

    /// Delete the metadata snapshot: it is no longer proven for whatever
    /// happens next. Walk decisions and `ResyncRequired` land here.
    pub(crate) fn discard_meta_snapshot(&self) {
        if let Some(meta) = &self.meta {
            meta.discard();
        }
    }

    /// Throw away everything this handle has prefetched or retained, so the
    /// next read goes back to the server.
    ///
    /// For callers that hold a handle open across remote changes. Closing and
    /// reopening the handle would also work, and is what the kernel daemon
    /// used to do — but a handle can own an advisory lock, and dropping it to
    /// refresh some cached bytes would quietly hand that lock to whoever asked
    /// next. This keeps the handle, and with it the lock.
    pub fn invalidate_read_cache(&self, fh: u64) {
        if let Some(state) = self.open_files.get(&fh) {
            state.ra.clear();
        }
    }

    /// True when the auto-cache walk was skipped this mount because the
    /// export tree token still matched what the cache was last complete at.
    /// A mount with no auto-cache never walks, and reports false.
    pub fn auto_cache_walk_skipped(&self) -> bool {
        self.cache.as_ref().is_some_and(|c| c.walk_skipped())
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

    /// The cached attribute for `ino`, if one is present and inside the TTL.
    ///
    /// Deliberately does NOT fetch on a miss: the callers are the ones deciding
    /// whether they can avoid talking to the server at all, and a helper that
    /// silently round-trips would defeat the whole point of asking.
    fn cached_attr_fresh(&self, ino: u64) -> Option<Attr> {
        let hit = self.attr_cache.get(&ino)?;
        let (attr, when) = *hit;
        (when.elapsed() < ATTR_TTL).then_some(attr)
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
            if when.elapsed() < ATTR_TTL {
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
                    if when.elapsed() < DIR_TTL && !entries.iter().any(|(n, _, _)| n == name) {
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
                let found = w.iter().find(|(n, _)| n == name).map(|(_, a)| *a);
                drop(w);
                return match found {
                    Some(attr) => {
                        self.cache_attr(ino, attr);
                        Ok(attr)
                    }
                    None => Err(ErrorCode::NotFound.into()),
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
            if when.elapsed() < DIR_TTL {
                return match entries.iter().find(|(n, _, _)| n == name) {
                    Some((_, ino, attr)) => Ok((*ino, *attr)),
                    None => Err(ErrorCode::NotFound.into()),
                };
            }
        }
        // The warm tier gives the same two answers when no live listing
        // does. Same completeness claim, different proof: the tree token the
        // walker verified at mount, kept honest by events instead of a TTL.
        // Feeding the attr cache on the way out is what lets the open that
        // follows this lookup take the lazy no-server path.
        if let Some(w) = self.warm.get(&dir) {
            let found = w.iter().find(|(n, _)| n == name).map(|(_, a)| *a);
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
            if when.elapsed() < DIR_TTL {
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
        // Read-only, and the cache already holds this file at the version the
        // last listing reported? Then the server has nothing to add. Skipping
        // the round trip here is what makes browsing a remote tree bearable:
        // `ls`, git and Explorer's property handlers all open files they never
        // read a byte of, and each of those opens was costing a full RTT.
        //
        // The freshness test is the SAME one the answer would have been fed
        // through — `fresh_for` compares size, mtime and version — applied to
        // the attribute the readdir already cached instead of to one fetched
        // again. That attribute is at most ATTR_TTL old and the event stream
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
                            version: AtomicU64::new(attr.version),
                        },
                    );
                    return Ok((fh, attr));
                }
            }
        }
        let (fh, attr) = expect_resp!(self.call(Request::Open { path: path.clone(), flags })?, Response::Opened { fh, attr } => (fh, attr));
        debug_assert!(fh & OVERLAY_FH_BIT == 0, "server fh collides with overlay bit");
        self.cache_attr(ino, attr);
        let cache_ok = self.cache.as_ref().is_some_and(|c| c.fresh_for(&path, &attr));
        self.open_files.insert(
            fh,
            OpenState {
                path,
                flags,
                server_fh: AtomicU64::new(fh),
                cache_ok: AtomicBool::new(cache_ok),
                wrote: AtomicBool::new(false),
                ra: ReadAhead::new(),
                lock: std::sync::Mutex::new(Vec::new()),
                poisoned: AtomicBool::new(false),
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

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return self.overlay_ref().read(fh, offset, size);
        }
        self.check_poisoned(fh)?;
        // Auto-cache fast path: serve from the local blob when fresh.
        if let (Some(cache), Some(state)) = (&self.cache, self.open_files.get(&fh)) {
            if state.cache_ok.load(Ordering::Relaxed) {
                if let Some(data) = cache.read(&state.path, offset, size) {
                    return Ok(data);
                }
                // Blob vanished (eviction race): fall through to the network.
                state.cache_ok.store(false, Ordering::Relaxed);
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
            for b in state.ra.missing(last_block + 1, eof_block_exclusive) {
                let conn = self.conn();
                state
                    .ra
                    .put(b, self.rt.spawn(Self::fetch_block(conn, server_fh, b)));
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
        // The last chunk's reply describes the file as it now stands; earlier
        // chunks' attributes are already superseded by the time the loop ends.
        let mut fresh: Option<Attr> = None;
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
                // A conflict is a refusal now, not a flag on a write that has
                // already happened: nothing was written for this chunk.
                // Earlier chunks of a large write may have landed, which is
                // the same partial-write hazard any interrupted write-through
                // has — worth logging the offset so it is diagnosable.
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
            // Both shapes are legal: v5+ servers answer with the attributes,
            // everything older with the byte count and version alone. The
            // version means the same thing in both — `Attr::version` IS what
            // `Written::new_version` carried.
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
        // Our own writes never come back as events (server strips
        // self-origin), so cache coherence is synchronous, right here.
        if let Some(state) = self.open_files.get(&fh) {
            state.wrote.store(true, Ordering::Relaxed);
            self.mark_path_written(&state.path);
            // Size, mtime and version all just changed. With the reply
            // carrying them the cached attr is REPLACED rather than dropped,
            // and the next stat of this file — which every mount does
            // immediately, to fill the write's own reply — is a memory hit
            // instead of a Getattr. Without them the entry has to go: serving
            // a pre-write size would be worse than paying for the round-trip.
            match (fresh, self.ino.ino_of(&state.path)) {
                (Some(attr), Some(ino)) => self.cache_attr(ino, attr),
                (None, Some(ino)) => self.invalidate_attr(ino),
                (_, None) => {} // never stat'ed through this mount: nothing cached
            }
        }
        Ok((data.len() as u32, fresh))
    }

    /// Invalidate the cache entry, readahead windows, and every open fh's
    /// fast path for `path`.
    /// Drop whatever open handles on `path` have prefetched or retained,
    /// after a change that did not come from this client.
    ///
    /// `mark_path_written` does this for our own writes. Remote changes arrive
    /// through the event pump instead, and nothing did it there: the attr
    /// cache and the listing were busted — so `stat` told the truth — while
    /// the bytes an open handle would serve still came from blocks fetched
    /// before the change.
    ///
    /// The retained blocks are the sharp part. A block retained SHORT because
    /// it was the tail of the file goes on answering after the file grows, and
    /// the read returns zero bytes. The FUSE kernel, seeing an offset inside
    /// i_size, reads that short reply as a hole and ZERO-FILLS the page, so
    /// the application gets NULs rather than an error or the appended data,
    /// for the life of the descriptor. `tail -f` across two machines is the
    /// ordinary way to meet it.
    pub(crate) fn invalidate_open_reads(&self, path: &RelPath) {
        for entry in self.open_files.iter() {
            if entry.value().path == *path {
                entry.value().cache_ok.store(false, Ordering::Relaxed);
                entry.value().ra.clear();
            }
        }
    }

    /// The same, for every open handle: a resync means nothing is trusted.
    pub(crate) fn invalidate_all_open_reads(&self) {
        for entry in self.open_files.iter() {
            entry.value().cache_ok.store(false, Ordering::Relaxed);
            entry.value().ra.clear();
        }
    }

    fn mark_path_written(&self, path: &RelPath) {
        self.invalidate_open_reads(path);
        // The parent's WARM listing carries this file's size and mtime, and
        // our own write will never echo back as an event to bust it. The live
        // dir_cache is left alone here on purpose: its TTL bounds the stale
        // attr to five seconds, which is the trade writes have always made,
        // while a warm listing would serve the pre-write size forever.
        if let Some((parent, _)) = path.split() {
            self.bust_warm(&parent);
        }
        if let Some(cache) = &self.cache {
            cache.invalidate(path);
        }
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
        let (fh, attr) = expect_resp!(
            self.call(Request::Create { path: path.clone(), flags, mode })?,
            Response::Opened { fh, attr } => (fh, attr)
        );
        // Self-origin events are stripped server-side, so the pump will never
        // tell us about our own create — the listing has to be busted here.
        self.invalidate_parent_dir(&path);
        let ino = self.ino.get_or_alloc(path.clone());
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
        self.invalidate_parent_dir(&path);
        let ino = self.ino.get_or_alloc(path);
        self.cache_attr(ino, attr);
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
        let req = if dir {
            Request::Rmdir { path: path.clone() }
        } else {
            Request::Unlink { path: path.clone() }
        };
        expect_resp!(self.call(req)?, Response::Ok => ());
        self.invalidate_parent_dir(&path);
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
        let attr = expect_resp!(
            self.call(Request::Setattr { path: path.clone(), size, mtime, mode })?,
            Response::Attr(attr) => attr
        );
        // The parent's cached listing carries this entry's attributes; an
        // explicit metadata change would otherwise show stale there for a TTL.
        self.invalidate_parent_dir(&path);
        self.cache_attr(ino, attr);
        if size.is_some() {
            self.mark_path_written(&path);
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
                self.invalidate_parent_dir(&link);
                let ino = self.ino.get_or_alloc(link);
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
        self.invalidate_parent_dir(&link);
        let ino = self.ino.get_or_alloc(link);
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

    // ------------------------------------------------------ locks & handles

    pub fn lock(&self, fh: u64, kind: alloyfs_proto::LockKind, wait: bool) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(()); // single-machine data: advisory lock is a no-op
        }
        self.check_poisoned(fh)?;
        let server_fh = self.server_fh_for_io(fh)?;
        let req = Request::Lock {
            fh: server_fh,
            kind,
            wait,
        };
        // A blocking wait may legitimately outlast any fixed timeout: bound
        // it by peer liveness instead (fails fast on disconnect, ~20 s on a
        // wedged-but-open server, forever-patient on a healthy busy one).
        let resp = if wait {
            self.rt.block_on(self.conn().request_keepalive(req))??
        } else {
            self.call(req)?
        };
        expect_resp!(resp, Response::Ok => ());
        if let Some(state) = self.open_files.get(&fh) {
            // Whole file, owner 0 — the coarse lock has no notion of either,
            // and recording it the same shape as a range keeps one replay path.
            let mut held = state.lock.lock().unwrap();
            held.clear(); // server did release-then-take
            record_lock(&mut held, 0, kind, 0, u64::MAX);
        }
        Ok(())
    }

    pub fn unlock(&self, fh: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(());
        }
        let server_fh = self.server_fh_for_io(fh)?;
        expect_resp!(self.call(Request::Unlock { fh: server_fh })?, Response::Ok => ());
        if let Some(state) = self.open_files.get(&fh) {
            state.lock.lock().unwrap().clear();
        }
        Ok(())
    }

    /// v7+: take a byte-range lock. `len == 0` is fcntl's "to the end of the
    /// file".
    pub fn lock_range(
        &self,
        fh: u64,
        owner: u64,
        kind: alloyfs_proto::LockKind,
        start: u64,
        len: u64,
        wait: bool,
    ) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(()); // single-machine data: advisory lock is a no-op
        }
        self.check_poisoned(fh)?;
        self.require_proto(7, "byte-range lock")?;
        let server_fh = self.server_fh_for_io(fh)?;
        let req = Request::LockRange {
            fh: server_fh,
            owner,
            kind,
            start,
            len,
            wait,
        };
        // A blocking wait may legitimately outlast any fixed timeout: bound it
        // by peer liveness instead, as the whole-file path does.
        let resp = if wait {
            self.rt.block_on(self.conn().request_keepalive(req))??
        } else {
            self.call(req)?
        };
        expect_resp!(resp, Response::Ok => ());
        if let Some(state) = self.open_files.get(&fh) {
            let mut held = state.lock.lock().unwrap();
            record_lock(&mut held, owner, kind, start, range_end(start, len));
        }
        Ok(())
    }

    /// v7+: release exactly `[start, len)` and nothing else.
    pub fn unlock_range(&self, fh: u64, owner: u64, start: u64, len: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(());
        }
        self.require_proto(7, "byte-range unlock")?;
        let server_fh = self.server_fh_for_io(fh)?;
        expect_resp!(
            self.call(Request::UnlockRange {
                fh: server_fh,
                owner,
                start,
                len,
            })?,
            Response::Ok => ()
        );
        if let Some(state) = self.open_files.get(&fh) {
            let mut held = state.lock.lock().unwrap();
            record_unlock(&mut held, owner, start, range_end(start, len));
        }
        Ok(())
    }

    /// v7+: `fcntl(F_GETLK)` — what would block this lock, if anything.
    pub fn test_lock(
        &self,
        fh: u64,
        owner: u64,
        kind: alloyfs_proto::LockKind,
        start: u64,
        len: u64,
    ) -> Result<Option<alloyfs_proto::LockConflict>, FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(None); // nothing else can hold overlay-routed data
        }
        self.check_poisoned(fh)?;
        self.require_proto(7, "test lock")?;
        let server_fh = self.server_fh_for_io(fh)?;
        let resp = self.call(Request::TestLock {
            fh: server_fh,
            owner,
            kind,
            start,
            len,
        })?;
        Ok(expect_resp!(resp, Response::LockStatus(c) => c))
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
        let server_fh = self.server_fh_for_io(fh)?;
        expect_resp!(self.call(Request::Flush { fh: server_fh })?, Response::Ok => ());
        Ok(())
    }

    pub fn release(&self, fh: u64) {
        if fh & OVERLAY_FH_BIT != 0 {
            self.overlay_ref().release(fh);
            return;
        }
        let server_fh = self.server_fh(fh);
        if let Some((_, state)) = self.open_files.remove(&fh) {
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

/// A mount's settled configuration: the merged options, plus the two cache
/// sizes with their fallbacks already resolved.
struct Negotiated {
    opts: ClientOptions,
    auto_cache_max: u64,
    auto_cache_budget: u64,
}

/// Fold the server's suggested client settings (protocol v2+) in underneath
/// the client's own.
///
/// Precedence is `explicit client value > server suggestion > fallback`, but
/// what "underneath" means differs per setting:
///
/// * Lists (`excludes`, `pins`) are UNIONED, the client's own entries first.
///   Both sides' patterns name something somebody wanted excluded or pinned,
///   so letting either side replace the other loses a stated intent. This is
///   deliberately unlike the config file's client-defaults merge, where a
///   per-mount list REPLACES the global one — there, both lists were written
///   by the same person, and replacing is how they say "not that, this".
/// * Sizes apply only where the client made no explicit choice, which is why
///   `auto_cache_max: Some(0)` is an OFF switch a server cannot override
///   while `None` is an invitation for it to suggest one.
/// * `no_server_defaults` skips the exchange entirely: `ask` is never
///   invoked, so nothing is sent. Same for a v1 peer, which cannot decode the
///   request variant at all.
///
/// `ask` yielding `None` — a server that refused, a connection that dropped —
/// leaves the client's own configuration exactly as it was.
async fn negotiate_defaults<F, Fut>(mut opts: ClientOptions, proto: u16, ask: F) -> Negotiated
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<Response>>,
{
    if !opts.no_server_defaults && proto >= 2 {
        if let Some(Response::MountDefaults {
            exclude,
            pin,
            auto_cache_max,
            auto_cache_budget,
        }) = ask().await
        {
            let suggested = exclude.len() + pin.len();
            for e in exclude {
                if !opts.excludes.contains(&e) {
                    opts.excludes.push(e);
                }
            }
            for p in pin {
                if !opts.pins.contains(&p) {
                    opts.pins.push(p);
                }
            }
            if opts.auto_cache_max.is_none() {
                opts.auto_cache_max = auto_cache_max;
            }
            if opts.auto_cache_budget.is_none() {
                opts.auto_cache_budget = auto_cache_budget;
            }
            if suggested > 0 || auto_cache_max.is_some() || auto_cache_budget.is_some() {
                tracing::info!(
                    excludes = opts.excludes.len(),
                    pins = opts.pins.len(),
                    "applied server-suggested mount defaults (--no-server-defaults to opt out)"
                );
            }
        }
    }
    Negotiated {
        auto_cache_max: opts.auto_cache_max.unwrap_or(opts.auto_cache_max_fallback),
        auto_cache_budget: opts.auto_cache_budget.unwrap_or(opts.auto_cache_budget_fallback),
        opts,
    }
}

/// The local overlay backing the `--exclude`d paths — built on EVERY mount,
/// user excludes or not.
///
/// It used to be `None` when the user excluded nothing, and that gate quietly
/// disabled the built-in `LOCAL_ARTIFACTS` routing: `Overlay::new` compiles
/// those defaults into its matcher, but a mount with no `--exclude` never
/// built one, so `desktop.ini`, `Thumbs.db` and `.DS_Store` went to the wire
/// — a full round trip per probe, and Explorer probes them in every directory
/// it shows. Measured: 0.6 ms with the overlay against 62 ms without.
///
/// Worse than the latency: those names are server-excluded by default too, so
/// WRITING them (Explorer customising a folder) got `NotFound` back. With the
/// overlay always present they land locally and work.
///
/// The always-on overlay costs nothing until used — `Overlay::new` no longer
/// touches the disk, and the directory materialises on the first write of an
/// excluded path.
fn build_overlay(opts: &ClientOptions) -> Result<Option<Overlay>, FsError> {
    let root = opts.data_dir.join("overlay").join(&opts.mount_key);
    let ov = Overlay::new(root, &opts.excludes).map_err(|e| {
        tracing::error!(error = %e, "overlay init failed");
        ErrorCode::Io
    })?;
    let orphans = ov.orphans();
    if !orphans.is_empty() {
        tracing::warn!(
            ?orphans,
            "overlay contains entries that no longer match any --exclude \
             pattern; they are invisible until the pattern returns"
        );
    }
    Ok(Some(ov))
}

/// Paths the auto-cache has decided to download, drained by the walker.
type FetchQueue = tokio::sync::mpsc::UnboundedReceiver<RelPath>;

/// What `build_auto_cache` hands back: the cache, the walker's fetch queue,
/// and the metadata snapshot — all `None` together when caching is off.
type CachePieces = (Option<Arc<AutoCache>>, Option<FetchQueue>, Option<MetaCache>);

/// The auto-download cache, the walker's fetch queue, and the metadata
/// snapshot. All are `None` unless something asks for a cache: a nonzero size
/// limit, or a pin (which must be held locally whatever the limit says).
fn build_auto_cache(
    opts: &ClientOptions,
    auto_cache_max: u64,
    auto_cache_budget: u64,
) -> Result<CachePieces, FsError> {
    if auto_cache_max == 0 && opts.pins.is_empty() {
        return Ok((None, None, None));
    }
    let root = opts.cache_dir.join(&opts.mount_key);
    let manifest = opts.cache_dir.join(format!("{}.manifest.json", opts.mount_key));
    // The metadata snapshot lives beside the manifest — same directory, same
    // mount key — because it lives under the same trust root: the tree token
    // the manifest records is the only thing that can prove its listings.
    let meta = MetaCache::new(opts.cache_dir.join(format!("{}.metadata.json", opts.mount_key)));
    let (cache, rx) = AutoCache::load(crate::autocache::AutoCacheConfig {
        max_file_size: auto_cache_max,
        budget: auto_cache_budget.max(1),
        pins: opts.pins.clone(),
        root,
        manifest,
    })
    .map_err(|e| {
        tracing::error!(error = %e, "auto-cache init failed");
        ErrorCode::Io
    })?;
    Ok((Some(Arc::new(cache)), Some(rx), Some(meta)))
}

/// Start the long-lived tasks, each only when its feature is configured: the
/// cache walker with its manifest flusher, and the reconnect supervisor.
fn spawn_background_tasks(fs: &Arc<RemoteFs>, fetch_rx: Option<FetchQueue>) {
    if let (Some(cache), Some(rx)) = (fs.cache.clone(), fetch_rx) {
        crate::walker::spawn(fs.clone(), cache.clone(), rx);
        // Manifest flusher: every 30 s when dirty.
        let flusher = cache;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let c = flusher.clone();
                let _ = tokio::task::spawn_blocking(move || c.flush_manifest()).await;
            }
        });
    }
    if fs.dialer.is_some() {
        tokio::spawn(reconnect_supervisor(fs.clone()));
    }
}

/// Reconnect supervisor: when the connection dies, dial a replacement with
/// exponential backoff, re-attach, re-open every live handle on the new
/// session, replay each handle's advisory lock, then swap it in and bump
/// the epoch (which re-triggers the event pump's subscription).
///
/// Lock replay is best-effort by nature: the server freed the old session's
/// locks the moment it saw the disconnect, so a waiting contender may
/// legally win the lock before our replay arrives. When that happens the
/// handle is POISONED (subsequent I/O fails EIO) rather than silently
/// continuing without mutual exclusion — apps handle EIO; none can handle a
/// lock that quietly stopped existing. Even a successful replay means "held
/// again", not "held continuously" — during an asymmetric partition the old
/// session's locks persist up to the ~30 s lease, and between its release
/// and our replay another client may have held and released the lock.
async fn reconnect_supervisor(fs: Arc<RemoteFs>) {
    let dialer = fs.dialer.clone().expect("supervisor spawned only with a dialer");
    loop {
        fs.conn().closed().await;
        tracing::warn!("connection lost; reconnecting");
        let mut delay = Duration::from_millis(500);
        let new_conn = loop {
            match dialer().await {
                Ok(c) => break c,
                Err(e) => {
                    tracing::warn!(error = %e, retry_in = ?delay, "reconnect failed");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(15));
                }
            }
        };
        // Re-attach before swapping so concurrent ops keep failing fast on
        // the dead conn instead of racing a half-initialized one.
        match new_conn
            .request(Request::Attach {
                export: fs.export.clone(),
            })
            .await
        {
            Ok(Ok(Response::AttachOk { .. })) => {}
            other => {
                tracing::error!(?other, "re-attach failed; retrying");
                continue;
            }
        }
        // Re-open live handles on the new session; kernel fhs stay stable,
        // only the server_fh translation changes. A handle's lock replays
        // right after its re-open (it needs the fresh server_fh); all of it
        // happens BEFORE the conn swap so lock state is settled by the time
        // the new connection is observable.
        let mut reopened = 0usize;
        let mut failed = 0usize;
        let mut locks_restored = 0usize;
        let mut locks_lost = 0usize;
        for entry in fs.open_files.iter() {
            let state = entry.value();
            state.ra.clear(); // in-flight blocks belong to the dead conn
                              // A handle the old session never knew about has nothing to restore.
                              // It cannot hold a lock either — taking one materialises it — so
                              // there is no mutual exclusion to have lost. It re-opens on its own
                              // if a read ever escapes the cache.
            if state.server_fh.load(Ordering::Acquire) == NO_SERVER_FH {
                continue;
            }
            let held: Vec<HeldRange> = state.lock.lock().unwrap().clone();
            let req = Request::Open {
                path: state.path.clone(),
                flags: state.flags,
            };
            match new_conn.request(req).await {
                Ok(Ok(Response::Opened { fh, .. })) => {
                    state.server_fh.store(fh, Ordering::Release);
                    reopened += 1;
                    // EVERY range, or the handle is poisoned. Restoring some
                    // of what was held and reporting success would leave the
                    // application believing in exclusion it no longer has,
                    // which is the one outcome this path exists to prevent.
                    if !held.is_empty() {
                        let proto = new_conn.proto;
                        let mut all = true;
                        for h in &held {
                            if !replay_lock(&new_conn, fh, h, proto).await {
                                all = false;
                                break;
                            }
                        }
                        if all {
                            locks_restored += 1;
                        } else {
                            locks_lost += 1;
                            state.poisoned.store(true, Ordering::Release);
                            tracing::warn!(
                                path = %state.path,
                                ranges = held.len(),
                                "lock lost across reconnect; handle poisoned (EIO until reopened)"
                            );
                        }
                    }
                }
                _ => {
                    // File may be gone; subsequent ops get BadHandle → EBADF —
                    // unless it held a lock, which makes the loss a mutual-
                    // exclusion break: poison so it surfaces as EIO.
                    failed += 1;
                    if !held.is_empty() {
                        locks_lost += 1;
                        state.poisoned.store(true, Ordering::Release);
                    }
                }
            }
        }
        *fs.conn.write().unwrap() = new_conn;
        // The server may have restarted (versions reset) or missed changes:
        // trust nothing cached until revalidated.
        fs.invalidate_all();
        if let Some(cache) = &fs.cache {
            cache.mark_all_unverified();
        }
        fs.conn_epoch.send_modify(|e| *e += 1);
        tracing::info!(reopened, failed, locks_restored, locks_lost, "reconnected");
    }
}

/// Re-acquire one advisory lock on the new session. Retries WouldBlock a few
/// times: the server may not have processed the OLD session's disconnect yet,
/// so the first attempts can lose to our own zombie's still-held lock.
///
/// Replays through whichever shape the NEW connection negotiated. A reconnect
/// can land on a different server, so a range taken at v7 may have to be
/// replayed against a v6 peer — where the honest thing is to coarsen it,
/// claiming more than was held rather than less.
async fn replay_lock(conn: &Arc<MuxConnection>, fh: u64, held: &HeldRange, proto: u16) -> bool {
    let req = if proto >= 7 {
        Request::LockRange {
            fh,
            owner: held.owner,
            kind: held.kind,
            start: held.start,
            len: held.wire_len(),
            wait: false,
        }
    } else {
        Request::Lock {
            fh,
            kind: held.kind,
            wait: false,
        }
    };
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        match conn.request(req.clone()).await {
            Ok(Ok(Response::Ok)) => return true,
            Ok(Err(ErrorCode::WouldBlock)) => continue, // maybe our zombie; retry
            _ => return false,
        }
    }
    false
}

/// Mount-defaults negotiation. Every rule here decides what a mount is
/// actually configured with, and each one has a plausible-looking wrong
/// answer — a suggestion that overrides an explicit `0`, a list that replaces
/// instead of unions, an opt-out that still sends the request — so they are
/// pinned individually rather than left to an end-to-end mount test.
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn suggests(
        exclude: &[&str],
        pin: &[&str],
        auto_cache_max: Option<u64>,
        auto_cache_budget: Option<u64>,
    ) -> Response {
        Response::MountDefaults {
            exclude: exclude.iter().map(|s| (*s).to_string()).collect(),
            pin: pin.iter().map(|s| (*s).to_string()).collect(),
            auto_cache_max,
            auto_cache_budget,
        }
    }

    /// Negotiate against a v2 server that answers with `resp`.
    async fn against(opts: ClientOptions, resp: Response) -> Negotiated {
        negotiate_defaults(opts, 2, move || async move { Some(resp) }).await
    }

    /// An explicit client value wins, including the explicit OFF: `Some(0)`
    /// means "no auto-cache on this mount" and an eager server must not talk
    /// the client back into one.
    #[tokio::test]
    async fn explicit_client_values_beat_the_server_suggestion() {
        let opts = ClientOptions {
            auto_cache_max: Some(0),
            auto_cache_budget: Some(4096),
            auto_cache_max_fallback: 111,
            auto_cache_budget_fallback: 222,
            ..ClientOptions::default()
        };
        let out = against(opts, suggests(&[], &[], Some(9_999_999), Some(8_888_888))).await;
        assert_eq!(out.auto_cache_max, 0);
        assert_eq!(out.auto_cache_budget, 4096);
    }

    /// No explicit choice: the server's suggestion outranks the fallback.
    #[tokio::test]
    async fn the_server_suggestion_beats_the_fallback() {
        let opts = ClientOptions {
            auto_cache_max_fallback: 111,
            auto_cache_budget_fallback: 222,
            ..ClientOptions::default()
        };
        let out = against(opts, suggests(&[], &[], Some(4096), Some(8192))).await;
        assert_eq!(out.auto_cache_max, 4096);
        assert_eq!(out.auto_cache_budget, 8192);
    }

    /// Neither side chose: the fallback applies. Suggesting one size and not
    /// the other must not drag the unsuggested one along.
    #[tokio::test]
    async fn the_fallback_applies_where_neither_side_chose() {
        let opts = ClientOptions {
            auto_cache_max_fallback: 111,
            auto_cache_budget_fallback: 222,
            ..ClientOptions::default()
        };
        let out = against(opts, suggests(&[], &[], Some(4096), None)).await;
        assert_eq!(out.auto_cache_max, 4096);
        assert_eq!(out.auto_cache_budget, 222);
    }

    /// Lists are UNIONED, not replaced, with the client's own entries first —
    /// a server suggestion never costs the client a pattern it asked for, and
    /// never silently drops one the server asked for either. Duplicates
    /// collapse rather than accumulating on every mount.
    #[tokio::test]
    async fn lists_are_unioned_with_the_clients_entries_first() {
        let opts = ClientOptions {
            excludes: vec!["mine/**".into(), "shared/**".into()],
            pins: vec!["*.mine".into()],
            ..ClientOptions::default()
        };
        let out = against(
            opts,
            suggests(&["shared/**", "theirs/**"], &["*.mine", "*.theirs"], None, None),
        )
        .await;
        assert_eq!(out.opts.excludes, ["mine/**", "shared/**", "theirs/**"]);
        assert_eq!(out.opts.pins, ["*.mine", "*.theirs"]);
    }

    /// `no_server_defaults` opts out of ASKING, not merely of applying the
    /// answer: the request must never be sent.
    #[tokio::test]
    async fn no_server_defaults_skips_the_exchange_entirely() {
        let asked = Cell::new(false);
        let opts = ClientOptions {
            excludes: vec!["mine/**".into()],
            no_server_defaults: true,
            auto_cache_max_fallback: 111,
            ..ClientOptions::default()
        };
        let out = negotiate_defaults(opts, 2, || async {
            asked.set(true);
            Some(suggests(&["theirs/**"], &[], Some(9_999_999), None))
        })
        .await;
        assert!(!asked.get(), "the request must not be sent at all");
        assert_eq!(out.opts.excludes, ["mine/**"]);
        assert_eq!(out.auto_cache_max, 111);
    }

    /// A v1 peer cannot decode the request variant, so it is never sent one.
    #[tokio::test]
    async fn a_v1_server_is_never_asked() {
        let asked = Cell::new(false);
        let out = negotiate_defaults(ClientOptions::default(), 1, || async {
            asked.set(true);
            Some(suggests(&["theirs/**"], &[], Some(9_999_999), None))
        })
        .await;
        assert!(!asked.get(), "the request must not be sent at all");
        assert!(out.opts.excludes.is_empty());
    }

    /// A refused or dropped exchange is not a configuration change: the
    /// client keeps its own values and its own fallbacks.
    #[tokio::test]
    async fn an_unanswered_exchange_leaves_the_client_configuration_alone() {
        let opts = ClientOptions {
            excludes: vec!["mine/**".into()],
            auto_cache_max_fallback: 111,
            auto_cache_budget_fallback: 222,
            ..ClientOptions::default()
        };
        let out = negotiate_defaults(opts, 2, || async { None }).await;
        assert_eq!(out.opts.excludes, ["mine/**"]);
        assert_eq!(out.auto_cache_max, 111);
        assert_eq!(out.auto_cache_budget, 222);
    }
}
