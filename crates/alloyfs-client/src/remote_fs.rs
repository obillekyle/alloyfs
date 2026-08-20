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
            pump_healthy: std::sync::atomic::AtomicBool::new(false),
            attach_tree_token: AtomicU64::new(attach_token),
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
                    if when.elapsed() < self.dir_ttl() && !entries.iter().any(|(n, _, _)| n == name) {
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
            if when.elapsed() < self.dir_ttl() {
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
                (None, Some(ino)) => self.invalidate_attr(ino),
                (_, None) => {} // never stat'ed through this mount: nothing cached
            }
        }
        Ok((data.len() as u32, fresh))
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
