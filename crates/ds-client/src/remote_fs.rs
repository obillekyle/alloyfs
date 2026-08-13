use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use ds_proto::{Attr, ErrorCode, OpenFlags, RelPath, Request, Response, DATA_CHUNK};
use ds_transport::{MuxConnection, TransportError};

use crate::autocache::AutoCache;
use crate::overlay::{Overlay, OVERLAY_FH_BIT};

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

/// Per-mount client behavior: local overlay excludes + auto-download cache.
/// Default (all empty/zero) means both features are off and RemoteFs behaves
/// exactly as before.
#[derive(Default, Clone)]
pub struct ClientOptions {
    pub excludes: Vec<String>,
    pub data_dir: PathBuf,
    pub mount_key: String,
    pub auto_cache_max: u64,
    pub auto_cache_budget: u64,
    pub pins: Vec<String>,
}

/// Client-side bookkeeping for one open remote handle.
pub(crate) struct OpenState {
    pub path: RelPath,
    /// May reads on this fh be served from the auto-cache blob?
    pub cache_ok: AtomicBool,
    /// Did any write happen through this fh (⇒ re-fetch on release)?
    pub wrote: AtomicBool,
}

pub struct RemoteFs {
    conn: Arc<MuxConnection>,
    rt: tokio::runtime::Handle,
    pub ino: crate::InodeTable,
    pub root_attr: Attr,
    attr_cache: DashMap<u64, (Attr, Instant)>,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) cache: Option<Arc<AutoCache>>,
    pub(crate) open_files: DashMap<u64, OpenState>,
}

impl RemoteFs {
    pub async fn attach(conn: Arc<MuxConnection>, export: &str) -> Result<Arc<Self>, FsError> {
        Self::attach_with(conn, export, ClientOptions::default()).await
    }

    /// Attach with overlay/auto-cache options. Spawns the cache walker,
    /// fetcher, and manifest flusher when the cache is enabled.
    pub async fn attach_with(
        conn: Arc<MuxConnection>,
        export: &str,
        opts: ClientOptions,
    ) -> Result<Arc<Self>, FsError> {
        let root_attr = expect_resp!(
            conn.request(Request::Attach { export: export.into() }).await??,
            Response::AttachOk { root_attr, .. } => root_attr
        );

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

        let cache_enabled = opts.auto_cache_max > 0 || !opts.pins.is_empty();
        let mut fetch_rx = None;
        let cache = if cache_enabled {
            let root = opts.data_dir.join("cache").join(&opts.mount_key);
            let manifest = opts.data_dir.join("cache").join(format!("{}.manifest.json", opts.mount_key));
            let (cache, rx) = AutoCache::load(crate::autocache::AutoCacheConfig {
                max_file_size: opts.auto_cache_max,
                budget: opts.auto_cache_budget.max(1),
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

        let fs = Arc::new(Self {
            conn,
            rt: tokio::runtime::Handle::current(),
            ino: crate::InodeTable::new(),
            root_attr,
            attr_cache: DashMap::new(),
            overlay,
            cache,
            open_files: DashMap::new(),
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

    fn call(&self, req: Request) -> Result<Response, FsError> {
        let out = self.rt.block_on(self.conn.request(req))?;
        Ok(out?)
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

    pub fn conn(&self) -> &Arc<MuxConnection> {
        &self.conn
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
        let attr = expect_resp!(self.call(Request::Getattr { path: path.clone() })?, Response::Attr(attr) => attr);
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
        let (fh, attr) =
            expect_resp!(self.call(Request::Open { path: path.clone(), flags })?, Response::Opened { fh, attr } => (fh, attr));
        debug_assert!(fh & OVERLAY_FH_BIT == 0, "server fh collides with overlay bit");
        self.cache_attr(ino, attr);
        let cache_ok = self.cache.as_ref().is_some_and(|c| c.fresh_for(&path, &attr));
        self.open_files
            .insert(fh, OpenState { path, cache_ok: AtomicBool::new(cache_ok), wrote: AtomicBool::new(false) });
        Ok((fh, attr))
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
        // Wire chunks of one kernel read go out CONCURRENTLY — on a 60 ms
        // link this is one round-trip per megabyte instead of eight.
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
        let responses = self.rt.block_on(async {
            futures::future::join_all(
                chunks
                    .iter()
                    .map(|&(pos, want)| self.conn.request(Request::Read { fh, offset: pos, len: want })),
            )
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
        let mut pos = 0usize;
        while pos < data.len() {
            let chunk = &data[pos..(pos + DATA_CHUNK as usize).min(data.len())];
            let (n, conflict) = expect_resp!(
                self.call(Request::Write {
                    fh,
                    offset: offset + pos as u64,
                    data: bytes::Bytes::copy_from_slice(chunk),
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

    /// Invalidate the cache entry and every open fh's fast path for `path`.
    fn mark_path_written(&self, path: &RelPath) {
        if let Some(cache) = &self.cache {
            cache.invalidate(path);
            for entry in self.open_files.iter() {
                if entry.value().path == *path {
                    entry.value().cache_ok.store(false, Ordering::Relaxed);
                }
            }
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
        self.open_files
            .insert(fh, OpenState { path, cache_ok: AtomicBool::new(false), wrote: AtomicBool::new(true) });
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
            return if dir { self.overlay_ref().rmdir(&path) } else { self.overlay_ref().unlink(&path) };
        }
        let req = if dir { Request::Rmdir { path: path.clone() } } else { Request::Unlink { path: path.clone() } };
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
                let attr =
                    expect_resp!(self.call(Request::Link { target, link: link.clone() })?, Response::Attr(attr) => attr);
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
        expect_resp!(self.call(Request::Lock { fh, kind, wait })?, Response::Ok => ());
        Ok(())
    }

    pub fn unlock(&self, fh: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(());
        }
        expect_resp!(self.call(Request::Unlock { fh })?, Response::Ok => ());
        Ok(())
    }

    pub fn flush(&self, fh: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return self.overlay_ref().flush(fh);
        }
        expect_resp!(self.call(Request::Flush { fh })?, Response::Ok => ());
        Ok(())
    }

    pub fn release(&self, fh: u64) {
        if fh & OVERLAY_FH_BIT != 0 {
            self.overlay_ref().release(fh);
            return;
        }
        if let Some((_, state)) = self.open_files.remove(&fh) {
            if state.wrote.load(Ordering::Relaxed) {
                if let Some(cache) = &self.cache {
                    // The finished file may qualify for (re-)caching now.
                    cache.enqueue_refetch(state.path);
                }
            }
        }
        let _ = self.call(Request::Release { fh });
    }

    pub fn statfs(&self) -> Result<(u32, u64, u64), FsError> {
        let out = expect_resp!(
            self.call(Request::Statfs)?,
            Response::Statfs { block_size, blocks, blocks_free } => (block_size, blocks, blocks_free)
        );
        Ok(out)
    }
}
