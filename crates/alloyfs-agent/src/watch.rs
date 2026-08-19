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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloyfs_common::coalesce::Coalescer;
use alloyfs_proto::{ErrorCode, EventKind, FsEvent, RelPath};
use dashmap::DashMap;
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
    log_cap: usize,
    tx: broadcast::Sender<Vec<FsEvent>>,
    recent_local: DashMap<RelPath, (u64, Instant)>,
}

impl EventHub {
    pub fn new() -> Arc<Self> {
        Self::with_capacity(EVENT_LOG_CAP)
    }

    /// A hub with a custom ring size — tests use tiny rings to exercise the
    /// `TooOld` path without publishing thousands of events.
    pub fn with_capacity(log_cap: usize) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            seq: AtomicU64::new(0),
            log: Mutex::new(std::collections::VecDeque::with_capacity(log_cap)),
            log_cap,
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

    /// Everything in the ring above `since`.
    ///
    /// Unlike `subscribe`, this neither registers a listener nor reports
    /// `TooOld` — the index build uses it to close the window between taking a
    /// mark and finishing its walk, and a ring that rotated during a 33 ms walk
    /// means the export is changing far faster than an index can track it. The
    /// build would be rebuilt on the next `ResyncRequired` anyway.
    pub fn since(&self, since: u64) -> Vec<FsEvent> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect()
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
                if log.len() == self.log_cap {
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

/// What the OS watcher hands the coalescing loop.
///
/// A gap has to travel the same channel as the events, not a side path: it is
/// ordered against them, and "everything before this point is suspect" only
/// means anything if it arrives in sequence.
enum WatchSignal {
    Event(notify::Event),
    /// The watcher can no longer describe what changed — the kernel queue
    /// overflowed, or the backend failed. Everything the index believes may
    /// now be wrong.
    Gap,
}

/// Classify one watcher event.
///
/// The kernel queue overflowing means events were dropped and WHICH ones is
/// precisely what nobody can know — `need_rescan` is how notify says so
/// (inotify raises it on Q_OVERFLOW). It was being ignored: the event went
/// through as an ordinary change and the index carried on believing it was
/// complete, which is the one thing it is not.
fn signal_for(event: notify::Event) -> WatchSignal {
    if event.need_rescan() {
        tracing::warn!("watcher reported a dropped-event gap; forcing a resync");
        WatchSignal::Gap
    } else {
        WatchSignal::Event(event)
    }
}

/// Spawn the watcher + coalescer for one export. The returned guard keeps the
/// OS watcher alive; drop it to stop watching.
pub fn spawn(export: Arc<Export>, hub: Arc<EventHub>, debounce: Duration) -> anyhow::Result<WatchGuard> {
    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<WatchSignal>();

    // follow_symlinks(false) matters twice over: an export symlink pointing
    // outside (say, at /etc) must be neither watched (privacy) nor able to
    // fail the watch setup on unreadable directories.
    let config = notify::Config::default().with_follow_symlinks(false);
    let mut watcher = notify::RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| match res {
            Ok(event) => {
                let _ = raw_tx.send(signal_for(event));
            }
            // A watcher error is the same class of problem: from here on the
            // stream cannot be trusted to have described everything. Logging
            // it and continuing left the index quietly wrong.
            Err(e) => {
                tracing::warn!(error = %e, "watcher error; forcing a resync");
                let _ = raw_tx.send(WatchSignal::Gap);
            }
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
    mut raw_rx: mpsc::UnboundedReceiver<WatchSignal>,
) {
    // The collapse/pairing/exclude semantics live in alloyfs_common::coalesce —
    // ONE copy, shared with the sync engine's local watcher. This loop owns
    // only the pacing and the server-side publish (version bumps + hub).
    let mut co = Coalescer::new(export.root.clone(), export.exclude.clone());

    loop {
        let timeout = if co.is_empty() {
            Duration::from_secs(3600) // idle: nothing to flush, just wait
        } else {
            debounce
        };
        tokio::select! {
            maybe = raw_rx.recv() => {
                let Some(signal) = maybe else { break }; // watcher dropped
                match signal {
                    WatchSignal::Event(event) => {
                        co.ingest(event);
                        // Overflow guards: flush early on a huge or old backlog.
                        if co.len() >= BATCH_MAX
                            || co.oldest_age().is_some_and(|age| age >= MAX_LATENCY)
                        {
                            publish_batch(&export, &hub, co.take_batch());
                        }
                    }
                    // Publish what was already coalesced first — those events
                    // did happen and describing them is still useful — then
                    // the gap, which tells every reader that anything not
                    // covered by them is unknown. Ordering matters: a resync
                    // ahead of the events it supersedes would be undone by
                    // them.
                    WatchSignal::Gap => {
                        publish_batch(&export, &hub, co.take_batch());
                        publish_batch(
                            &export,
                            &hub,
                            vec![(RelPath(String::new()), EventKind::ResyncRequired)],
                        );
                    }
                }
            }
            _ = tokio::time::sleep(timeout) => {
                publish_batch(&export, &hub, co.take_batch());
            }
        }
    }
    // Drain on shutdown so nothing observed is silently dropped.
    publish_batch(&export, &hub, co.take_batch());
}

/// Server-side effects per coalesced batch: version bookkeeping, then hub
/// fan-out. (Watcher-observed changes bump versions too, so remote edits
/// made directly on the server are visible to version-based caching.)
fn publish_batch(export: &Export, hub: &EventHub, batch: Vec<(RelPath, EventKind)>) {
    if batch.is_empty() {
        return;
    }
    for (path, kind) in &batch {
        match kind {
            EventKind::RenamedFrom { to } => {
                export.rename_version(path, to);
                export.tree.note_change(&export.root, path, true);
                export.tree.note_change(&export.root, to, false);
            }
            EventKind::Removed => {
                export.tree.note_change(&export.root, path, true);
            }
            // A gap means the watcher stopped being able to describe what
            // changed, so the index describes a state that may never have
            // existed. Dropping it costs one rebuild — cheap enough that it is
            // the whole recovery path rather than the last resort.
            EventKind::ResyncRequired => export.tree.invalidate(),
            _ => {
                export.bump(path);
                export.tree.note_change(&export.root, path, false);
            }
        }
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
                ..Default::default()
            },
        );
        let registry = crate::ExportRegistry::from_config(&cfg).unwrap();
        let export = registry.get("t").unwrap();
        (dir, export)
    }

    use notify::event::{DataChange, EventKind as NotifyKind, ModifyKind, RenameMode};

    fn coalescer(export: &Export) -> Coalescer {
        Coalescer::new(export.root.clone(), export.exclude.clone())
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

        let mut co = coalescer(&export);
        co.ingest(rename_event(RenameMode::From, vec![from.clone()]));
        co.ingest(rename_event(RenameMode::To, vec![to.clone()]));
        co.ingest(rename_event(RenameMode::Both, vec![from, to]));

        let hub = export.events.clone();
        let mut rx = hub.subscribe(None).unwrap().1;
        publish_batch(&export, &hub, co.take_batch());
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

        let mut co = coalescer(&export);
        co.ingest(rename_event(RenameMode::Both, vec![from, to.clone()]));
        co.ingest(notify::Event {
            kind: NotifyKind::Modify(ModifyKind::Data(DataChange::Any)),
            paths: vec![to],
            attrs: Default::default(),
        });

        let hub = export.events.clone();
        let mut rx = hub.subscribe(None).unwrap().1;
        publish_batch(&export, &hub, co.take_batch());
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

    /// A dropped-event gap must become a resync, not an ordinary change.
    ///
    /// This is the reachability half of the recovery path. `publish_batch`
    /// has always invalidated the index on `ResyncRequired` — and nothing
    /// ever produced one from the watcher, so the recovery its comment
    /// describes could not happen. inotify raises this flag on Q_OVERFLOW,
    /// which is the case that matters: events were dropped, WHICH ones is
    /// unknowable, and the index is quietly describing a state that may never
    /// have existed.
    #[test]
    fn a_rescan_flagged_event_becomes_a_gap() {
        let overflowed = notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert!(
            matches!(signal_for(overflowed), WatchSignal::Gap),
            "an overflow must force a resync"
        );
    }

    /// ...and an ordinary event must NOT, or every change would invalidate
    /// the index and the whole point of having one is gone.
    #[test]
    fn an_ordinary_event_is_not_a_gap() {
        let plain = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Content,
        )));
        assert!(matches!(signal_for(plain), WatchSignal::Event(_)));
    }
}
