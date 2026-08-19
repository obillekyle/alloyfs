//! Client-side event application: keep caches honest, then let the mount
//! backend re-emit natively.
//!
//! Order matters: invalidation happens BEFORE the backend callback, so by the
//! time an editor reacts to a change notification, a fresh getattr/read is
//! guaranteed to bypass stale cache.

use std::sync::Arc;

use alloyfs_proto::{EventKind, FsEvent, Request, Response};

use crate::error::FsError;
use crate::remote_fs::RemoteFs;

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
    ///
    /// With a reconnect dialer configured, the pump survives connection loss:
    /// it waits for the supervisor's swap, then resubscribes from the last
    /// event seq it applied (server ring-log catchup); a `TooOld` answer
    /// flushes caches and resubscribes live. Without a dialer it ends when
    /// the connection closes, as before.
    pub async fn start_event_pump(
        self: &Arc<Self>,
        on_batch: impl Fn(&[FsEvent]) + Send + 'static,
    ) -> Result<u64, FsError> {
        // Resume from where the cache left off, not from nothing. The manifest
        // records the sequence its blobs were current at, so a mount that finds
        // one asks the server to replay only what happened since — one request
        // to establish that the whole tree is still good, instead of proving it
        // a file at a time. 0 means no manifest (or a pre-seq one), which
        // subscribes live exactly as before.
        //
        // Correctness rests on the `TooOld` branch below: if the server cannot
        // replay from this point, the cache is dropped rather than trusted.
        let since = match self.cache.as_ref().map(|c| c.saved_seq()) {
            Some(0) | None => None,
            Some(seq) => Some(seq),
        };
        self.start_event_pump_since(since, on_batch).await
    }

    /// `start_event_pump` with an initial catch-up point: the first Subscribe
    /// asks the server to replay its ring log from `since` (the CLI's
    /// `events --since N`). `TooOld` falls back to a live subscription with a
    /// warning rather than failing the pump.
    pub async fn start_event_pump_since(
        self: &Arc<Self>,
        since: Option<u64>,
        on_batch: impl Fn(&[FsEvent]) + Send + 'static,
    ) -> Result<u64, FsError> {
        let conn = self.conn();
        // Receiver BEFORE Subscribe: catchup batches pushed with no receiver
        // would be silently dropped by the broadcast channel.
        let rx = conn.events();
        let first = conn.request(Request::Subscribe { since_seq: since }).await?;
        let resp = match first {
            Err(alloyfs_proto::ErrorCode::TooOld) if since.is_some() => {
                // The cache cannot be trusted past a gap it never saw. Whatever
                // changed while this client was away is unknown by definition,
                // so every entry goes back to unverified and re-proves itself
                // on next use. This is the resync half of the cursor: the seq
                // is only worth persisting if failing to replay from it forces
                // the tree to be re-established rather than quietly assumed.
                tracing::warn!(?since, "requested seq fell off the ring log; resyncing");
                self.invalidate_all();
                if let Some(cache) = &self.cache {
                    cache.mark_all_unverified();
                }
                conn.request(Request::Subscribe { since_seq: None }).await??
            }
            other => other?,
        };
        let last_seq = match resp {
            Response::Subscribed { last_seq } => last_seq,
            _ => return Err(alloyfs_proto::ErrorCode::Io.into()),
        };
        // Seed the resubscription cursor so a reconnect before the first
        // batch still resumes from the caller's point, not from zero.
        if let Some(s) = since {
            self.last_event_seq
                .fetch_max(s, std::sync::atomic::Ordering::AcqRel);
        }
        let fs = self.clone();
        tokio::spawn(async move {
            let mut rx = rx;
            loop {
                // Capture the epoch BEFORE consuming the stream: if the
                // supervisor reconnects while we're in the recv loop, the
                // wait below returns instantly instead of missing the bump.
                let epoch = fs.conn_epoch_now();
                loop {
                    match rx.recv().await {
                        Ok(batch) => {
                            let high = batch.iter().map(|e| e.seq).max();
                            if let Some(max) = high {
                                fs.last_event_seq
                                    .fetch_max(max, std::sync::atomic::Ordering::AcqRel);
                            }
                            let batch = fs.filter_for_overlay(batch);
                            if !batch.is_empty() {
                                fs.apply_events(&batch);
                                fs.apply_events_to_cache(&batch);
                                on_batch(&batch);
                            }
                            // Persist the cursor only AFTER the batch has been
                            // applied. The other order looks equivalent and is
                            // not: a crash in between would leave a manifest
                            // claiming to cover an event whose invalidation
                            // never happened, and the next mount would trust a
                            // stale blob because the seq said it was current.
                            //
                            // An empty batch still advances it. Empty here means
                            // every event named an overlay path, which by
                            // definition the server does not own and the cache
                            // does not hold — nothing to apply, but the events
                            // are genuinely accounted for.
                            if let (Some(max), Some(cache)) = (high, &fs.cache) {
                                cache.record_seq(max);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // We fell behind the connection reader: safest reset.
                            tracing::warn!(missed = n, "event pump lagged; flushing caches");
                            fs.invalidate_all();
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                if fs.dialer.is_none() {
                    break;
                }
                // Wait for the reconnect supervisor to swap in a fresh conn,
                // then resubscribe from where we left off.
                fs.conn_changed_since(epoch).await;
                let conn = fs.conn();
                rx = conn.events();
                let seen = fs.last_event_seq.load(std::sync::atomic::Ordering::Acquire);
                let since = if seen == 0 { None } else { Some(seen) };
                match conn.request(Request::Subscribe { since_seq: since }).await {
                    Ok(Ok(Response::Subscribed { .. })) => {
                        tracing::info!(since = seen, "event stream resubscribed");
                    }
                    Ok(Err(alloyfs_proto::ErrorCode::TooOld)) => {
                        tracing::warn!("event history expired during outage; flushing caches");
                        fs.invalidate_all();
                        if let Some(cache) = &fs.cache {
                            cache.mark_all_unverified();
                        }
                        let _ = conn.request(Request::Subscribe { since_seq: None }).await;
                    }
                    other => {
                        tracing::warn!(?other, "resubscribe failed; will retry on next reconnect");
                    }
                }
            }
            tracing::info!("event pump ended (connection closed)");
        });
        Ok(last_seq)
    }
}
