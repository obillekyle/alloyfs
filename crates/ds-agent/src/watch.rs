//! The event engine: OS watcher → coalescer → sequenced log → fan-out.
//!
//! Server-side flow, per export:
//!
//! ```text
//! notify (inotify/RDCW) ──raw──▶ coalescer task ──batches──▶ EventHub
//!                                                             ├─ ring log (catch-up for reconnects)
//!                                                             └─ broadcast (live subscribers)
//! ```
//!
//! The coalescer exists because real filesystems storm: a compiler writing an
//! artifact can emit dozens of raw events in milliseconds. We collapse them
//! per-path (Created+Modified→Created, Created+Removed→nothing, …) and flush
//! a batch when the export has been quiet for `debounce`, when the pending map
//! is large, or when the oldest pending change has waited too long.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use ds_proto::{ErrorCode, EventKind, FsEvent, RelPath};
use notify::event::{CreateKind, EventKind as NotifyKind, ModifyKind, RemoveKind, RenameMode};
use notify::{RecursiveMode, Watcher as _};
use tokio::sync::{broadcast, mpsc};

use crate::ops::Export;

/// How many events the catch-up ring remembers per export. A reconnecting
/// client whose `since_seq` fell off the ring gets `TooOld` and must flush
/// its caches instead of patching incrementally.
const EVENT_LOG_CAP: usize = 8192;
/// Flush the pending map when it grows past this many distinct paths.
const BATCH_MAX: usize = 512;
/// ...or when the oldest pending event has waited this long.
const MAX_LATENCY: Duration = Duration::from_secs(1);
/// Echo-suppression memory: how long a session "owns" a path after writing.
const ORIGIN_TTL: Duration = Duration::from_secs(2);

/// Per-export event state: sequence numbers, catch-up ring, live fan-out,
/// and the recent-local-writes table used for echo suppression.
pub struct EventHub {
    seq: AtomicU64,
    log: Mutex<std::collections::VecDeque<FsEvent>>,
    tx: broadcast::Sender<Vec<FsEvent>>,
    recent_local: DashMap<RelPath, (u64, Instant)>,
}

impl EventHub {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            seq: AtomicU64::new(0),
            log: Mutex::new(std::collections::VecDeque::with_capacity(EVENT_LOG_CAP)),
            tx,
            recent_local: DashMap::new(),
        })
    }

    /// Called by mutating request handlers: remember that `session` just
    /// touched `path`, so the imminent watcher echo carries its origin.
    pub fn note_local_write(&self, path: &RelPath, session: u64) {
        self.recent_local.insert(path.clone(), (session, Instant::now()));
    }

    pub fn last_seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Subscribe from `since` (None = live only). Returns the catch-up batch
    /// plus a live receiver, or `TooOld` if the ring no longer reaches back
    /// far enough.
    pub fn subscribe(
        &self,
        since: Option<u64>,
    ) -> Result<(Vec<FsEvent>, broadcast::Receiver<Vec<FsEvent>>), ErrorCode> {
        let rx = self.tx.subscribe();
        let catchup = match since {
            None => Vec::new(),
            Some(since) => {
                let log = self.log.lock().unwrap();
                match log.front() {
                    // Ring must contain (since+1); its head is the oldest kept.
                    Some(head) if head.seq > since + 1 => return Err(ErrorCode::TooOld),
                    _ => log.iter().filter(|e| e.seq > since).cloned().collect(),
                }
            }
        };
        Ok((catchup, rx))
    }

    /// Assign sequence numbers, remember in the ring, fan out.
    fn publish(&self, raw: Vec<(RelPath, EventKind)>) {
        if raw.is_empty() {
            return;
        }
        let mut batch = Vec::with_capacity(raw.len());
        {
            let mut log = self.log.lock().unwrap();
            for (path, kind) in raw {
                let origin = self.recent_local.get(&path).and_then(|entry| {
                    let (session, when) = *entry;
                    (when.elapsed() < ORIGIN_TTL).then_some(session)
                });
                let event = FsEvent {
                    seq: self.seq.fetch_add(1, Ordering::AcqRel) + 1,
                    kind,
                    path,
                    new_version: None, // filled by the caller when it knows better
                    origin,
                };
                if log.len() == EVENT_LOG_CAP {
                    log.pop_front();
                }
                log.push_back(event.clone());
                batch.push(event);
            }
        }
        // Send fails only with zero subscribers — which is fine.
        let _ = self.tx.send(batch);
    }
}

/// What we remember about a path between flushes.
#[derive(Clone, Debug)]
struct Pending {
    kind: EventKind,
    first_seen: Instant,
}

fn merge(old: Option<&EventKind>, new: EventKind) -> Option<EventKind> {
    use EventKind::*;
    Some(match (old, new) {
        // A file that appeared and was then modified is still just "Created"
        // from any observer's point of view.
        (Some(Created), Modified | AttrChanged) => Created,
        // Appeared then vanished: nothing happened, observably.
        (Some(Created), Removed) => return None,
        // Vanished then reappeared: net effect is content replacement.
        (Some(Removed), Created) => Modified,
        // Attr change then data change: report the stronger one.
        (Some(AttrChanged), Modified) => Modified,
        (Some(Modified), AttrChanged) => Modified,
        // Renames and resyncs always win outright.
        (_, ev @ (RenamedFrom { .. } | ResyncRequired)) => ev,
        (Some(old), _) => old.clone(),
        (None, ev) => ev,
    })
}

/// Convert an absolute path inside the export to wire form.
fn rel_of(root: &Path, abs: &Path) -> Option<RelPath> {
    let rel = abs.strip_prefix(root).ok()?;
    let s = rel.to_str()?;
    if s.is_empty() {
        return None; // events on the root itself aren't interesting
    }
    Some(RelPath(s.replace('\\', "/")))
}

/// Spawn the watcher + coalescer for one export. The returned guard keeps the
/// OS watcher alive; drop it to stop watching.
pub fn spawn(export: Arc<Export>, hub: Arc<EventHub>, debounce: Duration) -> anyhow::Result<WatchGuard> {
    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<notify::Event>();

    // follow_symlinks(false) matters twice over: an export symlink pointing
    // outside (say, at /etc) must be neither watched (privacy) nor able to
    // fail the watch setup on unreadable directories.
    let config = notify::Config::default().with_follow_symlinks(false);
    let mut watcher = notify::RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| match res {
            Ok(event) => {
                let _ = raw_tx.send(event);
            }
            Err(e) => tracing::warn!(error = %e, "watcher error"),
        },
        config,
    )?;
    watcher.watch(&export.root, RecursiveMode::Recursive)?;
    tracing::info!(export = export.name, "watching for changes");

    tokio::spawn(coalesce_loop(export, hub, debounce, raw_rx));
    Ok(WatchGuard {
        _watcher: Box::new(watcher),
    })
}

pub struct WatchGuard {
    _watcher: Box<dyn std::any::Any + Send>,
}

async fn coalesce_loop(
    export: Arc<Export>,
    hub: Arc<EventHub>,
    debounce: Duration,
    mut raw_rx: mpsc::UnboundedReceiver<notify::Event>,
) {
    let mut pending: HashMap<RelPath, Pending> = HashMap::new();
    let mut renames: Vec<(RelPath, RelPath)> = Vec::new(); // (from, to), flushed with the batch

    loop {
        let timeout = if pending.is_empty() && renames.is_empty() {
            Duration::from_secs(3600) // idle: nothing to flush, just wait
        } else {
            debounce
        };
        tokio::select! {
            maybe = raw_rx.recv() => {
                let Some(event) = maybe else { break }; // watcher dropped
                ingest(&export, &mut pending, &mut renames, event);
                // Overflow guards: flush early on a huge or old backlog.
                let oldest = pending.values().map(|p| p.first_seen).min();
                if pending.len() >= BATCH_MAX
                    || oldest.is_some_and(|t| t.elapsed() >= MAX_LATENCY)
                {
                    flush(&export, &hub, &mut pending, &mut renames);
                }
            }
            _ = tokio::time::sleep(timeout) => {
                flush(&export, &hub, &mut pending, &mut renames);
            }
        }
    }
    // Drain on shutdown so nothing observed is silently dropped.
    flush(&export, &hub, &mut pending, &mut renames);
}

fn ingest(
    export: &Export,
    pending: &mut HashMap<RelPath, Pending>,
    renames: &mut Vec<(RelPath, RelPath)>,
    event: notify::Event,
) {
    let mut push = |path: RelPath, kind: EventKind| {
        // Server-excluded paths never produce events — invisible means silent.
        if export.exclude.is_excluded(&path) {
            return;
        }
        match merge(pending.get(&path).map(|p| &p.kind), kind) {
            Some(kind) => {
                let first_seen = pending
                    .get(&path)
                    .map(|p| p.first_seen)
                    .unwrap_or_else(Instant::now);
                pending.insert(path, Pending { kind, first_seen });
            }
            None => {
                pending.remove(&path);
            }
        }
    };

    match event.kind {
        NotifyKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Folder | _) => {
            for p in &event.paths {
                if let Some(rel) = rel_of(&export.root, p) {
                    push(rel, EventKind::Created);
                }
            }
        }
        NotifyKind::Modify(ModifyKind::Name(mode)) => match (mode, event.paths.as_slice()) {
            (RenameMode::Both, [from, to]) => {
                if let (Some(f), Some(t)) = (rel_of(&export.root, from), rel_of(&export.root, to)) {
                    // Renames across the exclude boundary degrade to the
                    // visible half: into-excluded looks like a removal,
                    // out-of-excluded looks like a creation.
                    let fx = export.exclude.is_excluded(&f);
                    let tx = export.exclude.is_excluded(&t);
                    match (fx, tx) {
                        (true, true) => {}
                        (true, false) => push(t, EventKind::Created),
                        (false, true) => push(f, EventKind::Removed),
                        (false, false) => renames.push((f, t)),
                    }
                }
            }
            // Unpaired halves degrade to Remove/Create — still correct for
            // cache invalidation, just less precise.
            (RenameMode::From, [from]) => {
                if let Some(f) = rel_of(&export.root, from) {
                    push(f, EventKind::Removed);
                }
            }
            (RenameMode::To, [to]) => {
                if let Some(t) = rel_of(&export.root, to) {
                    push(t, EventKind::Created);
                }
            }
            _ => {
                for p in &event.paths {
                    if let Some(rel) = rel_of(&export.root, p) {
                        push(rel, EventKind::Modified);
                    }
                }
            }
        },
        NotifyKind::Modify(ModifyKind::Metadata(_)) => {
            for p in &event.paths {
                if let Some(rel) = rel_of(&export.root, p) {
                    push(rel, EventKind::AttrChanged);
                }
            }
        }
        NotifyKind::Modify(_) => {
            for p in &event.paths {
                if let Some(rel) = rel_of(&export.root, p) {
                    push(rel, EventKind::Modified);
                }
            }
        }
        NotifyKind::Remove(RemoveKind::Any | RemoveKind::File | RemoveKind::Folder | _) => {
            for p in &event.paths {
                if let Some(rel) = rel_of(&export.root, p) {
                    push(rel, EventKind::Removed);
                }
            }
        }
        NotifyKind::Any | NotifyKind::Access(_) | NotifyKind::Other => {}
    }
}

fn flush(
    export: &Export,
    hub: &EventHub,
    pending: &mut HashMap<RelPath, Pending>,
    renames: &mut Vec<(RelPath, RelPath)>,
) {
    if pending.is_empty() && renames.is_empty() {
        return;
    }
    let mut batch: Vec<(RelPath, EventKind)> = Vec::with_capacity(pending.len() + renames.len());
    for (from, to) in renames.drain(..) {
        // Some backends report one rename as From + To + Both: the paired
        // event wins, so drop the degraded halves it would duplicate — but
        // ONLY exact halves (a real Modified on the target must survive for
        // cache freshness).
        if pending
            .get(&from)
            .is_some_and(|p| matches!(p.kind, EventKind::Removed))
        {
            pending.remove(&from);
        }
        if pending
            .get(&to)
            .is_some_and(|p| matches!(p.kind, EventKind::Created))
        {
            pending.remove(&to);
        }
        export.rename_version(&from, &to);
        batch.push((from, EventKind::RenamedFrom { to }));
    }
    // Deterministic order helps tests and debugging; HashMap order is noise.
    let mut items: Vec<(RelPath, Pending)> = pending.drain().collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, p) in items {
        // Watcher-observed changes bump versions too, so remote edits made
        // directly on the server are visible to version-based caching.
        if !matches!(p.kind, EventKind::Removed) {
            export.bump(&path);
        }
        batch.push((path, p.kind));
    }
    tracing::debug!(export = export.name, n = batch.len(), "event batch");
    hub.publish(batch);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_export() -> (tempfile::TempDir, Arc<Export>) {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = crate::config::AgentConfig::default();
        cfg.exports.insert(
            "t".into(),
            crate::config::ExportConfig {
                path: dir.path().to_path_buf(),
                read_only: false,
                exclude: vec![],
                client: None,
            },
        );
        let registry = crate::ExportRegistry::from_config(&cfg).unwrap();
        let export = registry.get("t").unwrap();
        (dir, export)
    }

    fn rename_event(mode: RenameMode, paths: Vec<std::path::PathBuf>) -> notify::Event {
        notify::Event {
            kind: NotifyKind::Modify(ModifyKind::Name(mode)),
            paths,
            attrs: Default::default(),
        }
    }

    /// Backends that report one rename as From + To + Both must yield exactly
    /// one RenamedFrom — the degraded Removed/Created halves are dropped.
    #[tokio::test]
    async fn paired_rename_dedupes_degraded_halves() {
        let (_dir, export) = test_export();
        // Build paths from export.root (canonicalized) so rel_of strips them.
        let from = export.root.join("old.txt");
        let to = export.root.join("new.txt");

        let mut pending = HashMap::new();
        let mut renames = Vec::new();
        ingest(
            &export,
            &mut pending,
            &mut renames,
            rename_event(RenameMode::From, vec![from.clone()]),
        );
        ingest(
            &export,
            &mut pending,
            &mut renames,
            rename_event(RenameMode::To, vec![to.clone()]),
        );
        ingest(
            &export,
            &mut pending,
            &mut renames,
            rename_event(RenameMode::Both, vec![from, to]),
        );

        let hub = export.events.clone();
        let mut rx = hub.subscribe(None).unwrap().1;
        flush(&export, &hub, &mut pending, &mut renames);
        let batch = rx.try_recv().expect("one batch published");
        assert_eq!(batch.len(), 1, "exactly one event, got {batch:?}");
        assert!(
            matches!(&batch[0].kind, EventKind::RenamedFrom { to } if to.0 == "new.txt"),
            "the paired rename must win: {batch:?}"
        );
        assert_eq!(batch[0].path.0, "old.txt");
    }

    /// A real Modified on the rename target (e.g. write-after-move) must NOT
    /// be swallowed by the dedupe.
    #[tokio::test]
    async fn rename_dedupe_keeps_real_modify() {
        let (_dir, export) = test_export();
        let from = export.root.join("old.txt");
        let to = export.root.join("new.txt");

        let mut pending = HashMap::new();
        let mut renames = Vec::new();
        ingest(
            &export,
            &mut pending,
            &mut renames,
            rename_event(RenameMode::Both, vec![from, to.clone()]),
        );
        ingest(
            &export,
            &mut pending,
            &mut renames,
            notify::Event {
                kind: NotifyKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                paths: vec![to],
                attrs: Default::default(),
            },
        );

        let hub = export.events.clone();
        let mut rx = hub.subscribe(None).unwrap().1;
        flush(&export, &hub, &mut pending, &mut renames);
        let batch = rx.try_recv().expect("one batch published");
        assert_eq!(batch.len(), 2, "rename + modify both survive: {batch:?}");
        assert!(batch
            .iter()
            .any(|e| matches!(e.kind, EventKind::RenamedFrom { .. })));
        assert!(
            batch
                .iter()
                .any(|e| matches!(e.kind, EventKind::Modified) && e.path.0 == "new.txt"),
            "real Modified on the target must survive: {batch:?}"
        );
    }
}
