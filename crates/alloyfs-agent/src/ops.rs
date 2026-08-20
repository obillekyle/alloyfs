use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;

use alloyfs_common::{attr_from_metadata, read_fully, set_mode_path, write_fully, ExcludeSet, OrCode};
use alloyfs_proto::{
    Attr, DirEntry, ErrorCode, EventKind, FileKind, FsEvent, ManyEntry, OpenFlags, RelPath, Request,
    Response, DATA_CHUNK, PROTO_VERSION_MIN,
};
use alloyfs_transport::{EventPusher, RequestHandler};
use dashmap::DashMap;

/// Max directory entries per Readdir response; clients page with the cursor.
const READDIR_PAGE: usize = 1024;

/// Byte ceiling for one `ReadMany` reply. Well under `MAX_FRAME_LEN` (1 MiB)
/// so the entries, their paths and their attributes all still fit alongside
/// the file contents.
const READ_MANY_BUDGET: u32 = 768 * 1024;

/// Entry ceiling for one `ReadMany` reply, independent of the byte budget: a
/// request naming a million empty files would otherwise cost a million stats
/// in a single blocking call.
const READ_MANY_ENTRIES: usize = 4096;

/// Entries per `Tree` page. Larger than a readdir page because the whole
/// point is fewer round trips, and bounded because `MAX_FRAME_LEN` is 1 MiB —
/// at roughly 80 bytes an encoded entry, 4096 leaves generous headroom.
const TREE_PAGE: usize = 4096;

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
    /// The v6 index. Built on first request, not at startup — an export no
    /// client asks a tree of is never walked.
    pub tree: crate::tree::ExportTree,
    /// v11 Windows attribute bits: native NTFS on a Windows agent, the
    /// export's `.alloyfs` sidecar on Linux. See winattr.rs.
    pub winattrs: crate::winattr::WinAttrs,
    /// The mirror problem: POSIX modes on a WINDOWS agent, where NTFS has
    /// no execute bit to hold a Linux client's `chmod +x`. Same sidecar
    /// machinery, opposite direction; no wire change — `mode` was always
    /// there, this just makes a Windows server stop fabricating it.
    #[cfg(windows)]
    pub posix_modes: crate::winattr::SidecarMap,
}

impl Export {
    /// Merge the v11 bits into a served Attr. On Windows this is the
    /// identity — `attr_from_metadata` read the bits off the metadata
    /// already; on Linux it is the sidecar's serve-time merge. The tree
    /// index also stores bit-less attrs on Linux, which this covers at the
    /// same sites that overlay versions.
    pub fn with_winattrs(&self, rel: &RelPath, attr: Attr) -> Attr {
        #[cfg(unix)]
        {
            let bits = self.winattrs.get(rel);
            if bits != 0 {
                return Attr {
                    mode: attr.mode | bits,
                    ..attr
                };
            }
        }
        #[cfg(windows)]
        {
            if attr.kind == FileKind::File {
                if let Some(m) = self.posix_modes.get(rel) {
                    // The sidecar carries the POSIX intent; the native
                    // ReadOnly attribute stays authoritative for the write
                    // bits, so a server-side `attrib +r` is never overridden
                    // by an older chmod.
                    let write = if attr.mode & 0o222 == 0 { 0 } else { m & 0o222 };
                    let merged = (m & !0o222 & 0o7777) | write;
                    return Attr {
                        mode: (attr.mode & !0o7777) | merged,
                        ..attr
                    };
                }
            }
        }
        attr
    }

    /// Every sidecar's view of a removal — whichever platform carries one.
    fn sidecars_remove(&self, rel: &RelPath) {
        self.winattrs.remove(rel);
        #[cfg(windows)]
        self.posix_modes.remove(rel);
    }

    /// Every sidecar's view of a rename.
    fn sidecars_rename(&self, from: &RelPath, to: &RelPath) {
        self.winattrs.rename(from, to);
        #[cfg(windows)]
        self.posix_modes.rename(from, to);
    }
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
    /// True when `full` — a real, canonicalized on-disk path under the root —
    /// names something the export excludes. Split out because BOTH resolvers
    /// need it: a case-insensitive volume resolves `CREDENTIALS.JSON` to an
    /// excluded `credentials.json`, and only the on-disk spelling reveals that.
    fn excluded_on_disk(&self, full: &Path) -> bool {
        if self.exclude.is_empty() {
            return false;
        }
        let Ok(stripped) = full.strip_prefix(&self.root) else {
            return false;
        };
        let Some(s) = stripped.to_str() else {
            return false;
        };
        self.exclude.is_excluded(&RelPath(s.replace('\\', "/")))
    }

    /// Path resolution with exclusion enforced BEFORE touching the disk and
    /// re-checked against the canonicalized result (so a case-insensitive
    /// volume can't sidestep a literal-case pattern). Excluded → NotFound —
    /// never PermissionDenied, existence must not leak.
    pub fn resolve(&self, rel: &RelPath) -> Result<PathBuf, ErrorCode> {
        if self.exclude.is_excluded(rel) {
            return Err(ErrorCode::NotFound);
        }
        let full = crate::fsutil::resolve_unchecked(&self.root, rel)?;
        if self.excluded_on_disk(&full) {
            return Err(ErrorCode::NotFound);
        }
        Ok(full)
    }

    /// resolve() for paths that may not exist yet (create/mkdir/rename-to).
    ///
    /// `resolve` draws its safety from canonicalizing the result and checking
    /// the prefix afterwards. A path that does not exist yet cannot be
    /// canonicalized, so every check here has to stand on its own:
    ///
    /// - **The leaf must be exactly one ordinary component.** `Path::join`
    ///   REPLACES its buffer when the argument carries a prefix or a root
    ///   rather than appending under it, so a leaf that parses as anything
    ///   but a single plain name must never reach the join. `validate` already
    ///   rejects the characters that make that possible; this asserts the OS
    ///   agrees, on the platform actually running.
    /// - **The join must still land under the root** — cheap, and it catches
    ///   any splice the component check did not anticipate.
    /// - **An existing target is re-checked by its on-disk name**, which is
    ///   what closes the case-insensitive exclude bypass: without it,
    ///   `create("CREDENTIALS.JSON")` opens an excluded `credentials.json`
    ///   read-write on Windows or macOS, because the raw check above compares
    ///   case-sensitively and the filesystem does not.
    pub fn resolve_new(&self, rel: &RelPath) -> Result<PathBuf, ErrorCode> {
        if self.exclude.is_excluded(rel) {
            return Err(ErrorCode::NotFound);
        }
        rel.validate()?;
        let (parent, leaf) = rel.split().ok_or(ErrorCode::InvalidPath)?;
        let mut comps = Path::new(leaf).components();
        let one_plain_name = matches!(
            (comps.next(), comps.next()),
            (Some(std::path::Component::Normal(c)), None) if c.to_str() == Some(leaf)
        );
        if !one_plain_name {
            return Err(ErrorCode::InvalidPath);
        }
        let parent_full = self.resolve(&parent)?;
        let full = parent_full.join(leaf);
        if !full.starts_with(&self.root) {
            return Err(ErrorCode::InvalidPath);
        }
        if let Ok(canon) = std::fs::canonicalize(&full) {
            if self.excluded_on_disk(&canon) {
                return Err(ErrorCode::NotFound);
            }
        }
        Ok(full)
    }

    pub fn version_of(&self, path: &RelPath) -> u64 {
        self.versions.get(path).map(|v| *v).unwrap_or(0)
    }

    pub fn bump(&self, path: &RelPath) -> u64 {
        let v = self.vclock.fetch_add(1, Ordering::Relaxed) + 1;
        self.versions.insert(path.clone(), v);
        v
    }

    /// Advance the clock for a path that no longer exists, dropping whatever
    /// version it had.
    ///
    /// `bump` only ever inserts, and unlink and rmdir called it on the path
    /// they had just removed — so every delete left a tombstone that nothing
    /// pruned. A create/unlink loop over unique names grew `versions` without
    /// bound, and on a sub-gigabyte agent that ends as an OOM kill taking
    /// every other mount with it. The tree index is bounded by live files
    /// because it removes on delete; this map had no such path.
    ///
    /// The clock still advances, so a recreated path is handed a version
    /// strictly greater than the one a client may have cached for the file
    /// that used to be there.
    pub fn forget_version(&self, path: &RelPath) -> u64 {
        let v = self.vclock.fetch_add(1, Ordering::Relaxed) + 1;
        self.versions.remove(path);
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
                attr: self.with_winattrs(&child, attr_from_metadata(&md, self.version_of(&child))),
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
            // Server matching is always case-sensitive (documented) for the
            // user's own patterns; the built-in local artifacts are matched
            // case-insensitively regardless, since their casing is the OS's
            // choice rather than anybody's configuration.
            // `.alloyfs` is the export's own metadata (the v11 winattr
            // sidecar lives there): server-excluded on EVERY export, so no
            // client ever lists, reads, or creates through it. Added on both
            // platforms — an export served alternately by a Linux and a
            // Windows agent must hide the same directory either way.
            let mut patterns = vec![".alloyfs".to_string(), ".alloyfs/**".to_string()];
            patterns.extend(ec.exclude.iter().cloned());
            let exclude = if ec.default_excludes {
                ExcludeSet::compile_with_defaults(&patterns, false)
            } else {
                ExcludeSet::compile(&patterns, false)
            }
            .map_err(|e| anyhow::anyhow!("export {name}: {e}"))?;
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
                    winattrs: crate::winattr::WinAttrs::load(&root),
                    #[cfg(windows)]
                    posix_modes: crate::winattr::SidecarMap::load(&root, "posix.json", |m| m & 0o7777 != 0),
                    root,
                    read_only: ec.read_only,
                    versions: DashMap::new(),
                    vclock: AtomicU64::new(0),
                    events: crate::watch::EventHub::new(),
                    locks: crate::locks::LockManager::default(),
                    exclude,
                    mount_defaults,
                    tree: crate::tree::ExportTree::new(
                        ec.tree_max_entries.unwrap_or(crate::tree::DEFAULT_MAX_ENTRIES),
                    ),
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
    /// The version the handshake settled on, set by `negotiated()` before the
    /// first request. Starts at the floor so a session served by something
    /// that never reports one still answers in the shape every peer decodes.
    proto: AtomicU16,
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
            proto: AtomicU16::new(PROTO_VERSION_MIN),
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
        Ok(Response::Attr(export.with_winattrs(
            &path,
            attr_from_metadata(&md, export.version_of(&path)),
        )))
    }

    /// Build the index if needed, replaying anything that changed during the
    /// walk.
    ///
    /// The mark is taken BEFORE walking. The watcher has been running since
    /// the export was created — well before any client could ask — so a change
    /// landing mid-walk is either visible to the walk or present in the replay
    /// above the mark. Applying it through both paths is harmless: each is an
    /// idempotent overwrite of one path's entry.
    fn indexed_token(&self, export: &Arc<Export>) -> u64 {
        if export.tree.token() != 0 {
            return export.tree.token();
        }
        let mark = export.events.last_seq();
        let token = export.tree.ensure(&export.root, &export.exclude);
        if token == 0 {
            return 0;
        }
        for e in export.events.since(mark) {
            match e.kind {
                EventKind::Removed => export.tree.note_change(&export.root, &e.path, true),
                EventKind::RenamedFrom { ref to } => {
                    export.tree.note_change(&export.root, &e.path, true);
                    export.tree.note_change(&export.root, to, false);
                }
                // A gap during the build makes the build itself suspect.
                EventKind::ResyncRequired => export.tree.invalidate(),
                _ => export.tree.note_change(&export.root, &e.path, false),
            }
        }
        export.tree.token()
    }

    fn tree(&self, path: RelPath, cursor: Option<u64>) -> Result<Response, ErrorCode> {
        let export = self.export()?;
        // Resolve for the same reason readdir does: an excluded or escaping
        // path must not become listable just because a different verb asked.
        export.resolve(&path)?;
        // "Not indexed" is reported in-band as token 0 rather than as an
        // error. It is not a failure — the export is simply larger than the
        // cap, or indexing did not work here — and the client's answer is to
        // carry on with `Readdir`. An error code would also have meant
        // appending a variant to `ErrorCode` permanently, to say something the
        // token already says.
        let unindexed = Ok(Response::Tree {
            entries: Vec::new(),
            next_cursor: None,
            token: 0,
        });
        if self.indexed_token(&export) == 0 {
            return unindexed;
        }
        let offset = cursor.unwrap_or(0) as usize;
        let Some((entries, more, token)) = export.tree.page(&path, offset, TREE_PAGE) else {
            return unindexed;
        };
        let next_cursor = more.then(|| (offset + entries.len()) as u64);
        Ok(Response::Tree {
            entries: entries
                .into_iter()
                .map(|(p, attr)| alloyfs_proto::TreeEntry {
                    attr: export.with_winattrs(
                        &p,
                        Attr {
                            version: export.version_of(&p),
                            ..attr
                        },
                    ),
                    path: p,
                })
                .collect(),
            next_cursor,
            token,
        })
    }

    fn tree_token(&self) -> Result<Response, ErrorCode> {
        let export = self.export()?;
        Ok(Response::TreeToken {
            token: self.indexed_token(&export),
        })
    }

    fn readdir(&self, path: RelPath, cursor: u64) -> Result<Response, ErrorCode> {
        let export = self.export()?;
        // Serve from the v6 index when it can answer exactly. The disk path
        // below re-reads and re-stats the ENTIRE directory on every page —
        // O(N) disk work per page and O(N²) to list a big directory in full —
        // where the index has paid that cost once and is kept current by the
        // watcher. Three things make the swap safe:
        //
        // - Excludes: the index never contains an excluded path (the build
        //   walk filters them, and the Coalescer drops their events before
        //   `note_change` sees them), so no re-filter here — and an excluded
        //   directory is absent from the index, which routes it to the disk
        //   path's NotFound, same answer as today.
        // - Versions: the index stores attrs with version 0 (they come from
        //   plain stats). Served as-is that would defeat client freshness
        //   checks — autocache treats "either side 0" as unknowable — so the
        //   live version is overlaid per entry, exactly as the disk path and
        //   `tree()` do.
        // - Everything the index cannot answer exactly returns None (not
        //   indexed, path unknown to it, not a directory, a case-insensitive
        //   spelling only the volume can resolve) and falls through to disk,
        //   keeping every error identical.
        if let Some((children, more)) = export.tree.readdir_page(&path, cursor as usize, READDIR_PAGE) {
            let entries: Vec<DirEntry> = children
                .into_iter()
                .map(|(name, attr)| {
                    let child = path.join(&name);
                    // The index stores a link AS a link (following one during
                    // the walk could escape the export); the disk path below
                    // follows. Keep readdir's contract by following here —
                    // links are rare enough to stat individually, and a broken
                    // one keeps the link's own attrs, matching the disk path's
                    // symlink_metadata fallback.
                    let attr = if attr.kind == FileKind::Symlink {
                        std::fs::metadata(export.root.join(&child.0))
                            .map(|md| attr_from_metadata(&md, 0))
                            .unwrap_or(attr)
                    } else {
                        attr
                    };
                    DirEntry {
                        name,
                        attr: export.with_winattrs(
                            &child,
                            Attr {
                                version: export.version_of(&child),
                                ..attr
                            },
                        ),
                    }
                })
                .collect();
            let next_cursor = more.then(|| cursor + entries.len() as u64);
            return Ok(Response::Dir { entries, next_cursor });
        }
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
                attr: export.with_winattrs(&child, attr_from_metadata(&md, export.version_of(&child))),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let start = cursor as usize;
        let total = entries.len();
        // `into_iter`, not `iter().cloned()`: `entries` is dead after this, so
        // cloning every returned DirEntry (name String included) bought
        // nothing.
        let page: Vec<DirEntry> = entries.into_iter().skip(start).take(READDIR_PAGE).collect();
        let next_cursor = if start + page.len() < total {
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
        let attr = export.with_winattrs(&path, attr_from_metadata(&md, version));
        let fh = self.insert_handle(file, path, wants_write);
        Ok(Response::Opened { fh, attr })
    }

    /// v9+: `open` that carries the head of the file back with the handle.
    ///
    /// The read happens under the SAME handle being returned, after the open
    /// decided everything else — so every rule `open` enforces (read-only
    /// exports, truncate bumping the version, IsADirectory) holds identically,
    /// and a race with a concurrent writer is no wider than open-then-read
    /// always was.
    fn open_read(&self, path: RelPath, flags: OpenFlags, len: u32) -> Result<Response, ErrorCode> {
        let opened = self.open(path, flags)?;
        let Response::Opened { fh, attr } = opened else {
            return Ok(opened); // open() only returns Opened, but stay total
        };
        // No data wanted, none to have, or nothing that reads like a file:
        // answer with an empty head rather than refusing — the caller still
        // saved the round trip it actually needed (the open).
        let want = len.min(DATA_CHUNK).min(attr.size.min(u32::MAX as u64) as u32);
        let data = if want == 0 || flags.truncate {
            bytes::Bytes::new()
        } else {
            let of = self.handle_of(fh)?;
            let mut buf = vec![0u8; want as usize];
            let n = read_fully(&of.file, &mut buf, 0).or_code()?;
            buf.truncate(n);
            buf.into()
        };
        Ok(Response::OpenedData { fh, attr, data })
    }

    /// v9+: `setattr` plus `readonly`, resolved against the file's current
    /// mode HERE — one atomic step where the WinFsp adapter used to
    /// getattr-then-Setattr, paying a round trip and racing concurrent chmods.
    fn setattr2(
        &self,
        path: RelPath,
        size: Option<u64>,
        mtime: Option<std::time::SystemTime>,
        mode: Option<u32>,
        readonly: Option<bool>,
    ) -> Result<Response, ErrorCode> {
        let mode = match (mode, readonly) {
            // An explicit mode wins outright; readonly alongside it would be
            // two writers for one field.
            (Some(m), _) => Some(m),
            (None, Some(ro)) => {
                let export = self.writable_export()?;
                let full = export.resolve(&path)?;
                let cur = std::fs::metadata(&full).or_code()?;
                let cur_mode = alloyfs_common::mode_of_md(&cur);
                Some(if ro { cur_mode & !0o222 } else { cur_mode | 0o200 })
            }
            (None, None) => None,
        };
        self.setattr(path, size, mtime, mode)
    }

    /// v9+: attach + mount defaults + tree token, one exchange. Each half is
    /// the existing handler's answer verbatim; only the round trips fold.
    fn attach2(&self, export_name: String) -> Result<Response, ErrorCode> {
        let attached = self.attach(export_name)?;
        let Response::AttachOk { export_id, root_attr } = attached else {
            return Ok(attached);
        };
        let export = self.export()?;
        let d = &export.mount_defaults;
        Ok(Response::Attached2 {
            export_id,
            root_attr,
            exclude: d.exclude.clone(),
            pin: d.pin.clone(),
            auto_cache_max: d.auto_cache_max,
            auto_cache_budget: d.auto_cache_budget,
            // `token()`, never `ensure()`: attach must not build the index.
            // Forcing it here broke two contracts at once — "an export no
            // client asks a tree of is never walked", and worse, on an
            // UNWATCHED export the freshly built index is frozen, and readdir
            // (which serves from a live index) stops seeing mutations. 0 here
            // means "not currently indexed"; a client that wants the token
            // asks `TreeToken`, which is the request that builds — same as
            // v6 always worked.
            tree_token: export.tree.token(),
        })
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
        // The mode goes to `open`, NOT to a chmod afterwards. They are not the
        // same operation and the difference is visible in the resulting file:
        //
        //   - `open(..., O_CREAT, mode)` applies `mode & ~umask`, so the
        //     SERVER's umask decides — which is what `mkdir` below already
        //     relies on — and a default ACL on the parent directory
        //     contributes its entries.
        //   - `chmod(mode)` applies the mode verbatim, umask ignored, and
        //     rewrites the ACL mask from the group bits, discarding whatever
        //     the default ACL had granted.
        //
        // Measured on an export with umask 002 and a default ACL granting
        // www-data rwx: chmod produced 0666 (world-writable, umask skipped),
        // open produces 0664. The old code chmodded, which also meant a plain
        // O_CREAT that merely OPENED an existing file silently reset its
        // permissions — open has never done that, and nothing here wanted it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode & !alloyfs_proto::MODE_WIN_MASK);
        }
        let file = opts.open(&full).or_code()?;
        // Windows has no umask and no mode; `set_mode` only maps the write bit
        // onto the read-only attribute, so it still has to happen after open.
        #[cfg(windows)]
        alloyfs_common::set_mode(&file, mode);
        // Only interesting creation modes earn a sidecar entry — a Linux
        // client creating an executable must keep its x-bits, while the
        // ocean of 0o666 creates stays out of the store.
        #[cfg(windows)]
        if mode & 0o111 != 0 {
            export.posix_modes.put(&path, Some(mode & 0o7777));
        }
        let md = file.metadata().or_code()?;
        export.events.note_local_write(&path, self.id);
        let attr = export.with_winattrs(&path, attr_from_metadata(&md, export.bump(&path)));
        let fh = self.insert_handle(file, path, true);
        Ok(Response::Opened { fh, attr })
    }

    /// v8+: several whole files in one reply.
    ///
    /// Fills up to `budget` bytes and stops, so the reply is always a PREFIX
    /// of the request and the client can work out what is outstanding without
    /// a cursor. One awkward case decides the shape: a single file bigger than
    /// the whole budget would otherwise serve zero entries and leave the
    /// client asking for the same thing forever, so it is answered
    /// `TooLarge` — a per-entry outcome, not a failed request, which also
    /// keeps one unreadable file from costing the whole batch.
    fn read_many(&self, paths: Vec<RelPath>, budget: u32) -> Result<Response, ErrorCode> {
        let export = self.export()?;
        // The client's budget is a request, not an instruction: the reply has
        // to fit MAX_FRAME_LEN whatever it asks for.
        let budget = budget.min(READ_MANY_BUDGET) as u64;
        let mut spent = 0u64;
        let mut entries = Vec::with_capacity(paths.len().min(READ_MANY_ENTRIES));

        for path in paths.into_iter().take(READ_MANY_ENTRIES) {
            let full = match export.resolve(&path) {
                Ok(f) => f,
                Err(e) => {
                    entries.push(ManyEntry::Skipped(e));
                    continue;
                }
            };
            let md = match std::fs::metadata(&full) {
                Ok(md) if md.is_file() => md,
                Ok(_) => {
                    entries.push(ManyEntry::Skipped(ErrorCode::IsADirectory));
                    continue;
                }
                Err(e) => {
                    entries.push(ManyEntry::Skipped(alloyfs_common::io_to_code(&e)));
                    continue;
                }
            };
            let size = md.len();
            if size > budget {
                // Never fits, whatever we do: say so and let the client read
                // it the chunked way rather than retry a request that cannot
                // succeed.
                entries.push(ManyEntry::Skipped(ErrorCode::TooLarge));
                continue;
            }
            if spent + size > budget && !entries.is_empty() {
                break; // out of room; the client asks for the rest
            }
            let mut buf = vec![0u8; size as usize];
            let n = match read_fully(&File::open(&full).or_code()?, &mut buf, 0) {
                Ok(n) => n,
                Err(e) => {
                    entries.push(ManyEntry::Skipped(alloyfs_common::io_to_code(&e)));
                    continue;
                }
            };
            buf.truncate(n);
            spent += n as u64;
            let version = export.version_of(&path);
            entries.push(ManyEntry::File {
                attr: export.with_winattrs(&path, attr_from_metadata(&md, version)),
                data: buf.into(),
            });
        }
        Ok(Response::Many(entries))
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
        // A client that sends `expect_version` is asking to be STOPPED, not
        // merely told: reporting a conflict after the bytes have landed is a
        // notification that the data it was protecting is already gone. So a
        // mismatch refuses the write and nothing is written.
        //
        // Only opt-in clients are affected. `expect_version: None` — every
        // client before this, and every mount without --detect-conflicts —
        // keeps the old last-writer-wins behaviour untouched.
        if matches!(expect_version, Some(v) if v != export.version_of(&of.path)) {
            tracing::warn!(
                path = %of.path,
                expected = ?expect_version,
                actual = export.version_of(&of.path),
                "refusing a write over a concurrent modification"
            );
            return Err(ErrorCode::Conflict);
        }
        write_fully(&of.file, &data, offset).or_code()?;
        export.events.note_local_write(&of.path, self.id);
        let new_version = export.bump(&of.path);
        let n = data.len() as u32;

        // v5+: answer with the attributes the client would otherwise come
        // back for. One fstat on a handle already open costs microseconds
        // here and saves a full round-trip there.
        //
        // The mtime is the one field this can report slightly early: Windows
        // does not necessarily flush a file's timestamp to the FCB until the
        // handle is closed, so a rapid rewrite may report the previous one.
        // Freshness is decided by `version` (bumped above, always exact) and
        // never by mtime, and the alternative every backend used until now —
        // keeping the PRE-write attr and patching its size — was strictly
        // more stale. A failed stat simply falls back to the old shape, which
        // a v5 client also accepts.
        if self.proto.load(Ordering::Relaxed) >= 5 {
            if let Ok(md) = of.file.metadata() {
                return Ok(Response::WrittenAttr {
                    n,
                    attr: export.with_winattrs(&of.path, attr_from_metadata(&md, new_version)),
                });
            }
            tracing::debug!(path = %of.path, "post-write stat failed; replying without attributes");
        }
        Ok(Response::Written {
            n,
            new_version,
            // Always false now: a conflict returns above rather than writing
            // and flagging. The field is frozen by the append-only wire rule
            // and no client reads it.
            conflict: false,
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
            // By PATH, not through a write-opened handle. Opening for write
            // was refused for exactly the file most likely to have its mode
            // changed: a READONLY one — Windows denies the write-open before
            // the chmod can run, so clearing the readonly bit was impossible
            // on a Windows server. A path chmod needs no write access on any
            // platform.
            set_mode_path(&full, mode).or_code()?;
            // A Windows filesystem holds only the write-bit projection of
            // that chmod; the full mode persists in the export's posix
            // sidecar so a Linux client's chmod survives verbatim — this is
            // what makes `chmod +x` real on a Windows server.
            #[cfg(windows)]
            if std::fs::metadata(&full).map(|m| m.is_file()).unwrap_or(false) {
                export.posix_modes.put(&path, Some(mode & 0o7777));
            }
        }
        let md = std::fs::metadata(&full).or_code()?;
        export.events.note_local_write(&path, self.id);
        Ok(Response::Attr(export.with_winattrs(
            &path,
            attr_from_metadata(&md, export.bump(&path)),
        )))
    }

    fn mkdir(&self, path: RelPath, _mode: u32) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let full = export.resolve_new(&path)?;
        std::fs::create_dir(&full).or_code()?; // directory modes: server umask decides
        let md = std::fs::metadata(&full).or_code()?;
        export.events.note_local_write(&path, self.id);
        Ok(Response::Attr(export.with_winattrs(
            &path,
            attr_from_metadata(&md, export.bump(&path)),
        )))
    }

    /// v10+: create-or-replace several WHOLE files in one exchange — the
    /// write-side sibling of `read_many`, fed by the client's batcher.
    ///
    /// Entries apply in order and answer individually: one refused path must
    /// not poison its neighbours, because the client acknowledged each of
    /// these to its application already and needs to know exactly which
    /// promises broke. Per entry this is `create`'s body with truncate
    /// semantics; the mode contract is create-only (an existing file keeps
    /// its own — `create_new` then an `AlreadyExists` fallback is what makes
    /// that knowable on Windows, where the mode is applied post-open).
    fn write_many(&self, files: Vec<alloyfs_proto::ManyWrite>) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        // One sidecar save for the whole batch, not one per executable.
        #[cfg(windows)]
        let _sidecar = export.posix_modes.batch();
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            out.push(self.write_one_whole(&export, f));
        }
        Ok(Response::ManyOutcome(out))
    }

    fn write_one_whole(
        &self,
        export: &Export,
        f: alloyfs_proto::ManyWrite,
    ) -> Result<Option<Attr>, ErrorCode> {
        f.path.validate()?;
        let full = export.resolve_new(&f.path)?;
        let create_attempt = {
            let mut o = File::options();
            o.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(f.mode & !alloyfs_proto::MODE_WIN_MASK);
            }
            o.open(&full)
        };
        let (mut file, created) = match create_attempt {
            Ok(file) => (file, true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let file = File::options().write(true).truncate(true).open(&full).or_code()?;
                (file, false)
            }
            other => (other.or_code()?, true),
        };
        #[cfg(windows)]
        if created {
            alloyfs_common::set_mode(&file, f.mode);
            // Same rule as create: x-bits earn a posix-sidecar entry so a
            // Linux client's batched executable stays executable.
            if f.mode & 0o111 != 0 {
                export.posix_modes.put(&f.path, Some(f.mode & 0o7777));
            }
        }
        #[cfg(not(windows))]
        let _ = created;
        use std::io::Write as _;
        file.write_all(&f.data).or_code()?;
        let md = file.metadata().or_code()?;
        export.events.note_local_write(&f.path, self.id);
        Ok(Some(export.with_winattrs(
            &f.path,
            attr_from_metadata(&md, export.bump(&f.path)),
        )))
    }

    /// v10+: several removals in one exchange, in order, answered per entry.
    fn remove_many(&self, entries: Vec<alloyfs_proto::ManyRemove>) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        // Sidecar removals coalesce to one save per batch on both platforms.
        let _wa = export.winattrs.batch();
        #[cfg(windows)]
        let _pm = export.posix_modes.batch();
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let one = (|| -> Result<Option<Attr>, ErrorCode> {
                e.path.validate()?;
                let full = export.resolve(&e.path)?;
                if e.dir {
                    std::fs::remove_dir(&full).or_code()?;
                } else {
                    std::fs::remove_file(&full).or_code()?;
                }
                export.events.note_local_write(&e.path, self.id);
                export.forget_version(&e.path);
                export.sidecars_remove(&e.path);
                Ok(None)
            })();
            out.push(one);
        }
        Ok(Response::ManyOutcome(out))
    }

    /// v10+: several metadata changes in one exchange — `setattr2`'s body per
    /// entry, so the readonly-onto-mode mapping stays server-side and atomic.
    fn setattr_many(&self, entries: Vec<alloyfs_proto::ManySetattr>) -> Result<Response, ErrorCode> {
        // chmod -R shape: one sidecar save for the whole sweep. The export
        // binding keeps the Arc alive for the guard borrowing into it.
        #[cfg(windows)]
        let export = self.export()?;
        #[cfg(windows)]
        let _sidecar = export.posix_modes.batch();
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            out.push(match self.setattr2(e.path, e.size, e.mtime, e.mode, e.readonly) {
                Ok(Response::Attr(a)) => Ok(Some(a)),
                Ok(_) => Err(ErrorCode::Io),
                Err(c) => Err(c),
            });
        }
        Ok(Response::ManyOutcome(out))
    }

    /// v11+: change the Windows attribute bits — natively where the agent
    /// runs on Windows, through the `.alloyfs` sidecar on Linux. `set` and
    /// `clear` are masked intents; the reply carries the post-change attrs,
    /// bits included (this session is v11+ by definition of having decoded
    /// the request, so nothing strips them).
    fn set_win_attrs(&self, path: RelPath, set: u32, clear: u32) -> Result<Response, ErrorCode> {
        use alloyfs_proto::MODE_WIN_MASK;
        let export = self.writable_export()?;
        let full = export.resolve(&path)?;
        #[cfg(windows)]
        crate::winattr::WinAttrs::apply_native(&full, set & MODE_WIN_MASK, clear & MODE_WIN_MASK)
            .or_code()?;
        #[cfg(unix)]
        export
            .winattrs
            .apply(&path, set & MODE_WIN_MASK, clear & MODE_WIN_MASK);
        let md = std::fs::metadata(&full).or_code()?;
        export.events.note_local_write(&path, self.id);
        let attr = attr_from_metadata(&md, export.bump(&path));
        Ok(Response::Attr(export.with_winattrs(&path, attr)))
    }

    fn unlink(&self, path: RelPath) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let full = export.resolve(&path)?;
        std::fs::remove_file(&full).or_code()?;
        export.events.note_local_write(&path, self.id);
        export.forget_version(&path);
        export.sidecars_remove(&path);
        Ok(Response::Ok)
    }

    fn rmdir(&self, path: RelPath) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let full = export.resolve(&path)?;
        std::fs::remove_dir(&full).or_code()?;
        export.events.note_local_write(&path, self.id);
        export.forget_version(&path);
        export.sidecars_remove(&path);
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
        export.sidecars_rename(&from, &to);
        Ok(Response::Ok)
    }

    fn link(&self, target: RelPath, link: RelPath) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let target_full = export.resolve(&target)?;
        let link_full = export.resolve_new(&link)?;
        std::fs::hard_link(&target_full, &link_full).or_code()?;
        let md = std::fs::metadata(&link_full).or_code()?;
        export.events.note_local_write(&link, self.id);
        Ok(Response::Attr(export.with_winattrs(
            &link,
            attr_from_metadata(&md, export.bump(&link)),
        )))
    }

    /// Create a symlink, refusing any that would point out of the export.
    ///
    /// The check is on where the link LANDS, not on whether the target
    /// currently exists — a dangling symlink is legal and must stay creatable,
    /// but a dangling symlink to `../../etc/passwd` must not, because the day
    /// that path appears it becomes a hole in the export boundary.
    fn symlink(&self, target: String, link: RelPath) -> Result<Response, ErrorCode> {
        let export = self.writable_export()?;
        let link_full = export.resolve_new(&link)?;

        let landing = crate::fsutil::symlink_lands_inside(&link, &target).ok_or_else(|| {
            tracing::warn!(%link, target, "refusing a symlink that escapes the export");
            ErrorCode::PermissionDenied
        })?;
        // Where it lands must also survive the export's excludes: a link is
        // otherwise a trivial way to read a path the config says is invisible.
        if export.exclude.is_excluded(&RelPath(landing.clone())) {
            tracing::warn!(%link, target, "refusing a symlink into an excluded path");
            return Err(ErrorCode::PermissionDenied);
        }

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link_full).or_code()?;
        #[cfg(windows)]
        {
            // Windows needs to know at creation time whether the target is a
            // directory, and cannot be told later. An absent target is
            // assumed to be a file: that is the common case, and a dangling
            // link is what the caller asked for.
            let resolved = export.root.join(landing.replace('/', "\\"));
            if resolved.is_dir() {
                std::os::windows::fs::symlink_dir(&target, &link_full).or_code()?;
            } else {
                std::os::windows::fs::symlink_file(&target, &link_full).or_code()?;
            }
        }

        let md = std::fs::symlink_metadata(&link_full).or_code()?;
        export.events.note_local_write(&link, self.id);
        Ok(Response::Attr(export.with_winattrs(
            &link,
            attr_from_metadata(&md, export.bump(&link)),
        )))
    }

    /// Read a symlink's target, verbatim.
    ///
    /// Uses `resolve_new` rather than `resolve`: `resolve` canonicalizes,
    /// which follows the very link we are being asked to describe, and would
    /// fail outright on a dangling one. The parent is still resolved and
    /// checked, so the link itself cannot be reached from outside the export.
    fn readlink(&self, path: RelPath) -> Result<Response, ErrorCode> {
        let export = self.export()?;
        let full = export.resolve_new(&path)?;
        let md = std::fs::symlink_metadata(&full).or_code()?;
        if !md.file_type().is_symlink() {
            return Err(ErrorCode::InvalidPath);
        }
        let target = std::fs::read_link(&full).or_code()?;
        let target = target.to_str().ok_or(ErrorCode::InvalidPath)?;
        // Normalize separators so a Windows-created link reads the same to a
        // Unix client; the wire form is always forward slashes.
        Ok(Response::Target(target.replace('\\', "/")))
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
            Request::Attach2 { export } => self.attach2(export),
            Request::OpenRead { path, flags, len } => self.open_read(path, flags, len),
            Request::Getattr { path } => self.getattr(path),
            Request::Readdir { path, cursor } => self.readdir(path, cursor),
            Request::Tree { path, cursor } => self.tree(path, cursor),
            Request::TreeToken => self.tree_token(),
            Request::Open { path, flags } => self.open(path, flags),
            Request::Create { path, flags, mode } => self.create(path, flags, mode),
            Request::Read { fh, offset, len } => self.read(fh, offset, len),
            Request::ReadMany { paths, budget } => self.read_many(paths, budget),
            Request::WriteMany { files } => self.write_many(files),
            Request::RemoveMany { entries } => self.remove_many(entries),
            Request::SetattrMany { entries } => self.setattr_many(entries),
            Request::SetWinAttrs { path, set, clear } => self.set_win_attrs(path, set, clear),
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
            Request::Setattr2 {
                path,
                size,
                mtime,
                mode,
                readonly,
            } => self.setattr2(path, size, mtime, mode, readonly),
            Request::Mkdir { path, mode } => self.mkdir(path, mode),
            Request::Unlink { path } => self.unlink(path),
            Request::Rmdir { path } => self.rmdir(path),
            Request::Rename { from, to, replace } => self.rename(from, to, replace),
            Request::Link { target, link } => self.link(target, link),
            Request::Symlink { target, link } => self.symlink(target, link),
            Request::ReadLink { path } => self.readlink(path),
            Request::Statfs => self.statfs(),
            Request::MountDefaults => self.mount_defaults(),
            // Handled in async context (handle()); never reach the pool.
            Request::Lock { .. }
            | Request::Unlock { .. }
            | Request::LockRange { .. }
            | Request::UnlockRange { .. }
            | Request::TestLock { .. }
            | Request::Subscribe { .. }
            | Request::Auth { .. } => Err(ErrorCode::Io),
        }
    }
}

/// Mask the v11 `MODE_WIN_*` bits out of every Attr a reply carries.
///
/// Exhaustive over `Response` on purpose: a future variant that carries an
/// Attr must decide here whether it strips, exactly as the golden-wire test
/// forces a decision about encoding.
fn strip_win_bits(resp: Response) -> Response {
    use alloyfs_proto::MODE_WIN_MASK;
    fn clean(a: Attr) -> Attr {
        Attr {
            mode: a.mode & !MODE_WIN_MASK,
            ..a
        }
    }
    match resp {
        Response::Attr(a) => Response::Attr(clean(a)),
        Response::AttachOk { export_id, root_attr } => Response::AttachOk {
            export_id,
            root_attr: clean(root_attr),
        },
        Response::Attached2 {
            export_id,
            root_attr,
            exclude,
            pin,
            auto_cache_max,
            auto_cache_budget,
            tree_token,
        } => Response::Attached2 {
            export_id,
            root_attr: clean(root_attr),
            exclude,
            pin,
            auto_cache_max,
            auto_cache_budget,
            tree_token,
        },
        Response::Dir {
            mut entries,
            next_cursor,
        } => {
            for e in &mut entries {
                e.attr = clean(e.attr);
            }
            Response::Dir { entries, next_cursor }
        }
        Response::Tree {
            mut entries,
            next_cursor,
            token,
        } => {
            for e in &mut entries {
                e.attr = clean(e.attr);
            }
            Response::Tree {
                entries,
                next_cursor,
                token,
            }
        }
        Response::Opened { fh, attr } => Response::Opened {
            fh,
            attr: clean(attr),
        },
        Response::OpenedData { fh, attr, data } => Response::OpenedData {
            fh,
            attr: clean(attr),
            data,
        },
        Response::WrittenAttr { n, attr } => Response::WrittenAttr { n, attr: clean(attr) },
        Response::Many(mut entries) => {
            for e in &mut entries {
                if let ManyEntry::File { attr, .. } = e {
                    *attr = clean(*attr);
                }
            }
            Response::Many(entries)
        }
        Response::ManyOutcome(mut outcomes) => {
            for o in &mut outcomes {
                if let Ok(Some(attr)) = o {
                    *attr = clean(*attr);
                }
            }
            Response::ManyOutcome(outcomes)
        }
        // No Attr inside: pass through untouched.
        r @ (Response::Data(_)
        | Response::Written { .. }
        | Response::Statfs { .. }
        | Response::Subscribed { .. }
        | Response::Ok
        | Response::MountDefaults { .. }
        | Response::Target(_)
        | Response::TreeToken { .. }
        | Response::LockStatus(_)) => r,
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
        let resp = match req {
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
            // v7+. The end is computed rather than added because `len == 0` is
            // fcntl's "to the end of the file, however large it grows" —
            // `start + 0` would be an empty range, which is not what was asked.
            Request::LockRange {
                fh,
                owner,
                kind,
                start,
                len,
                wait,
            } => {
                let inner = &self.inner;
                let export = inner.export()?;
                let path = inner.handle_of(fh)?.path.clone();
                let owner = crate::locks::Owner::new(inner.id, fh, owner);
                export
                    .locks
                    .lock_range(&path, owner, kind, start, range_end(start, len), wait)
                    .await?;
                Ok(Response::Ok)
            }
            Request::UnlockRange {
                fh,
                owner,
                start,
                len,
            } => {
                let inner = &self.inner;
                let export = inner.export()?;
                let path = inner.handle_of(fh)?.path.clone();
                let owner = crate::locks::Owner::new(inner.id, fh, owner);
                export
                    .locks
                    .unlock_range(&path, owner, start, range_end(start, len));
                Ok(Response::Ok)
            }
            Request::TestLock {
                fh,
                owner,
                kind,
                start,
                len,
            } => {
                let inner = &self.inner;
                let export = inner.export()?;
                let path = inner.handle_of(fh)?.path.clone();
                let owner = crate::locks::Owner::new(inner.id, fh, owner);
                Ok(Response::LockStatus(export.locks.test_range(
                    &path,
                    owner,
                    kind,
                    start,
                    range_end(start, len),
                )))
            }
            req => {
                let inner = self.inner.clone();
                tokio::task::spawn_blocking(move || inner.dispatch_blocking(req))
                    .await
                    .map_err(|_| ErrorCode::Io)?
            }
        };
        // The v11 gate, server half: a session below 11 must never see the
        // MODE_WIN_* high bits — an older peer reads mode as pure POSIX
        // permissions, and a bit it cannot name would corrupt every st_mode
        // it fabricates. One boundary, every reply.
        if self.inner.proto.load(Ordering::Relaxed) < 11 {
            return resp.map(strip_win_bits);
        }
        resp
    }

    async fn negotiated(&self, proto: u16) {
        self.inner.proto.store(proto, Ordering::Relaxed);
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

/// Exclusive end of a lock range, in `fcntl`'s terms: `l_len == 0` means "to
/// the end of the file, however large it grows", not an empty range.
fn range_end(start: u64, len: u64) -> u64 {
    if len == 0 {
        u64::MAX
    } else {
        start.saturating_add(len)
    }
}
