//! Client-side event application: keep caches honest, then let the mount
//! backend re-emit natively.
//!
//! Order matters: invalidation happens BEFORE the backend callback, so by the
//! time an editor reacts to a change notification, a fresh getattr/read is
//! guaranteed to bypass stale cache.

use std::sync::Arc;

use ds_proto::{EventKind, FsEvent, Request, Response};

use crate::remote_fs::{FsError, RemoteFs};

impl RemoteFs {
    /// Overlay filter: the server may legitimately emit events for paths this
    /// client excludes (its own copy changing server-side). Those must never
    /// reach cache application or native re-emission — the paths are
    /// invisible here. Boundary renames degrade to their visible half.
    fn filter_for_overlay(&self, batch: Vec<FsEvent>) -> Vec<FsEvent> {
        if self.overlay.is_none() {
            return batch;
        }
        batch
            .into_iter()
            .filter_map(|mut ev| {
                let px = self.is_overlay(&ev.path);
                match &ev.kind {
                    EventKind::RenamedFrom { to } => match (px, self.is_overlay(to)) {
                        (true, true) => None,
                        (true, false) => {
                            let to = to.clone();
                            ev.path = to;
                            ev.kind = EventKind::Created;
                            Some(ev)
                        }
                        (false, true) => {
                            ev.kind = EventKind::Removed;
                            Some(ev)
                        }
                        (false, false) => Some(ev),
                    },
                    _ if px => None,
                    _ => Some(ev),
                }
            })
            .collect()
    }

    /// Auto-cache maintenance driven by (already overlay-filtered) events.
    fn apply_events_to_cache(&self, batch: &[FsEvent]) {
        let Some(cache) = &self.cache else { return };
        for ev in batch {
            match &ev.kind {
                EventKind::Modified | EventKind::AttrChanged | EventKind::Created => {
                    let known = cache.known(&ev.path);
                    cache.invalidate(&ev.path);
                    // Re-fetch things we cared about (or now should: pins).
                    if known || cache.pin_match(&ev.path) || matches!(ev.kind, EventKind::Modified) {
                        cache.enqueue_refetch(ev.path.clone());
                    }
                }
                EventKind::Removed => cache.remove(&ev.path),
                EventKind::RenamedFrom { to } => cache.rename(&ev.path, to),
                EventKind::ResyncRequired => cache.mark_all_unverified(),
            }
        }
    }

    /// Apply one server event batch to local state (attr cache + inode table).
    pub fn apply_events(&self, batch: &[FsEvent]) {
        for ev in batch {
            match &ev.kind {
                EventKind::ResyncRequired => {
                    tracing::warn!("server requested resync: flushing caches");
                    self.invalidate_all();
                }
                EventKind::RenamedFrom { to } => {
                    if let Some(ino) = self.ino.ino_of(&ev.path) {
                        self.invalidate_attr(ino);
                    }
                    self.ino.rename(&ev.path, to);
                    if let Some(ino) = self.ino.ino_of(to) {
                        self.invalidate_attr(ino);
                    }
                }
                _ => {
                    if let Some(ino) = self.ino.ino_of(&ev.path) {
                        self.invalidate_attr(ino);
                    }
                }
            }
        }
    }

    /// Subscribe on the server and spawn the pump task. `on_batch` runs after
    /// cache invalidation — mount backends re-emit events natively from it.
    /// The pump ends when the connection closes.
    pub async fn start_event_pump(
        self: &Arc<Self>,
        on_batch: impl Fn(&[FsEvent]) + Send + 'static,
    ) -> Result<u64, FsError> {
        let last_seq = match self.conn().request(Request::Subscribe { since_seq: None }).await?? {
            Response::Subscribed { last_seq } => last_seq,
            _ => return Err(ds_proto::ErrorCode::Io.into()),
        };
        let fs = self.clone();
        let mut rx = self.conn().events();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(batch) => {
                        let batch = fs.filter_for_overlay(batch);
                        if batch.is_empty() {
                            continue;
                        }
                        fs.apply_events(&batch);
                        fs.apply_events_to_cache(&batch);
                        on_batch(&batch);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // We fell behind the connection reader: safest reset.
                        tracing::warn!(missed = n, "event pump lagged; flushing caches");
                        fs.invalidate_all();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            tracing::info!("event pump ended (connection closed)");
        });
        Ok(last_seq)
    }
}
