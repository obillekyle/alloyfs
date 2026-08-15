use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use alloyfs_proto::{Attr, ErrorCode, OpenFlags, RelPath, Request, Response, DATA_CHUNK};
use alloyfs_transport::{MuxConnection, TransportError};
use bytes::Bytes;
use dashmap::DashMap;
use futures::future::BoxFuture;

use crate::autocache::AutoCache;
use crate::overlay::{Overlay, OVERLAY_FH_BIT};
use crate::readahead::ReadAhead;

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// The server answered with an error — maps 1:1 onto an errno/NTSTATUS.
    #[error(transparent)]
    Remote(#[from] ErrorCode),
    /// The connection itself failed — surfaces as EIO on the mount.
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
}

/// Linux errno for a filesystem error, shared by both Linux mount backends.
///
/// FUSE and the kernel-module daemon are two front-ends onto one platform, and
/// each used to carry its own copy of this table. They had already drifted:
/// the kernel backend mapped `NoSuchExport` to ENOENT while FUSE let it fall
/// through to EIO, and only FUSE had tests. Unified on ENOENT — "that export
/// does not exist" is a miss, not a device failure.
///
/// WinFsp keeps its own mapping, and correctly so: NTSTATUS is a genuinely
/// different target, not a second spelling of the same one.
pub fn posix_errno(err: &FsError) -> i32 {
    // Values rather than libc constants: this crate compiles on Windows too,
    // and the kernel daemon's abi module already spells them out for the same
    // reason.
    match err {
        FsError::Remote(code) => match code {
            ErrorCode::NotFound | ErrorCode::NoSuchExport => 2, // ENOENT
            ErrorCode::PermissionDenied => 13,                  // EACCES
            ErrorCode::AlreadyExists => 17,                     // EEXIST
            ErrorCode::NotADirectory => 20,                     // ENOTDIR
            ErrorCode::IsADirectory => 21,                      // EISDIR
            ErrorCode::NotEmpty => 39,                          // ENOTEMPTY
            ErrorCode::InvalidPath => 22,                       // EINVAL
            ErrorCode::BadHandle => 9,                          // EBADF
            ErrorCode::ReadOnly => 30,                          // EROFS
            ErrorCode::WouldBlock => 11,                        // EAGAIN
            ErrorCode::CrossDevice => 18,                       // EXDEV
            // An old server, not a broken disk.
            ErrorCode::VersionMismatch => 95, // EOPNOTSUPP
            _ => 5,                           // EIO
        },
        // The operation may or may not have happened; EIO is the honest answer.
        FsError::Transport(_) => 5,
    }
}

/// Rewrite `target` relative to `link_dir` when it points back into the mount
/// rooted at `mount_root`; otherwise return it unchanged.
///
/// A free function so it can be tested without standing up a connection — the
/// version this replaced lived in the WinFsp backend and had no direct tests
/// at all.
fn localize_symlink_target(mount_root: &str, target: &str, link_dir: &RelPath) -> String {
    let norm = target.replace('\\', "/");
    // Case-insensitive: Windows drive letters and paths are, and a missed
    // match here just means no rewrite — never a wrong one.
    let prefix = format!("{}/", mount_root.replace('\\', "/").trim_end_matches('/'));
    if !norm.to_uppercase().starts_with(&prefix.to_uppercase()) {
        return target.to_string();
    }
    let rest = norm[prefix.len()..].trim_start_matches('/');

    // Walk up out of the link's directory, then back down to the target; both
    // are export-relative, so the shared prefix cancels.
    let from: Vec<&str> = if link_dir.is_root() {
        Vec::new()
    } else {
        link_dir.0.split('/').collect()
    };
    let to: Vec<&str> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('/').collect()
    };
    let shared = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
        .count();
    let mut out: Vec<&str> = vec![".."; from.len() - shared];
    out.extend_from_slice(&to[shared..]);
    if out.is_empty() {
        return ".".to_string(); // the link points at its own directory
    }
    out.join("/")
}

#[cfg(test)]
mod localize_tests {
    use super::*;

    fn dir(s: &str) -> RelPath {
        RelPath(s.to_string())
    }

    /// The case that made this necessary: PowerShell resolves a relative
    /// target to a drive-letter-absolute one before the syscall is made.
    #[test]
    fn a_target_inside_the_mount_becomes_relative() {
        assert_eq!(
            localize_symlink_target("Y:", "Y:\\linktest\\real.txt", &dir("linktest")),
            "real.txt"
        );
        assert_eq!(
            localize_symlink_target("Y:", "Y:\\real.txt", &dir("")),
            "real.txt"
        );
        // Up and back down.
        assert_eq!(
            localize_symlink_target("Y:", "Y:\\real.txt", &dir("sub")),
            "../real.txt"
        );
        assert_eq!(
            localize_symlink_target("Y:", "Y:\\a\\b\\t.txt", &dir("a/c")),
            "../b/t.txt"
        );
    }

    /// The same problem exists on Unix — `ln -s "$(pwd)/real.txt"` — which is
    /// why this does not live in the Windows backend.
    #[test]
    fn a_unix_mountpoint_works_the_same_way() {
        assert_eq!(
            localize_symlink_target("/mnt/alloy", "/mnt/alloy/dir/real.txt", &dir("dir")),
            "real.txt"
        );
        assert_eq!(
            localize_symlink_target("/mnt/alloy/", "/mnt/alloy/real.txt", &dir("sub")),
            "../real.txt"
        );
    }

    /// Absolute somewhere else is left alone — the server refuses it, which is
    /// correct, because it genuinely does leave the export.
    #[test]
    fn targets_outside_the_mount_are_untouched() {
        for t in [
            "C:\\Windows\\system32",
            "/etc/passwd",
            "\\\\server\\share\\x",
            "../already-relative.txt",
            "plain.txt",
        ] {
            assert_eq!(localize_symlink_target("Y:", t, &dir("sub")), t, "{t:?}");
        }
    }

    /// A prefix that only looks like ours must not match: "Y:" should not
    /// swallow "Y:extra" or a different drive.
    #[test]
    fn a_near_miss_prefix_does_not_match() {
        assert_eq!(
            localize_symlink_target("Y:", "Z:/real.txt", &dir("")),
            "Z:/real.txt"
        );
        assert_eq!(
            localize_symlink_target("/mnt/alloy", "/mnt/alloys/x", &dir("")),
            "/mnt/alloys/x"
        );
    }

    #[test]
    fn a_link_to_its_own_directory_is_dot() {
        assert_eq!(localize_symlink_target("Y:", "Y:\\sub", &dir("sub")), ".");
    }
}

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

/// Re-dial an equivalent connection after the current one dies. Built by the
/// CLI from the original mount url (tcp dial or ssh re-spawn).
pub type Dialer = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<Arc<MuxConnection>>> + Send + Sync>;

/// Per-mount client behavior: local overlay excludes + auto-download cache +
/// optional reconnect. Default means every feature is off and RemoteFs
/// behaves exactly as a plain single-connection client.
#[derive(Clone)]
pub struct ClientOptions {
    pub excludes: Vec<String>,
    /// Durable per-host tree: the overlay lives here and losing it loses
    /// files that exist on no server.
    pub data_dir: PathBuf,
    /// Disposable per-host tree: blobs only, safe to delete at any time.
    /// Kept separate from `data_dir` so "clear the cache" can never reach
    /// the overlay.
    pub cache_dir: PathBuf,
    /// Directory name for this export within those trees.
    pub mount_key: String,
    /// None = no explicit choice: a server suggestion (v2+) applies, else
    /// the fallback below. `Some(0)` is an explicit OFF that beats both.
    pub auto_cache_max: Option<u64>,
    pub auto_cache_budget: Option<u64>,
    /// Used when neither the client nor the server chose a value. The
    /// library default is off/512M; the CLI mounts pass 2M/512M.
    pub auto_cache_max_fallback: u64,
    pub auto_cache_budget_fallback: u64,
    pub pins: Vec<String>,
    pub dialer: Option<Dialer>,
    /// Ignore the server's suggested client settings entirely.
    pub no_server_defaults: bool,
    /// Refuse to write over a file another machine changed since this handle
    /// last saw it. Off by default: it trades "your editor's save failed" for
    /// "your colleague's edit vanished", and which of those is worse depends
    /// entirely on what the mount is for.
    pub detect_conflicts: bool,
    /// Where this mount appears locally ("Y:", "/mnt/alloy"). Used only to
    /// rewrite symlink targets that point back into the mount; `None` skips
    /// that entirely, which is right for a client with no mountpoint (sync,
    /// the CLI diagnostics).
    pub mount_root: Option<String>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            excludes: Vec::new(),
            data_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            mount_key: String::new(),
            auto_cache_max: None,
            auto_cache_budget: None,
            auto_cache_max_fallback: 0, // library default: cache off
            auto_cache_budget_fallback: 512 * 1024 * 1024,
            pins: Vec::new(),
            dialer: None,
            no_server_defaults: false,
            detect_conflicts: false,
            mount_root: None,
        }
    }
}

/// Client-side bookkeeping for one open remote handle. Keyed by the fh the
/// KERNEL holds (stable across reconnects); `server_fh` is what the current
/// server session knows and is rewritten by the reconnect supervisor.
pub(crate) struct OpenState {
    pub path: RelPath,
    pub flags: OpenFlags,
    pub server_fh: AtomicU64,
    /// May reads on this fh be served from the auto-cache blob?
    pub cache_ok: AtomicBool,
    /// Did any write happen through this fh (⇒ re-fetch on release)?
    pub wrote: AtomicBool,
    /// Sequential prefetch window for this handle.
    pub ra: ReadAhead,
    /// The advisory lock this fh currently holds (client mirror of server
    /// state) — what the reconnect supervisor replays.
    pub lock: std::sync::Mutex<Option<alloyfs_proto::LockKind>>,
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

        // Config negotiation (protocol v2+): merge the server's suggested
        // client settings under the client's own. Precedence: explicit client
        // value > server suggestion > fallback. Lists are unioned (client
        // entries first); `no_server_defaults` skips the exchange entirely.
        let mut opts = opts;
        if !opts.no_server_defaults && conn.proto >= 2 {
            if let Ok(Ok(Response::MountDefaults {
                exclude,
                pin,
                auto_cache_max,
                auto_cache_budget,
            })) = conn.request(Request::MountDefaults).await
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
        let auto_cache_max = opts.auto_cache_max.unwrap_or(opts.auto_cache_max_fallback);
        let auto_cache_budget = opts.auto_cache_budget.unwrap_or(opts.auto_cache_budget_fallback);

        let overlay = if opts.excludes.is_empty() {
            None
        } else {
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
            Some(ov)
        };

        let cache_enabled = auto_cache_max > 0 || !opts.pins.is_empty();
        let mut fetch_rx = None;
        let cache = if cache_enabled {
            let root = opts.cache_dir.join(&opts.mount_key);
            let manifest = opts.cache_dir.join(format!("{}.manifest.json", opts.mount_key));
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
            fetch_rx = Some(rx);
            Some(Arc::new(cache))
        } else {
            None
        };

        let (epoch_tx, _) = tokio::sync::watch::channel(0u64);
        let fs = Arc::new(Self {
            conn: RwLock::new(conn),
            rt: tokio::runtime::Handle::current(),
            ino: crate::InodeTable::new(),
            root_attr,
            attr_cache: DashMap::new(),
            overlay,
            cache,
            open_files: DashMap::new(),
            export: export.to_string(),
            dialer: opts.dialer.clone(),
            conn_epoch: epoch_tx,
            last_event_seq: AtomicU64::new(0),
            detect_conflicts: opts.detect_conflicts,
            mount_root: opts.mount_root.clone(),
        });

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
        let mut out = Vec::new();
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
                out.push((e.name, child_ino, e.attr));
            }
            match next_cursor {
                Some(c) => cursor = c,
                None => break,
            }
        }
        if let Some(ov) = &self.overlay {
            for (name, attr) in ov.readdir_children(&dir) {
                let child = dir.join(&name);
                if self.lives_in_overlay(&child) {
                    let child_ino = self.ino.get_or_alloc(child);
                    out.push((name, child_ino, attr));
                }
            }
        }
        Ok(out)
    }

    pub fn open(&self, ino: u64, flags: OpenFlags) -> Result<(u64, Attr), FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            return self.overlay_ref().open(&path, flags);
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
                lock: std::sync::Mutex::new(None),
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
        let last_block = ReadAhead::block_of(offset + size.max(1) as u64 - 1);

        // Top up the window BEFORE waiting on this read's own blocks: the
        // prefetches ride the same connection while we block, so the pipe
        // stays full instead of draining once per kernel read.
        if prefetch {
            for b in state.ra.missing(last_block + 1, u64::MAX) {
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

    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return self.overlay_ref().write(fh, offset, data);
        }
        self.check_poisoned(fh)?;
        let server_fh = self.server_fh(fh);
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
            let (n, new_version) = expect_resp!(
                written,
                Response::Written { n, new_version, .. } => (n, new_version)
            );
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
        }
        Ok(data.len() as u32)
    }

    /// Invalidate the cache entry, readahead windows, and every open fh's
    /// fast path for `path`.
    fn mark_path_written(&self, path: &RelPath) {
        for entry in self.open_files.iter() {
            if entry.value().path == *path {
                entry.value().cache_ok.store(false, Ordering::Relaxed);
                entry.value().ra.clear(); // prefetched blocks predate the write
            }
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
                lock: std::sync::Mutex::new(None),
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
        let server_fh = self.server_fh(fh);
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
            *state.lock.lock().unwrap() = Some(kind); // server did release-then-take
        }
        Ok(())
    }

    pub fn unlock(&self, fh: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(());
        }
        let server_fh = self.server_fh(fh);
        expect_resp!(self.call(Request::Unlock { fh: server_fh })?, Response::Ok => ());
        if let Some(state) = self.open_files.get(&fh) {
            *state.lock.lock().unwrap() = None;
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
        let server_fh = self.server_fh(fh);
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
        let _ = self.call(Request::Release { fh: server_fh });
    }

    pub fn statfs(&self) -> Result<(u32, u64, u64), FsError> {
        let out = expect_resp!(
            self.call(Request::Statfs)?,
            Response::Statfs { block_size, blocks, blocks_free } => (block_size, blocks, blocks_free)
        );
        Ok(out)
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
            let held = *state.lock.lock().unwrap();
            let req = Request::Open {
                path: state.path.clone(),
                flags: state.flags,
            };
            match new_conn.request(req).await {
                Ok(Ok(Response::Opened { fh, .. })) => {
                    state.server_fh.store(fh, Ordering::Release);
                    reopened += 1;
                    if let Some(kind) = held {
                        if replay_lock(&new_conn, fh, kind).await {
                            locks_restored += 1;
                        } else {
                            locks_lost += 1;
                            state.poisoned.store(true, Ordering::Release);
                            tracing::warn!(
                                path = %state.path,
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
                    if held.is_some() {
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
async fn replay_lock(conn: &Arc<MuxConnection>, fh: u64, kind: alloyfs_proto::LockKind) -> bool {
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        match conn
            .request(Request::Lock {
                fh,
                kind,
                wait: false,
            })
            .await
        {
            Ok(Ok(Response::Ok)) => return true,
            Ok(Err(ErrorCode::WouldBlock)) => continue, // maybe our zombie; retry
            _ => return false,
        }
    }
    false
}
