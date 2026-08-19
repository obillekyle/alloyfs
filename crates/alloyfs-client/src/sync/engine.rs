//! The sync runtime: one sequential executor owns the manifest and performs
//! every mutation; the event pump (remote→local) and the local watcher
//! (local→remote) only enqueue work. Sequential execution is a deliberate
//! v1 simplification — it makes per-path ordering trivially correct; the
//! bounded-parallel transfer pool can come later if initial syncs of huge
//! trees need it.
//!
//! Echo suppression (the one genuinely subtle piece): every mutation the
//! engine makes to the local tree first records what the result should look
//! like (size+mtime, or absence). When the local watcher later reports that
//! path, the executor stats the file and compares: an exact match within the
//! TTL is our own echo and is dropped; anything else is a real human edit
//! and is pushed. This is stricter than time-window suppression — a human
//! save inside the window still syncs unless it produced byte-identical
//! size and identical nanosecond mtime (in which case skipping is harmless).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use alloyfs_common::coalesce::Coalescer;
use alloyfs_common::ExcludeSet;
use alloyfs_proto::{ErrorCode, EventKind, FsEvent, RelPath, Request, Response};
use alloyfs_transport::MuxConnection;
use dashmap::DashMap;
use notify::Watcher as _;
use tokio::sync::mpsc;

use super::manifest::{mtime_ns_of, EntryKind, SyncEntry, SyncManifest};
use super::reconcile::{plan, snapshot_local, snapshot_remote, Action, Stat};
use super::transfer::{conflict_name, download_to, upload};
use super::{is_internal, ConflictPolicy, SyncOptions, SyncStats};
use crate::FsError;

/// How long a recorded expectation suppresses watcher echoes.
const SUPPRESS_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    Present {
        size: u64,
        mtime_ns: u128,
    },
    Absent,
    /// Directories: existence is the only observable (no size/mtime check).
    Dir,
}

struct Suppress {
    expect: Expect,
    at: Instant,
}

enum Op {
    Remote(Vec<FsEvent>),
    Local(Vec<(RelPath, EventKind)>),
    Reconcile,
}

pub struct SyncEngine {
    conn: RwLock<Arc<MuxConnection>>,
    root: PathBuf,
    export: String,
    exclude: ExcludeSet,
    manifest: Mutex<SyncManifest>,
    manifest_path: PathBuf,
    suppress: DashMap<String, Suppress>,
    policy: ConflictPolicy,
    dialer: Option<crate::options::Dialer>,
    conn_epoch: tokio::sync::watch::Sender<u64>,
    op_tx: mpsc::UnboundedSender<Op>,
    pub stats: SyncStats,
    /// Set by `shutdown`: refuse new work and let the loops exit. Without it
    /// a "stopped" engine keeps watching and mutating (its tasks hold Arc
    /// clones), so two engines over one directory fight each other.
    stopped: std::sync::atomic::AtomicBool,
    /// Keeps the OS watcher alive; dropped by `shutdown` to end the watch.
    watcher: Mutex<Option<Box<dyn std::any::Any + Send>>>,
}

impl SyncEngine {
    /// Attach, reconcile, and (unless one_shot) start the watchers. Returns
    /// once the initial reconcile has been ENQUEUED; use `wait_quiescent`
    /// (tests) or just let it run (CLI).
    pub async fn start(
        conn: Arc<MuxConnection>,
        export: &str,
        local_dir: &Path,
        opts: SyncOptions,
    ) -> Result<Arc<Self>, FsError> {
        std::fs::create_dir_all(local_dir).map_err(|_| ErrorCode::Io)?;
        let root = std::fs::canonicalize(local_dir).map_err(|_| ErrorCode::Io)?;
        match conn
            .request(Request::Attach {
                export: export.into(),
            })
            .await??
        {
            Response::AttachOk { .. } => {}
            _ => return Err(ErrorCode::Io.into()),
        }
        let exclude = ExcludeSet::compile_with_defaults(&opts.excludes, cfg!(windows))
            .map_err(|_| ErrorCode::InvalidPath)?;
        let manifest_path = opts
            .data_dir
            .join("sync")
            .join(format!("{}.manifest.json", opts.sync_key));
        let manifest = SyncManifest::load(&manifest_path);
        sweep_staging(&root);

        // Receiver BEFORE Subscribe (broadcast drops events with no receiver).
        let rx = conn.events();
        let since = if manifest.last_seq == 0 {
            None
        } else {
            Some(manifest.last_seq)
        };
        let mut force_reconcile_reason: Option<&str> = None;
        let subscribed = match conn.request(Request::Subscribe { since_seq: since }).await? {
            Ok(Response::Subscribed { last_seq }) => last_seq,
            Err(ErrorCode::TooOld) => {
                force_reconcile_reason = Some("event history expired");
                match conn.request(Request::Subscribe { since_seq: None }).await?? {
                    Response::Subscribed { last_seq } => last_seq,
                    _ => return Err(ErrorCode::Io.into()),
                }
            }
            Ok(_) => return Err(ErrorCode::Io.into()),
            Err(e) => return Err(e.into()),
        };
        if subscribed < manifest.last_seq {
            // Server sequence went backwards: agent restarted, versions reset.
            force_reconcile_reason = Some("server restarted");
        }
        if let Some(reason) = force_reconcile_reason {
            tracing::warn!(reason, "full reconcile required");
        }

        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let (epoch_tx, _) = tokio::sync::watch::channel(0u64);
        let engine = Arc::new(Self {
            conn: RwLock::new(conn),
            root,
            export: export.to_string(),
            exclude,
            manifest: Mutex::new(manifest),
            manifest_path,
            suppress: DashMap::new(),
            policy: opts.conflict_policy,
            dialer: opts.dialer.clone(),
            conn_epoch: epoch_tx,
            op_tx,
            stats: SyncStats::default(),
            stopped: std::sync::atomic::AtomicBool::new(false),
            watcher: Mutex::new(None),
        });

        // Watch BEFORE the first reconcile: the reconcile can create local
        // files (conflict copies, pulled files), and a watcher installed
        // afterwards would never see them — they'd sit unpushed until some
        // later edit. Echoes of our own applies are handled by the
        // suppression map, so watching early costs nothing.
        if !opts.one_shot {
            engine.start_local_watcher(opts.debounce)?;
        }
        engine.enqueue(Op::Reconcile);
        tokio::spawn(executor(engine.clone(), op_rx));
        tokio::spawn(event_pump(engine.clone(), rx));
        if engine.dialer.is_some() {
            tokio::spawn(reconnect_supervisor(engine.clone()));
        }
        Ok(engine)
    }

    /// The server version this baseline recorded for `path`, if any.
    ///
    /// Narrower than `baseline_debug` and meant for assertions rather than
    /// eyeballing: the version is what decides whether a delete is sent or
    /// turned into a pull, so a test can check it directly instead of
    /// reproducing the race that made it matter.
    pub fn baseline_version(&self, path: &str) -> Option<u64> {
        self.manifest
            .lock()
            .unwrap()
            .entries
            .get(path)
            .map(|e| e.remote_version)
    }

    /// The sync baseline as text, one path per line, sorted.
    ///
    /// The baseline decides whether a local delete is pushed at all:
    /// `push_removal` treats a path with no entry as "never synced, nothing
    /// to delete remotely" and returns without asking the server. So when a
    /// delete appears to do nothing, this is the first thing worth looking
    /// at — and it is otherwise invisible from outside the engine, which is
    /// what made an intermittent CI failure cost a whole investigation with
    /// nothing to show for it.
    ///
    /// A snapshot for diagnostics, not a handle for control.
    pub fn baseline_debug(&self) -> String {
        let m = self.manifest.lock().unwrap();
        let mut out = format!(
            "baseline: {} entries (last_seq {})\n",
            m.entries.len(),
            m.last_seq
        );
        for (path, entry) in &m.entries {
            out.push_str(&format!(
                "    {path}  kind={:?} size={} mtime={} ver={}\n",
                entry.kind, entry.size, entry.mtime_ns, entry.remote_version
            ));
        }
        out
    }

    /// True once every accepted op has been fully processed.
    pub fn is_quiescent(&self) -> bool {
        self.stats.pending.load(Relaxed) == 0
    }

    /// Stop syncing and persist the baseline. After this returns the engine
    /// accepts no new work and its watcher is gone; in-flight operations
    /// finish, but nothing new starts.
    pub fn shutdown(&self) {
        self.stopped.store(true, Relaxed);
        // Dropping the notify watcher closes its channel, which ends the
        // local-watch loop.
        let _ = self.watcher.lock().unwrap().take();
        let m = self.manifest.lock().unwrap();
        m.flush(&self.manifest_path);
        tracing::info!(
            pulls = self.stats.pulls.load(Relaxed),
            pushes = self.stats.pushes.load(Relaxed),
            conflicts = self.stats.conflicts.load(Relaxed),
            "sync engine stopped"
        );
    }

    fn enqueue(&self, op: Op) {
        if self.stopped.load(Relaxed) {
            return;
        }
        self.stats.pending.fetch_add(1, Relaxed);
        if self.op_tx.send(op).is_err() {
            self.stats.pending.fetch_sub(1, Relaxed);
        }
    }

    fn conn(&self) -> Arc<MuxConnection> {
        self.conn.read().unwrap().clone()
    }

    fn full(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for comp in rel.split('/') {
            p.push(comp);
        }
        p
    }

    fn local_stat(&self, rel: &str) -> Option<Stat> {
        let md = std::fs::symlink_metadata(self.full(rel)).ok()?;
        Some(if md.is_dir() {
            Stat {
                kind: EntryKind::Dir,
                size: 0,
                mtime_ns: 0,
                version: 0,
            }
        } else {
            Stat {
                kind: EntryKind::File,
                size: md.len(),
                mtime_ns: md.modified().map(mtime_ns_of).unwrap_or(0),
                version: 0,
            }
        })
    }

    /// Record what the local tree will look like after our own imminent
    /// mutation, so the watcher's echo is recognized and dropped.
    fn expect(&self, rel: &str, expect: Expect) {
        self.suppress.insert(
            rel.to_string(),
            Suppress {
                expect,
                at: Instant::now(),
            },
        );
    }

    /// Is this watcher-reported path our own recent mutation? (Stat-compare,
    /// not just time-window — see module docs.)
    fn is_own_echo(&self, rel: &str) -> bool {
        let Some(entry) = self.suppress.get(rel) else {
            return false;
        };
        if entry.at.elapsed() > SUPPRESS_TTL {
            drop(entry);
            self.suppress.remove(rel);
            return false;
        }
        // Entry stays until TTL: one mutation can echo several raw events.
        match (entry.expect, self.local_stat(rel)) {
            (Expect::Absent, None) => true,
            (Expect::Dir, Some(s)) => s.kind == EntryKind::Dir,
            (Expect::Present { size, mtime_ns }, Some(s)) => {
                s.kind == EntryKind::File && s.size == size && s.mtime_ns == mtime_ns
            }
            _ => false,
        }
    }

    fn record(&self, rel: &str, kind: EntryKind, size: u64, mtime_ns: u128, version: u64) {
        self.manifest.lock().unwrap().entries.insert(
            rel.to_string(),
            SyncEntry {
                kind,
                remote_version: version,
                size,
                mtime_ns,
            },
        );
    }

    // ------------------------------------------------------ remote → local

    async fn apply_remote(&self, ev: &FsEvent) -> Result<(), FsError> {
        let rel = ev.path.0.as_str();
        if self.exclude.is_excluded(&ev.path) || is_internal(rel) {
            return Ok(());
        }
        match &ev.kind {
            EventKind::Created | EventKind::Modified | EventKind::AttrChanged => {
                self.pull_if_needed(rel).await
            }
            EventKind::Removed => self.delete_local(rel).await,
            EventKind::RenamedFrom { to } => {
                let to_rel = to.0.clone();
                let from_full = self.full(rel);
                let to_full = self.full(&to_rel);
                if from_full.exists() {
                    self.expect(rel, Expect::Absent);
                    if let Some(parent) = to_full.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::rename(&from_full, &to_full).is_ok() {
                        if let Some(s) = self.local_stat(&to_rel) {
                            self.expect(
                                &to_rel,
                                match s.kind {
                                    EntryKind::Dir => Expect::Dir,
                                    EntryKind::File => Expect::Present {
                                        size: s.size,
                                        mtime_ns: s.mtime_ns,
                                    },
                                },
                            );
                        }
                        // A miss here means the local rename succeeded but the
                        // manifest gained no baseline for the target. Falling
                        // through to the pull below rebuilds one from the
                        // server, which is exactly the recovery this arm
                        // already has for the other ways it can lose track.
                        if self.manifest.lock().unwrap().rename_prefix(rel, &to_rel) {
                            return Ok(());
                        }
                    }
                }
                // Source missing or rename failed: converge by pulling `to`.
                self.pull_if_needed(&to_rel).await
            }
            EventKind::ResyncRequired => {
                self.enqueue(Op::Reconcile);
                Ok(())
            }
        }
    }

    /// Check-then-act pull: skip when the manifest already matches the
    /// current remote (stale event / our own catch-up echo), route to the
    /// conflict path when the LOCAL side is dirty too.
    async fn pull_if_needed(&self, rel: &str) -> Result<(), FsError> {
        let conn = self.conn();
        let attr = match conn
            .request(Request::Getattr {
                path: RelPath(rel.to_string()),
            })
            .await?
        {
            Ok(Response::Attr(attr)) => attr,
            Ok(_) => return Err(ErrorCode::Io.into()),
            Err(ErrorCode::NotFound) => return Ok(()), // already gone again
            Err(e) => return Err(e.into()),
        };
        if attr.kind == alloyfs_proto::FileKind::Dir {
            self.expect(rel, Expect::Dir);
            std::fs::create_dir_all(self.full(rel)).map_err(|_| ErrorCode::Io)?;
            self.record(rel, EntryKind::Dir, 0, 0, attr.version);
            return Ok(());
        }
        let remote = Stat {
            kind: EntryKind::File,
            size: attr.size,
            mtime_ns: mtime_ns_of(attr.mtime),
            version: attr.version,
        };
        let baseline = self.manifest.lock().unwrap().entries.get(rel).cloned();
        if let Some(b) = &baseline {
            if b.size == remote.size && b.mtime_ns == remote.mtime_ns {
                return Ok(()); // manifest already reflects this remote state
            }
        }
        // Local dirty too? (differs from baseline) → conflict, not blind pull.
        let local = self.local_stat(rel);
        let local_dirty = match (&baseline, &local) {
            (Some(b), Some(l)) => !(l.kind == b.kind && l.size == b.size && l.mtime_ns == b.mtime_ns),
            (None, Some(_)) => true,
            _ => false,
        };
        if local_dirty {
            if let Some(l) = local {
                if l.size == remote.size && l.mtime_ns == remote.mtime_ns {
                    // Already identical: adopt.
                    self.record(rel, l.kind, l.size, l.mtime_ns, remote.version);
                    return Ok(());
                }
                return self.resolve_conflict(rel, l, remote).await;
            }
        }
        self.pull(rel).await
    }

    /// Unconditional download into place (staging + mtime stamp + rename).
    async fn pull(&self, rel: &str) -> Result<(), FsError> {
        let full = self.full(rel);
        let parent = full.parent().ok_or(ErrorCode::InvalidPath)?;
        std::fs::create_dir_all(parent).map_err(|_| ErrorCode::Io)?;
        let name = full
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(ErrorCode::InvalidPath)?;
        let staging = parent.join(format!(".ds-tmp-{name}"));
        let conn = self.conn();
        let attr = download_to(&conn, rel, &staging).await.inspect_err(|_| {
            let _ = std::fs::remove_file(&staging);
        })?;
        let mtime_ns = mtime_ns_of(attr.mtime);
        self.expect(
            rel,
            Expect::Present {
                size: attr.size,
                mtime_ns,
            },
        );
        std::fs::rename(&staging, &full).map_err(|_| ErrorCode::Io)?;
        self.record(rel, EntryKind::File, attr.size, mtime_ns, attr.version);
        self.stats.pulls.fetch_add(1, Relaxed);
        Ok(())
    }

    async fn delete_local(&self, rel: &str) -> Result<(), FsError> {
        let full = self.full(rel);
        let Some(stat) = self.local_stat(rel) else {
            self.manifest.lock().unwrap().entries.remove(rel);
            return Ok(());
        };
        // Local dirty (differs from baseline)? Edit wins over remote delete:
        // leave it — the watcher/reconcile will push it back.
        let baseline = self.manifest.lock().unwrap().entries.get(rel).cloned();
        if let Some(b) = baseline {
            let clean = stat.kind == b.kind
                && (stat.kind == EntryKind::Dir || (stat.size == b.size && stat.mtime_ns == b.mtime_ns));
            if !clean {
                tracing::info!(path = rel, "remote delete skipped: local edit wins");
                return Ok(());
            }
        }
        if stat.kind == EntryKind::Dir {
            // Suppress every child the recursive delete will touch.
            if let Ok(snap) = snapshot_local(&full, &ExcludeSet::default()) {
                for child in snap.keys() {
                    self.expect(&format!("{rel}/{child}"), Expect::Absent);
                }
            }
            self.expect(rel, Expect::Absent);
            std::fs::remove_dir_all(&full).map_err(|_| ErrorCode::Io)?;
            let mut m = self.manifest.lock().unwrap();
            let prefix = format!("{rel}/");
            m.entries.retain(|p, _| p != rel && !p.starts_with(&prefix));
        } else {
            self.expect(rel, Expect::Absent);
            std::fs::remove_file(&full).map_err(|_| ErrorCode::Io)?;
            self.manifest.lock().unwrap().entries.remove(rel);
        }
        self.stats.deletes_local.fetch_add(1, Relaxed);
        Ok(())
    }

    // ------------------------------------------------------ local → remote

    /// Local change → server: a dispatch table over the event kinds, one arm
    /// per method. Echo suppression is common to all of them and stays here.
    async fn push_local(&self, rel: &str, kind: &EventKind) -> Result<(), FsError> {
        if self.is_own_echo(rel) {
            return Ok(());
        }
        let conn = self.conn();
        match kind {
            EventKind::Created | EventKind::Modified | EventKind::AttrChanged => {
                self.push_upsert(&conn, rel).await
            }
            EventKind::Removed => self.push_removal(&conn, rel).await,
            EventKind::RenamedFrom { to } => self.push_rename(&conn, rel, to).await,
            // Remote-side instruction; nothing to send back.
            EventKind::ResyncRequired => Ok(()),
        }
    }

    /// A created, modified or attribute-changed local path: mkdir it or
    /// upload it — unless the remote moved since our baseline, which is a
    /// conflict rather than something to clobber.
    async fn push_upsert(&self, conn: &MuxConnection, rel: &str) -> Result<(), FsError> {
        let Some(local) = self.local_stat(rel) else {
            return Ok(()); // vanished before we got to it
        };
        if local.kind == EntryKind::Dir {
            match conn
                .request(Request::Mkdir {
                    path: RelPath(rel.to_string()),
                    mode: 0o755,
                })
                .await?
            {
                Ok(_) | Err(ErrorCode::AlreadyExists) => {}
                Err(e) => return Err(e.into()),
            }
            self.record(rel, EntryKind::Dir, 0, 0, 0);
            // inotify races adding watches on brand-new directories: children
            // created before the watch attached were never reported. Scan the
            // subtree and push whatever the manifest doesn't know about yet.
            self.push_new_dir_contents(rel).await;
            return Ok(());
        }
        // Remote moved since our baseline? Conflict — never clobber.
        let remote = match conn
            .request(Request::Getattr {
                path: RelPath(rel.to_string()),
            })
            .await?
        {
            Ok(Response::Attr(a)) => Some(a),
            Err(ErrorCode::NotFound) => None,
            Ok(_) => return Err(ErrorCode::Io.into()),
            Err(e) => return Err(e.into()),
        };
        if let Some(r) = &remote {
            let r_stat = Stat {
                kind: EntryKind::File,
                size: r.size,
                mtime_ns: mtime_ns_of(r.mtime),
                version: r.version,
            };
            let baseline = self.manifest.lock().unwrap().entries.get(rel).cloned();
            let remote_dirty = match &baseline {
                Some(b) => {
                    !(b.remote_version != 0 && b.remote_version == r.version)
                        && !(b.size == r_stat.size && b.mtime_ns == r_stat.mtime_ns)
                }
                None => true, // remote exists and we never synced it
            };
            if remote_dirty {
                if local.size == r_stat.size && local.mtime_ns == r_stat.mtime_ns {
                    self.record(rel, local.kind, local.size, local.mtime_ns, r.version);
                    return Ok(());
                }
                return self.resolve_conflict(rel, local, r_stat).await;
            }
        }
        self.push(rel).await
    }

    /// A locally deleted path: delete it on the server too, unless the remote
    /// changed since our baseline — a remote edit outranks a local delete and
    /// is pulled back instead.
    async fn push_removal(&self, conn: &MuxConnection, rel: &str) -> Result<(), FsError> {
        // Remote changed since baseline? Edit wins: pull instead.
        let baseline = self.manifest.lock().unwrap().entries.get(rel).cloned();
        let Some(b) = baseline else {
            return Ok(()); // never synced — nothing to delete remotely
        };
        let remote = conn
            .request(Request::Getattr {
                path: RelPath(rel.to_string()),
            })
            .await?;
        match remote {
            Err(ErrorCode::NotFound) => {
                self.manifest.lock().unwrap().entries.remove(rel);
                Ok(())
            }
            Ok(Response::Attr(a)) => {
                let fresh = (b.remote_version != 0 && b.remote_version == a.version)
                    || (b.size == a.size && b.mtime_ns == mtime_ns_of(a.mtime));
                if !fresh && a.kind != alloyfs_proto::FileKind::Dir {
                    tracing::info!(path = rel, "local delete skipped: remote edit wins");
                    return self.pull(rel).await;
                }
                let req = if a.kind == alloyfs_proto::FileKind::Dir {
                    Request::Rmdir {
                        path: RelPath(rel.to_string()),
                    }
                } else {
                    Request::Unlink {
                        path: RelPath(rel.to_string()),
                    }
                };
                match conn.request(req).await? {
                    Ok(_) | Err(ErrorCode::NotFound) => {}
                    Err(ErrorCode::NotEmpty) => {
                        tracing::warn!(path = rel, "remote rmdir skipped (not empty yet)");
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                }
                let mut m = self.manifest.lock().unwrap();
                let prefix = format!("{rel}/");
                m.entries.retain(|p, _| p != rel && !p.starts_with(&prefix));
                self.stats.deletes_remote.fetch_add(1, Relaxed);
                Ok(())
            }
            Ok(_) => Err(ErrorCode::Io.into()),
            Err(e) => Err(e.into()),
        }
    }

    /// A local rename: replay it on the server, then re-baseline the target
    /// from what the server now reports.
    async fn push_rename(&self, conn: &MuxConnection, rel: &str, to: &RelPath) -> Result<(), FsError> {
        match conn
            .request(Request::Rename {
                from: RelPath(rel.to_string()),
                to: to.clone(),
                replace: true,
            })
            .await?
        {
            Ok(_) => {
                let known = self.manifest.lock().unwrap().rename_prefix(rel, &to.0);
                // `rename_prefix` only MOVES keys, so a source the manifest
                // never held leaves the target unrecorded — while the server
                // has just performed the rename. That combination is
                // unrecoverable rather than merely stale: `push_removal`
                // treats "no baseline" as "never synced, nothing to delete
                // remotely" and returns without asking the server, so the
                // renamed file could never be deleted again from this side.
                if !known {
                    if let Some(s) = self.local_stat(&to.0) {
                        self.record(&to.0, s.kind, s.size, s.mtime_ns, 0);
                        tracing::debug!(
                            path = %to.0,
                            "rename target adopted into the manifest (source was unknown)"
                        );
                    }
                }

                // Re-stat the server and adopt what it now reports.
                //
                // A moved entry still describes the file under its OLD name:
                // the server bumps the version when it performs the rename,
                // and the recorded mtime is the local one, which need not
                // match what the server stored. So both legs of the freshness
                // test in `push_removal` fail — version differs, size+mtime
                // differs — and a later delete of this path is read as
                // "somebody else edited it", converted into a pull, and the
                // file comes back instead of going away.
                //
                // That is not hypothetical: it is what
                // `local_changes_pushed_live` caught in CI, with
                // deletes_remote=0 and the file present in both trees.
                if let Ok(Ok(Response::Attr(a))) = conn.request(Request::Getattr { path: to.clone() }).await {
                    self.record(
                        &to.0,
                        if a.kind == alloyfs_proto::FileKind::Dir {
                            EntryKind::Dir
                        } else {
                            EntryKind::File
                        },
                        a.size,
                        mtime_ns_of(a.mtime),
                        a.version,
                    );
                }
                self.stats.pushes.fetch_add(1, Relaxed);
                Ok(())
            }
            Err(ErrorCode::NotFound) => {
                // Source never made it remotely: push the target fresh
                // (direct upload — no async recursion).
                self.manifest.lock().unwrap().entries.remove(rel);
                match self.local_stat(&to.0) {
                    Some(s) if s.kind == EntryKind::Dir => {
                        match self
                            .conn()
                            .request(Request::Mkdir {
                                path: to.clone(),
                                mode: 0o755,
                            })
                            .await?
                        {
                            Ok(_) | Err(ErrorCode::AlreadyExists) => {
                                self.record(&to.0, EntryKind::Dir, 0, 0, 0);
                                Ok(())
                            }
                            Err(e) => Err(e.into()),
                        }
                    }
                    Some(_) => self.push(&to.0).await,
                    None => Ok(()),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Push everything under a freshly created local directory that the
    /// manifest doesn't know (the watcher may have missed it — see caller).
    async fn push_new_dir_contents(&self, rel: &str) {
        let Ok(snap) = snapshot_local(&self.full(rel), &self.exclude) else {
            return;
        };
        for (child, stat) in snap {
            let child_rel = format!("{rel}/{child}");
            let known = {
                let m = self.manifest.lock().unwrap();
                m.entries.get(&child_rel).map(|b| (b.kind, b.size, b.mtime_ns))
            };
            if known == Some((stat.kind, stat.size, stat.mtime_ns))
                || (stat.kind == EntryKind::Dir && known.is_some_and(|(k, _, _)| k == EntryKind::Dir))
            {
                continue;
            }
            let outcome = if stat.kind == EntryKind::Dir {
                match self
                    .conn()
                    .request(Request::Mkdir {
                        path: RelPath(child_rel.clone()),
                        mode: 0o755,
                    })
                    .await
                {
                    Ok(Ok(_)) | Ok(Err(ErrorCode::AlreadyExists)) => {
                        self.record(&child_rel, EntryKind::Dir, 0, 0, 0);
                        Ok(())
                    }
                    Ok(Err(e)) => Err(FsError::from(e)),
                    Err(e) => Err(e.into()),
                }
            } else {
                self.push(&child_rel).await
            };
            if let Err(e) = outcome {
                tracing::warn!(path = %child_rel, error = %e, "new-dir child push failed");
            }
        }
    }

    /// Unconditional upload of the local file (parents pushed first by batch
    /// ordering / reconcile ordering).
    async fn push(&self, rel: &str) -> Result<(), FsError> {
        let conn = self.conn();
        // Stat BEFORE the upload, and record THAT.
        //
        // The baseline's whole job is to describe what was last sent. Taking
        // the stat afterwards described whatever the file happened to be by
        // then, and had two failure modes, both silent:
        //
        // - The file changed during the upload: the baseline claims a
        //   generation that was never sent. Harmless on its own — the change
        //   raises another event and another push corrects it.
        // - The file VANISHED during the upload (a rename, most often): the
        //   post-upload stat returns None, `push` reports Io — after the
        //   upload has already succeeded — and the baseline is never updated
        //   at all. The server moves on and the manifest stays a generation
        //   behind, permanently.
        //
        // That second one is not a lost update, it is a lost DELETE later:
        // every subsequent local removal of the path compares a stale
        // baseline against a newer remote, reads it as delete-vs-edit, and
        // pulls the file back. A renamed-away file reappearing is what it
        // looks like from outside.
        //
        // If the file vanishes between this stat and the upload, `upload`'s
        // own read fails and nothing is recorded — the remote is untouched
        // and the two stay consistent, which is the outcome to want.
        let local = self.local_stat(rel).ok_or(ErrorCode::Io)?;
        let attr = upload(&conn, rel, &self.full(rel)).await?;
        self.record(rel, EntryKind::File, local.size, local.mtime_ns, attr.version);
        self.stats.pushes.fetch_add(1, Relaxed);
        Ok(())
    }

    // ---------------------------------------------------------- conflicts

    /// Both sides changed and differ. Winner per policy; the loser is
    /// ALWAYS preserved as a `.sync-conflict-*` file that itself syncs.
    async fn resolve_conflict(&self, rel: &str, local: Stat, remote: Stat) -> Result<(), FsError> {
        self.stats.conflicts.fetch_add(1, Relaxed);
        let local_wins = match self.policy {
            ConflictPolicy::Local => true,
            ConflictPolicy::Remote => false,
            // Ties go to the server — deterministic on both ends.
            ConflictPolicy::Newer => local.mtime_ns > remote.mtime_ns,
        };
        // A conflict copy that itself conflicts must not spawn
        // `x.sync-conflict-N.sync-conflict-N`: the file IS already a
        // preservation copy, so last-writer-wins is enough.
        if rel.contains(".sync-conflict-") {
            tracing::warn!(
                path = rel,
                local_wins,
                "conflict on a conflict copy; no further copy"
            );
            return if local_wins {
                self.push(rel).await
            } else {
                self.pull(rel).await
            };
        }
        let keep = conflict_name(rel);
        tracing::warn!(
            path = rel,
            local_wins,
            conflict_copy = %keep,
            "sync conflict (both sides changed)"
        );
        if local_wins {
            // Preserve the remote copy locally first, then overwrite it.
            let full = self.full(&keep);
            if let Some(parent) = full.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let staging = full.with_file_name(format!(
                ".ds-tmp-{}",
                full.file_name().and_then(|n| n.to_str()).unwrap_or("conflict")
            ));
            let conn = self.conn();
            let mut kept = false;
            if download_to(&conn, rel, &staging).await.is_ok() {
                kept = std::fs::rename(&staging, &full).is_ok();
            }
            let res = self.push(rel).await;
            // Push the preserved copy explicitly rather than waiting for the
            // watcher to notice it: during the startup reconcile there may
            // not be a watcher yet, and a conflict copy that never reaches
            // the other side is exactly the data loss this is preventing.
            if kept {
                if let Err(e) = self.push(&keep).await {
                    tracing::warn!(path = %keep, error = %e, "conflict copy not pushed");
                }
            }
            res
        } else {
            // Set the local copy aside with COPY, not rename: a rename would
            // reach the watcher as a rename pair and be replayed as a
            // SERVER-side rename — moving the winner's file. The pull then
            // overwrites the original in place.
            let kept = std::fs::copy(self.full(rel), self.full(&keep)).is_ok();
            let res = self.pull(rel).await;
            // Explicit push, for the same reason as the local-wins branch.
            if kept {
                if let Err(e) = self.push(&keep).await {
                    tracing::warn!(path = %keep, error = %e, "conflict copy not pushed");
                }
            }
            res
        }
    }

    // ---------------------------------------------------------- reconcile

    async fn reconcile(&self) -> Result<(), FsError> {
        let conn = self.conn();
        let remote = snapshot_remote(&conn, &self.exclude).await?;
        let root = self.root.clone();
        let exclude = self.exclude.clone();
        let local = tokio::task::spawn_blocking(move || snapshot_local(&root, &exclude))
            .await
            .map_err(|_| ErrorCode::Io)?
            .map_err(|_| ErrorCode::Io)?;
        let actions = {
            let m = self.manifest.lock().unwrap();
            plan(&m, &local, &remote)
        };
        if actions.is_empty() {
            return Ok(());
        }
        tracing::info!(n = actions.len(), "reconcile plan");
        // The count alone has twice been not enough to explain a wrong
        // outcome: every action can succeed and still leave the tree wrong if
        // the PLAN was wrong. At debug, say what it decided to do.
        tracing::debug!(?actions, "reconcile plan detail");
        // Non-deletes in path order (parents first); deletes afterwards,
        // children first.
        let (deletes, rest): (Vec<_>, Vec<_>) = actions
            .into_iter()
            .partition(|a| matches!(a, Action::DeleteLocal(..) | Action::DeleteRemote(..)));
        for action in rest {
            self.run_action(action).await;
        }
        for action in deletes.into_iter().rev() {
            self.run_action(action).await;
        }
        Ok(())
    }

    async fn run_action(&self, action: Action) {
        let outcome: Result<(), FsError> = match action.clone() {
            Action::Pull(p) => self.pull_if_needed(&p).await,
            Action::Push(p) => {
                let kind = match self.local_stat(&p) {
                    Some(s) if s.kind == EntryKind::Dir => EventKind::Created,
                    _ => EventKind::Modified,
                };
                self.push_local(&p, &kind).await
            }
            Action::DeleteLocal(p, _) => self.delete_local(&p).await,
            Action::DeleteRemote(p, _) => self.push_local(&p, &EventKind::Removed).await,
            Action::Adopt(p, s) => {
                self.record(&p, s.kind, s.size, s.mtime_ns, s.version);
                Ok(())
            }
            Action::Conflict { path, local, remote } => self.resolve_conflict(&path, local, remote).await,
            Action::Forget(p) => {
                self.manifest.lock().unwrap().entries.remove(&p);
                Ok(())
            }
        };
        if let Err(e) = outcome {
            tracing::warn!(?action, error = %e, "reconcile action failed (next pass retries)");
        }
    }

    // ------------------------------------------------------------ watcher

    fn start_local_watcher(self: &Arc<Self>, debounce: Duration) -> Result<(), FsError> {
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<notify::Event>();
        let config = notify::Config::default().with_follow_symlinks(false);
        let mut watcher = notify::RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = raw_tx.send(event);
                }
            },
            config,
        )
        .map_err(|_| ErrorCode::Io)?;
        watcher
            .watch(&self.root, notify::RecursiveMode::Recursive)
            .map_err(|_| ErrorCode::Io)?;
        *self.watcher.lock().unwrap() = Some(Box::new(watcher));

        let engine = self.clone();
        tokio::spawn(async move {
            let mut co = Coalescer::new(engine.root.clone(), engine.exclude.clone());
            loop {
                if engine.stopped.load(Relaxed) {
                    break;
                }
                let timeout = if co.is_empty() {
                    Duration::from_secs(3600)
                } else {
                    debounce
                };
                tokio::select! {
                    maybe = raw_rx.recv() => {
                        // None = the watcher was dropped by shutdown().
                        let Some(event) = maybe else { break };
                        co.ingest(event);
                    }
                    _ = tokio::time::sleep(timeout) => {
                        let batch: Vec<_> = co
                            .take_batch()
                            .into_iter()
                            .filter(|(p, _)| !is_internal(&p.0))
                            .collect();
                        if !batch.is_empty() {
                            engine.enqueue(Op::Local(batch));
                        }
                    }
                }
            }
        });
        Ok(())
    }
}

/// The single mutation loop: every filesystem/manifest change happens here.
async fn executor(engine: Arc<SyncEngine>, mut rx: mpsc::UnboundedReceiver<Op>) {
    while let Some(op) = rx.recv().await {
        if engine.stopped.load(Relaxed) {
            break;
        }
        match op {
            Op::Remote(batch) => {
                for ev in &batch {
                    if let Err(e) = engine.apply_remote(ev).await {
                        tracing::warn!(path = %ev.path, error = %e, "remote apply failed");
                    }
                    let mut m = engine.manifest.lock().unwrap();
                    m.last_seq = m.last_seq.max(ev.seq);
                }
            }
            Op::Local(batch) => {
                let mut failed = 0usize;
                for (path, kind) in &batch {
                    if let Err(e) = engine.push_local(&path.0, kind).await {
                        failed += 1;
                        tracing::warn!(path = %path, error = %e, "local push failed");
                    }
                }
                // A dropped push is a lost change; retrying it comes in the
                // next commit, once the reason it was unsafe is gone.
                let _ = failed;
            }
            Op::Reconcile => {
                if let Err(e) = engine.reconcile().await {
                    tracing::warn!(error = %e, "reconcile failed (will retry on next trigger)");
                }
            }
        }
        engine.manifest.lock().unwrap().flush(&engine.manifest_path);
        engine.stats.pending.fetch_sub(1, Relaxed);
    }
}

/// Forwards remote event batches to the executor; survives reconnects by
/// resubscribing from the last applied seq (same pattern as the mount pump).
async fn event_pump(engine: Arc<SyncEngine>, mut rx: tokio::sync::broadcast::Receiver<Vec<FsEvent>>) {
    loop {
        let epoch = *engine.conn_epoch.subscribe().borrow();
        loop {
            match rx.recv().await {
                Ok(batch) => engine.enqueue(Op::Remote(batch)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "sync event pump lagged; reconciling");
                    engine.enqueue(Op::Reconcile);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        if engine.dialer.is_none() {
            break;
        }
        // Wait for the supervisor's swap, then resubscribe.
        let mut watch_rx = engine.conn_epoch.subscribe();
        while *watch_rx.borrow() <= epoch {
            if watch_rx.changed().await.is_err() {
                return;
            }
        }
        let conn = engine.conn();
        rx = conn.events();
        let since = {
            let m = engine.manifest.lock().unwrap();
            if m.last_seq == 0 {
                None
            } else {
                Some(m.last_seq)
            }
        };
        match conn.request(Request::Subscribe { since_seq: since }).await {
            Ok(Ok(Response::Subscribed { last_seq })) if since.is_none_or(|s| last_seq >= s) => {
                tracing::info!(?since, "sync event stream resubscribed");
            }
            Ok(Ok(Response::Subscribed { .. })) => {
                // Sequence went backwards: server restarted.
                tracing::warn!("server restarted during outage; reconciling");
                engine.enqueue(Op::Reconcile);
            }
            Ok(Err(ErrorCode::TooOld)) => {
                tracing::warn!("event history expired during outage; reconciling");
                let _ = conn.request(Request::Subscribe { since_seq: None }).await;
                engine.enqueue(Op::Reconcile);
            }
            other => {
                tracing::warn!(?other, "sync resubscribe failed; will retry next reconnect");
            }
        }
    }
}

async fn reconnect_supervisor(engine: Arc<SyncEngine>) {
    let dialer = engine.dialer.clone().expect("supervisor needs a dialer");
    loop {
        engine.conn().closed().await;
        tracing::warn!("sync connection lost; reconnecting");
        let mut delay = Duration::from_millis(500);
        let new_conn = loop {
            match dialer().await {
                Ok(c) => break c,
                Err(e) => {
                    tracing::warn!(error = %e, retry_in = ?delay, "sync reconnect failed");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(15));
                }
            }
        };
        match new_conn
            .request(Request::Attach {
                export: engine.export.clone(),
            })
            .await
        {
            Ok(Ok(Response::AttachOk { .. })) => {}
            other => {
                tracing::error!(?other, "sync re-attach failed; retrying");
                continue;
            }
        }
        *engine.conn.write().unwrap() = new_conn;
        engine.conn_epoch.send_modify(|e| *e += 1);
        tracing::info!("sync reconnected");
    }
}

/// Remove leftover `.ds-tmp-*` staging files from a crashed prior run.
fn sweep_staging(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(items) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in items.flatten() {
            let path = item.path();
            let name = item.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                stack.push(path);
            } else if name.starts_with(".ds-tmp-") {
                tracing::info!(?path, "sweeping stale staging file");
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
