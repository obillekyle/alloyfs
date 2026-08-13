use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use ds_proto::{Attr, ErrorCode, OpenFlags, RelPath, Request, Response, DATA_CHUNK};
use ds_transport::{MuxConnection, TransportError};
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
    pub data_dir: PathBuf,
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
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            excludes: Vec::new(),
            data_dir: PathBuf::new(),
            mount_key: String::new(),
            auto_cache_max: None,
            auto_cache_budget: None,
            auto_cache_max_fallback: 0, // library default: cache off
            auto_cache_budget_fallback: 512 * 1024 * 1024,
            pins: Vec::new(),
            dialer: None,
            no_server_defaults: false,
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
            let root = opts.data_dir.join("cache").join(&opts.mount_key);
            let manifest = opts
                .data_dir
                .join("cache")
                .join(format!("{}.manifest.json", opts.mount_key));
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
        let first = self.rt.block_on(conn.request(req.clone()));
        match first {
            Err(TransportError::Closed) if retryable => {
                let now = self.conn();
                if !Arc::ptr_eq(&conn, &now) && !now.is_closed() {
                    return Ok(self.rt.block_on(now.request(req))??);
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

        // Serve [offset, offset+size) from DATA_CHUNK-aligned blocks: consume
        // prefetched ones, fetch the rest concurrently.
        let chunk = DATA_CHUNK as u64;
        let first_block = ReadAhead::block_of(offset);
        let last_block = ReadAhead::block_of(offset + size.max(1) as u64 - 1);
        let mut ready: HashMap<u64, Bytes> = HashMap::new();
        let mut need: Vec<u64> = Vec::new();
        for b in first_block..=last_block {
            match state.ra.take(b) {
                Some(task) => match self.rt.block_on(task) {
                    Ok(Some(data)) => {
                        ready.insert(b, data);
                    }
                    _ => need.push(b), // prefetch failed (old conn?) — refetch
                },
                None => need.push(b),
            }
        }
        if !need.is_empty() {
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
            let data = ready.get(&b).expect("all needed blocks fetched");
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

        // Top up the prefetch window behind the reader's back.
        if prefetch {
            for b in state.ra.missing(last_block + 1, u64::MAX) {
                let conn = self.conn();
                state
                    .ra
                    .put(b, self.rt.spawn(Self::fetch_block(conn, server_fh, b)));
            }
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
        let server_fh = self.server_fh(fh);
        let mut pos = 0usize;
        while pos < data.len() {
            let chunk = &data[pos..(pos + DATA_CHUNK as usize).min(data.len())];
            let (n, conflict) = expect_resp!(
                self.call(Request::Write {
                    fh: server_fh,
                    offset: offset + pos as u64,
                    data: Bytes::copy_from_slice(chunk),
                    expect_version: None,
                })?,
                Response::Written { n, conflict, .. } => (n, conflict)
            );
            if conflict {
                tracing::warn!(fh, "server reported concurrent modification");
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

    // ------------------------------------------------------ locks & handles

    pub fn lock(&self, fh: u64, kind: ds_proto::LockKind, wait: bool) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(()); // single-machine data: advisory lock is a no-op
        }
        let server_fh = self.server_fh(fh);
        expect_resp!(self.call(Request::Lock { fh: server_fh, kind, wait })?, Response::Ok => ());
        Ok(())
    }

    pub fn unlock(&self, fh: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(());
        }
        let server_fh = self.server_fh(fh);
        expect_resp!(self.call(Request::Unlock { fh: server_fh })?, Response::Ok => ());
        Ok(())
    }

    pub fn flush(&self, fh: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return self.overlay_ref().flush(fh);
        }
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
/// session, then swap it in and bump the epoch (which re-triggers the event
/// pump's subscription). Advisory locks do NOT survive a reconnect —
/// documented behavior, logged when it happens.
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
        // only the server_fh translation changes.
        let mut reopened = 0usize;
        let mut failed = 0usize;
        for entry in fs.open_files.iter() {
            let state = entry.value();
            state.ra.clear(); // in-flight blocks belong to the dead conn
            let req = Request::Open {
                path: state.path.clone(),
                flags: state.flags,
            };
            match new_conn.request(req).await {
                Ok(Ok(Response::Opened { fh, .. })) => {
                    state.server_fh.store(fh, Ordering::Release);
                    reopened += 1;
                }
                _ => {
                    // File may be gone; subsequent ops get BadHandle → EBADF.
                    failed += 1;
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
        tracing::info!(reopened, failed, "reconnected (locks do not survive reconnects)");
    }
}
