use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use alloyfs_common::{attr_from_metadata, read_fully, set_mode, write_fully, ExcludeSet, OrCode};
use alloyfs_proto::{
    DirEntry, ErrorCode, EventKind, FsEvent, OpenFlags, RelPath, Request, Response, DATA_CHUNK,
};
use alloyfs_transport::{EventPusher, RequestHandler};
use dashmap::DashMap;

/// Max directory entries per Readdir response; clients page with the cursor.
const READDIR_PAGE: usize = 1024;

pub struct Export {
    pub name: String,
    /// Canonicalized at startup; the anchor for every path-escape check.
    pub root: PathBuf,
    pub read_only: bool,
    /// Per-file version counters, bumped on every mutation through alloyfs.
    /// Advisory: files changed directly on the server get their bump from the
    /// watcher. Values come from one per-export clock so they also give a
    /// rough ordering.
    versions: DashMap<RelPath, u64>,
    vclock: AtomicU64,
    /// Event fan-out for this export (watcher feeds it, sessions subscribe).
    pub events: Arc<crate::watch::EventHub>,
    /// Whole-file advisory locks shared by all sessions of this export.
    pub locks: crate::locks::LockManager,
    /// Server-side excludes: matching paths are invisible to every client.
    pub exclude: ExcludeSet,
    /// Suggested client settings (already size-parsed), served to v2 mounts.
    pub mount_defaults: MountDefaults,
}

/// Resolved form of the config's `client:` section.
#[derive(Debug, Default, Clone)]
pub struct MountDefaults {
    pub exclude: Vec<String>,
    pub pin: Vec<String>,
    pub auto_cache_max: Option<u64>,
    pub auto_cache_budget: Option<u64>,
}

impl Export {
    /// Path resolution with exclusion enforced BEFORE touching the disk and
    /// re-checked against the canonicalized result (so a case-insensitive
    /// volume can't sidestep a literal-case pattern). Excluded → NotFound —
    /// never PermissionDenied, existence must not leak.
    pub fn resolve(&self, rel: &RelPath) -> Result<PathBuf, ErrorCode> {
        if self.exclude.is_excluded(rel) {
            return Err(ErrorCode::NotFound);
        }
        let full = crate::fsutil::resolve_unchecked(&self.root, rel)?;
        if !self.exclude.is_empty() {
            if let Ok(stripped) = full.strip_prefix(&self.root) {
                if let Some(s) = stripped.to_str() {
                    if self.exclude.is_excluded(&RelPath(s.replace('\\', "/"))) {
                        return Err(ErrorCode::NotFound);
                    }
                }
            }
        }
        Ok(full)
    }

    /// resolve() for paths that may not exist yet (create/mkdir/rename-to).
    pub fn resolve_new(&self, rel: &RelPath) -> Result<PathBuf, ErrorCode> {
        if self.exclude.is_excluded(rel) {
            return Err(ErrorCode::NotFound);
        }
        rel.validate()?;
        let (parent, leaf) = rel.split().ok_or(ErrorCode::InvalidPath)?;
        let parent_full = self.resolve(&parent)?;
        Ok(parent_full.join(leaf))
    }

    pub fn version_of(&self, path: &RelPath) -> u64 {
        self.versions.get(path).map(|v| *v).unwrap_or(0)
    }

    pub fn bump(&self, path: &RelPath) -> u64 {
        let v = self.vclock.fetch_add(1, Ordering::Relaxed) + 1;
        self.versions.insert(path.clone(), v);
        v
    }

    /// Rename bookkeeping. Versions of children of a renamed directory keep
    /// their old keys — stale-but-harmless, since versions are freshness
    /// hints, not the source of truth.
    pub fn rename_version(&self, from: &RelPath, to: &RelPath) -> u64 {
        self.versions.remove(from);
        self.bump(to)
    }

    /// Directory listing with the same path hardening as the wire protocol —
    /// used by the HTTP browse endpoint. Blocking: call via spawn_blocking.
    pub fn browse(&self, rel: &RelPath) -> Result<Vec<DirEntry>, ErrorCode> {
        let full = self.resolve(rel)?;
        let mut entries = Vec::new();
        for item in std::fs::read_dir(&full).or_code()? {
            let item = item.or_code()?;
            let Ok(name) = item.file_name().into_string() else {
                continue;
            };
            let child = rel.join(&name);
            if self.exclude.is_excluded(&child) {
                continue; // invisible
            }
            let md = item
                .metadata()
                .or_else(|_| std::fs::symlink_metadata(item.path()));
            let Ok(md) = md else { continue };
            entries.push(DirEntry {
                name,
                attr: attr_from_metadata(&md, self.version_of(&child)),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }
}

/// All exports this agent serves. Built once at startup, shared by sessions.
pub struct ExportRegistry {
    exports: BTreeMap<String, Arc<Export>>,
    /// Live sessions, for the lease reaper. Weak: a session's true lifetime
    /// is its connection; the reaper only borrows.
    sessions: DashMap<u64, std::sync::Weak<SessionInner>>,
}

impl ExportRegistry {
    pub fn from_config(cfg: &crate::config::AgentConfig) -> anyhow::Result<Self> {
        let mut exports = BTreeMap::new();
        for (name, ec) in &cfg.exports {
            let root = std::fs::canonicalize(&ec.path)
                .map_err(|e| anyhow::anyhow!("export {name}: cannot resolve {:?}: {e}", ec.path))?;
            anyhow::ensure!(root.is_dir(), "export {name}: {root:?} is not a directory");
            // Server matching is always case-sensitive (documented).
            let exclude =
                ExcludeSet::compile(&ec.exclude, false).map_err(|e| anyhow::anyhow!("export {name}: {e}"))?;
            // Resolve the suggested-client-config sizes at startup so a bad
            // value fails the boot, not a mount.
            let mount_defaults = match &ec.client {
                Some(c) => MountDefaults {
                    exclude: c.exclude.clone(),
                    pin: c.pin.clone(),
                    auto_cache_max: c
                        .auto_cache_max
                        .as_ref()
                        .map(|s| s.to_bytes())
                        .transpose()
                        .map_err(|e| anyhow::anyhow!("export {name}: client.auto_cache_max: {e}"))?,
                    auto_cache_budget: c
                        .auto_cache_budget
                        .as_ref()
                        .map(|s| s.to_bytes())
                        .transpose()
                        .map_err(|e| anyhow::anyhow!("export {name}: client.auto_cache_budget: {e}"))?,
                },
                None => MountDefaults::default(),
            };
            tracing::info!(
                name,
                root = %root.display(),
                read_only = ec.read_only,
                excludes = ec.exclude.len(),
                "export ready"
            );
            exports.insert(
                name.clone(),
                Arc::new(Export {
                    name: name.clone(),
                    root,
                    read_only: ec.read_only,
                    versions: DashMap::new(),
                    vclock: AtomicU64::new(0),
                    events: crate::watch::EventHub::new(),
                    locks: crate::locks::LockManager::default(),
                    exclude,
                    mount_defaults,
                }),
            );
        }
        anyhow::ensure!(!exports.is_empty(), "no exports configured");
        Ok(Self {
            exports,
            sessions: DashMap::new(),
        })
    }

    pub fn get(&self, name: &str) -> Option<Arc<Export>> {
        self.exports.get(name).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Export>> {
        self.exports.values().cloned().collect()
    }

    /// Background task: every 5 s, free locks and handles of sessions whose
    /// last activity is older than `lease`. The answer to "what if a client
    /// dies while holding a lock" — at most `lease` of blockage.
    pub fn spawn_lease_reaper(self: &Arc<Self>, lease: std::time::Duration) {
        let registry = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                let Some(registry) = registry.upgrade() else { break };
                registry.sessions.retain(|id, weak| {
                    let Some(session) = weak.upgrade() else {
                        return false;
                    };
                    if session.last_seen_elapsed() > lease {
                        let released = session.force_release();
                        if released > 0 {
                            tracing::warn!(
                                session = id,
                                released,
                                "lease expired: released stale locks/handles"
                            );
                        }
                    }
                    true
                });
            }
        });
    }
}

struct OpenFile {
    file: File,
    path: RelPath,
    writable: bool,
}

/// Per-connection server state: which export the client attached to and the
/// file handles it holds. Handles are closed when the session ends
/// (`disconnected`), so a dead client never leaks descriptors.
///
/// The state lives behind an `Arc` because each request is dispatched on the
/// blocking thread pool (std::fs calls block their thread) and needs to own a
/// reference that outlives the `handle()` call.
pub struct AgentSession {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    /// Process-unique session number; rides on events as `origin` so clients
    /// can skip invalidating their own writes.
    id: u64,
    registry: Arc<ExportRegistry>,
    attached: std::sync::OnceLock<Arc<Export>>,
    handles: DashMap<u64, Arc<OpenFile>>,
    next_fh: AtomicU64,
    /// The connection's server→client push handle.
    ///
    /// Clearable, and that is the whole point. An `EventPusher` owns a clone
    /// of the connection's outbound `mpsc::Sender`, and `serve_connection`
    /// finishes by waiting for that channel to close. Holding one past the end
    /// of the session deadlocked the serve loop: the sender could not drop
    /// until the handler dropped, the handler could not drop until the loop
    /// returned, and the loop was waiting on the sender. One task and one
    /// socket write half leaked per disconnected client. `disconnected()`
    /// drops it — see the test in tests/session_leak.rs.
    push: std::sync::Mutex<Option<EventPusher>>,
    /// Updated on every request; the lease reaper compares against it.
    last_seen: std::sync::Mutex<std::time::Instant>,
    /// v3 TCP auth: `Some` on token-protected listeners, `None` on stdio
    /// (ssh already authenticated the user) and open TCP.
    required_token: Option<String>,
    /// Starts true when no token is required; flipped by `Request::Auth`.
    authed: AtomicBool,
}

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

impl AgentSession {
    pub fn new(registry: Arc<ExportRegistry>) -> Self {
        Self::with_token(registry, None)
    }

    /// A session that must present `token` (via `Request::Auth`, protocol
    /// v3+) before any other request. Used by token-protected TCP listeners;
    /// stdio sessions use `new` — ssh already authenticated the user.
    pub fn with_token(registry: Arc<ExportRegistry>, token: Option<String>) -> Self {
        let inner = Arc::new(SessionInner {
            id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            registry: registry.clone(),
            attached: std::sync::OnceLock::new(),
            handles: DashMap::new(),
            next_fh: AtomicU64::new(1),
            push: std::sync::Mutex::new(None),
            last_seen: std::sync::Mutex::new(std::time::Instant::now()),
            authed: AtomicBool::new(token.is_none()),
            required_token: token,
        });
        registry.sessions.insert(inner.id, Arc::downgrade(&inner));
        Self { inner }
    }

    /// Async-context Subscribe: spawns the forwarding task tying this
    /// session's connection to the export's event hub.
    fn subscribe(&self, since_seq: Option<u64>) -> Result<Response, ErrorCode> {
        let export = self.inner.export()?;
        // Cloned out under the lock, never held across an await.
        let push = self.inner.push.lock().unwrap().clone().ok_or(ErrorCode::Io)?;
        let session_id = self.inner.id;
        let (catchup, mut rx) = export.events.subscribe(since_seq)?;
        let last_seq = export.events.last_seq();
        tokio::spawn(async move {
            // A session never hears its own writes back: the OS on its side
            // already announced changes that went through its mount, so an
            // echo would double-notify (and waste a wakeup).
            let strip = move |mut batch: Vec<FsEvent>| {
                batch.retain(|e| e.origin != Some(session_id));
                batch
            };
            let catchup = strip(catchup);
            if !catchup.is_empty() && !push.events(catchup).await {
                return;
            }
            loop {
                match rx.recv().await {
                    Ok(batch) => {
                        let batch = strip(batch);
                        if batch.is_empty() {
                            continue;
                        }
                        if !push.events(batch).await {
                            break; // connection gone
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // This subscriber missed events: tell it to resync.
                        tracing::warn!(missed = n, "subscriber lagged, sending resync");
                        let resync = vec![FsEvent {
                            seq: 0,
                            kind: EventKind::ResyncRequired,
                            path: RelPath(String::new()),
                            new_version: None,
                            origin: None,
                        }];
                        if !push.events(resync).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(Response::Subscribed { last_seq })
    }
}

impl SessionInner {
    fn touch(&self) {
        *self.last_seen.lock().unwrap() = std::time::Instant::now();
    }

    fn last_seen_elapsed(&self) -> std::time::Duration {
        self.last_seen.lock().unwrap().elapsed()
    }

    /// Free all locks and handles (lease expiry / disconnect). The session
    /// object stays usable — new opens simply start fresh.
    fn force_release(&self) -> usize {
        let mut released = 0;
        if let Some(export) = self.attached.get() {
            released = export.locks.release_session(self.id);
        }
        released += self.handles.len();
        self.handles.clear();
        released
    }

    fn export(&self) -> Result<Arc<Export>, ErrorCode> {
        self.attached.get().cloned().ok_or(ErrorCode::NotAttached)
    }

    /// The single read_only gate: every mutating handler starts here.
    fn writable_export(&self) -> Result<Arc<Export>, ErrorCode> {
        let export = self.export()?;
        if export.read_only {
            return Err(ErrorCode::ReadOnly);
        }
        Ok(export)
    }

    fn handle_of(&self, fh: u64) -> Result<Arc<OpenFile>, ErrorCode> {
        self.handles
            .get(&fh)
            .map(|h| h.clone())
            .ok_or(ErrorCode::BadHandle)
    }

    fn insert_handle(&self, file: File, path: RelPath, writable: bool) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles
            .insert(fh, Arc::new(OpenFile { file, path, writable }));
        fh
    }

    // ---- one method per request; dispatch_blocking is just the phone book --

    fn attach(&self, export: String) -> Result<Response, ErrorCode> {
        // Re-attaching to the SAME export is idempotent (harmless retry);
        // a different export would silently keep serving the first one —
        // refuse instead of misbinding the session.
        if let Some(current) = self.attached.get() {
            if current.name != export {
                tracing::warn!(
                    attached = current.name,
                    requested = export,
                    "second attach to a different export refused"
                );
                return Err(ErrorCode::AlreadyExists);
            }
        }
        let export = self.registry.get(&export).ok_or(ErrorCode::NoSuchExport)?;
        let md = std::fs::metadata(&export.root).or_code()?;
        let attr = attr_from_metadata(&md, 0);
        tracing::info!(export = export.name, "client attached");
        let _ = self.attached.set(export);
        Ok(Response::AttachOk {
            export_id: 0,
            root_attr: attr,
        })
    }

    fn getattr(&self, path: RelPath) -> Result<Response, ErrorCode> {
        let export = self.export()?;
        let full = export.resolve(&path)?;
        let md = std::fs::metadata(&full).or_code()?;
        Ok(Response::Attr(attr_from_metadata(&md, export.version_of(&path))))
    }

    fn readdir(&self, path: RelPath, cursor: u64) -> Result<Response, ErrorCode> {
        let export = self.export()?;
        let full = export.resolve(&path)?;
        let mut entries: Vec<DirEntry> = Vec::new();
        for item in std::fs::read_dir(&full).or_code()? {
            let item = item.or_code()?;
            let name = match item.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue, // non-UTF-8 names are not served
            };
            let child = path.join(&name);
            if export.exclude.is_excluded(&child) {
                continue; // invisible to every client
            }
            // metadata() follows symlinks; fall back to the link's own
            // metadata for broken links so the entry still lists.
            let md = item
                .metadata()
                .or_else(|_| std::fs::symlink_metadata(item.path()));
            let Ok(md) = md else { continue };
            // Real versions in listings: the client auto-cache walker relies
            // on them for freshness without extra Getattrs.
            entries.push(DirEntry {
                name,
                attr: attr_from_metadata(&md, export.version_of(&child)),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let start = cursor as usize;
        let page: Vec<DirEntry> = entries.iter().skip(start).take(READDIR_PAGE).cloned().collect();
        let next_cursor = if start + page.len() < entries.len() {
            Some((start + page.len()) as u64)
        } else {
            None
        };
        Ok(Response::Dir {
            entries: page,
            next_cursor,
        })
    }

    fn open(&self, path: RelPath, flags: OpenFlags) -> Result<Response, ErrorCode> {
        let export = self.export()?;
        let wants_write = flags.write || flags.truncate || flags.append;
        if wants_write && export.read_only {
            return Err(ErrorCode::ReadOnly);
        }
        let full = export.resolve(&path)?;
        let file = File::options()
            .read(true)
            .write(wants_write)
            .truncate(flags.truncate)
            .open(&full)
            .or_code()?;
        let md = file.metadata().or_code()?;
        if md.is_dir() {
            return Err(ErrorCode::IsADirectory);
        }
        let version = if flags.truncate {
            export.bump(&path)
        } else {
            export.version_of(&path)
        };
        let attr = attr_from_metadata(&md, version);
        let fh = self.insert_handle(file, path, wants_write);
        Ok(Response::Opened { fh, attr })
    }

    fn create(&self, path: RelPath, flags: OpenFlags, mode: u32) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let full = export.resolve_new(&path)?;
        // O_EXCL means "must not exist"; a plain O_CREAT loser of a create
        // race just opens the existing file (POSIX).
        let mut opts = File::options();
        opts.read(true).write(true);
        if flags.excl {
            opts.create_new(true);
        } else {
            opts.create(true).truncate(flags.truncate);
        }
        let file = opts.open(&full).or_code()?;
        set_mode(&file, mode);
        let md = file.metadata().or_code()?;
        export.events.note_local_write(&path, self.id);
        let attr = attr_from_metadata(&md, export.bump(&path));
        let fh = self.insert_handle(file, path, true);
        Ok(Response::Opened { fh, attr })
    }

    fn read(&self, fh: u64, offset: u64, len: u32) -> Result<Response, ErrorCode> {
        if len > DATA_CHUNK {
            return Err(ErrorCode::Io);
        }
        let of = self.handle_of(fh)?;
        let mut buf = vec![0u8; len as usize];
        let n = read_fully(&of.file, &mut buf, offset).or_code()?;
        buf.truncate(n);
        Ok(Response::Data(buf.into()))
    }

    fn write(
        &self,
        fh: u64,
        offset: u64,
        data: bytes::Bytes,
        expect_version: Option<u64>,
    ) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let of = self.handle_of(fh)?;
        if !of.writable {
            return Err(ErrorCode::BadHandle);
        }
        // Conflict *detection*, not prevention: last writer wins, but the
        // loser's client gets told and can surface it.
        let conflict = matches!(expect_version, Some(v) if v != export.version_of(&of.path));
        if conflict {
            tracing::warn!(path = %of.path, "write conflict (concurrent modification)");
        }
        write_fully(&of.file, &data, offset).or_code()?;
        export.events.note_local_write(&of.path, self.id);
        let new_version = export.bump(&of.path);
        Ok(Response::Written {
            n: data.len() as u32,
            new_version,
            conflict,
        })
    }

    fn setattr(
        &self,
        path: RelPath,
        size: Option<u64>,
        mtime: Option<std::time::SystemTime>,
        mode: Option<u32>,
    ) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let full = export.resolve(&path)?;
        if let Some(size) = size {
            File::options()
                .write(true)
                .open(&full)
                .or_code()?
                .set_len(size)
                .or_code()?;
        }
        if let Some(mtime) = mtime {
            File::options()
                .write(true)
                .open(&full)
                .or_code()?
                .set_modified(mtime)
                .or_code()?;
        }
        if let Some(mode) = mode {
            let f = File::options().write(true).open(&full).or_code()?;
            set_mode(&f, mode);
        }
        let md = std::fs::metadata(&full).or_code()?;
        export.events.note_local_write(&path, self.id);
        Ok(Response::Attr(attr_from_metadata(&md, export.bump(&path))))
    }

    fn mkdir(&self, path: RelPath, _mode: u32) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let full = export.resolve_new(&path)?;
        std::fs::create_dir(&full).or_code()?; // directory modes: server umask decides
        let md = std::fs::metadata(&full).or_code()?;
        export.events.note_local_write(&path, self.id);
        Ok(Response::Attr(attr_from_metadata(&md, export.bump(&path))))
    }

    fn unlink(&self, path: RelPath) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let full = export.resolve(&path)?;
        std::fs::remove_file(&full).or_code()?;
        export.events.note_local_write(&path, self.id);
        export.bump(&path);
        Ok(Response::Ok)
    }

    fn rmdir(&self, path: RelPath) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let full = export.resolve(&path)?;
        std::fs::remove_dir(&full).or_code()?;
        export.events.note_local_write(&path, self.id);
        export.bump(&path);
        Ok(Response::Ok)
    }

    fn rename(&self, from: RelPath, to: RelPath, replace: bool) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let from_full = export.resolve(&from)?;
        let to_full = export.resolve_new(&to)?;
        if !replace && to_full.exists() {
            return Err(ErrorCode::AlreadyExists);
        }
        // std::fs::rename replaces atomically on BOTH platforms now (Windows
        // uses FileRenameInfoEx with POSIX semantics since ~Rust 1.78 —
        // verified on this toolchain, including with the target open), so no
        // remove-then-rename window is needed.
        std::fs::rename(&from_full, &to_full).or_code()?;
        export.events.note_local_write(&from, self.id);
        export.events.note_local_write(&to, self.id);
        export.rename_version(&from, &to);
        Ok(Response::Ok)
    }

    fn link(&self, target: RelPath, link: RelPath) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let target_full = export.resolve(&target)?;
        let link_full = export.resolve_new(&link)?;
        std::fs::hard_link(&target_full, &link_full).or_code()?;
        let md = std::fs::metadata(&link_full).or_code()?;
        export.events.note_local_write(&link, self.id);
        Ok(Response::Attr(attr_from_metadata(&md, export.bump(&link))))
    }

    fn release(&self, fh: u64) -> Result<Response, ErrorCode> {
        if let Some((_, of)) = self.handles.remove(&fh) {
            // Closing a handle drops any lock it held (flock semantics).
            if let Some(export) = self.attached.get() {
                export.locks.unlock(&of.path, self.id, fh);
            }
        }
        Ok(Response::Ok)
    }

    fn statfs(&self) -> Result<Response, ErrorCode> {
        // Real filesystem numbers for the attached export's volume.
        // Placeholders remain for pre-attach callers (`alloyfs stress`
        // sends Statfs without attaching) and syscall failure — `df` erroring
        // would be worse than `df` lying.
        if let Some(export) = self.attached.get() {
            if let Some((block_size, blocks, blocks_free)) = crate::fsutil::fs_space(&export.root) {
                return Ok(Response::Statfs {
                    block_size,
                    blocks,
                    blocks_free,
                });
            }
        }
        Ok(Response::Statfs {
            block_size: 4096,
            blocks: 1 << 24,
            blocks_free: 1 << 23,
        })
    }

    fn mount_defaults(&self) -> Result<Response, ErrorCode> {
        let export = self.export()?;
        let d = &export.mount_defaults;
        Ok(Response::MountDefaults {
            exclude: d.exclude.clone(),
            pin: d.pin.clone(),
            auto_cache_max: d.auto_cache_max,
            auto_cache_budget: d.auto_cache_budget,
        })
    }

    /// Blocking-pool dispatch: pure routing, no logic.
    fn dispatch_blocking(&self, req: Request) -> Result<Response, ErrorCode> {
        match req {
            Request::Attach { export } => self.attach(export),
            Request::Getattr { path } => self.getattr(path),
            Request::Readdir { path, cursor } => self.readdir(path, cursor),
            Request::Open { path, flags } => self.open(path, flags),
            Request::Create { path, flags, mode } => self.create(path, flags, mode),
            Request::Read { fh, offset, len } => self.read(fh, offset, len),
            Request::Write {
                fh,
                offset,
                data,
                expect_version,
            } => self.write(fh, offset, data, expect_version),
            Request::Flush { .. } => Ok(Response::Ok),
            Request::Release { fh } => self.release(fh),
            Request::Setattr {
                path,
                size,
                mtime,
                mode,
            } => self.setattr(path, size, mtime, mode),
            Request::Mkdir { path, mode } => self.mkdir(path, mode),
            Request::Unlink { path } => self.unlink(path),
            Request::Rmdir { path } => self.rmdir(path),
            Request::Rename { from, to, replace } => self.rename(from, to, replace),
            Request::Link { target, link } => self.link(target, link),
            Request::Statfs => self.statfs(),
            Request::MountDefaults => self.mount_defaults(),
            // Handled in async context (handle()); never reach the pool.
            Request::Lock { .. }
            | Request::Unlock { .. }
            | Request::Subscribe { .. }
            | Request::Auth { .. } => Err(ErrorCode::Io),
        }
    }
}

#[async_trait::async_trait]
impl RequestHandler for AgentSession {
    async fn handle(&self, req: Request) -> Result<Response, ErrorCode> {
        self.inner.touch();
        // Auth gate first: on a token-protected session nothing else is
        // served until Request::Auth presents the right secret.
        if let Request::Auth { token } = &req {
            let expected = self.inner.required_token.as_deref().unwrap_or("");
            return if self.inner.required_token.is_none() {
                Ok(Response::Ok) // no token required: auth is a no-op
            } else if alloyfs_common::token_eq(token, expected) {
                self.inner.authed.store(true, Ordering::Release);
                Ok(Response::Ok)
            } else {
                tracing::warn!(session = self.inner.id, "rejected bad auth token");
                Err(ErrorCode::PermissionDenied)
            };
        }
        if !self.inner.authed.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthRequired);
        }
        // Subscribe spawns an async forwarder and Lock may await another
        // session's release — both must run here in async context; everything
        // else does blocking file I/O and moves to the blocking pool.
        match req {
            Request::Subscribe { since_seq } => self.subscribe(since_seq),
            Request::Lock { fh, kind, wait } => {
                let inner = &self.inner;
                let export = inner.export()?;
                let path = inner.handle_of(fh)?.path.clone();
                export.locks.lock(&path, inner.id, fh, kind, wait).await?;
                Ok(Response::Ok)
            }
            Request::Unlock { fh } => {
                let inner = &self.inner;
                let export = inner.export()?;
                let path = inner.handle_of(fh)?.path.clone();
                export.locks.unlock(&path, inner.id, fh);
                Ok(Response::Ok)
            }
            req => {
                let inner = self.inner.clone();
                tokio::task::spawn_blocking(move || inner.dispatch_blocking(req))
                    .await
                    .map_err(|_| ErrorCode::Io)?
            }
        }
    }

    async fn connected(&self, push: EventPusher) {
        *self.inner.push.lock().unwrap() = Some(push);
    }

    async fn disconnected(&self) {
        let released = self.inner.force_release();
        if released > 0 {
            tracing::info!(released, "session ended, released handles/locks");
        }
        self.inner.registry.sessions.remove(&self.inner.id);
        // Drop the outbound sender. `serve_connection` waits for that channel
        // to close before it returns, so keeping it here wedges the connection
        // open forever — see the field comment.
        self.inner.push.lock().unwrap().take();
    }
}
