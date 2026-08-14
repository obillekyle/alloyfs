//! Server events → kernel notifications.
//!
//! The event pump hands us batches from an async task, but turning an event
//! into a notification needs blocking `RemoteFs` calls (a `Created` event does
//! not say whether a directory or a file appeared, and a `Modified` one does
//! not carry the new size). So the pump's callback only *queues*, and a plain
//! thread does the work and writes the frames.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use alloyfs_proto::FsEvent;

use crate::server::Server;

/// How often the notify thread looks at the shutdown flag while idle.
const IDLE_TICK: Duration = Duration::from_millis(200);

/// Anywhere a finished frame can go: the device in production, a `Vec` in
/// tests.
pub trait Frames: Send + Sync + 'static {
    fn write_frame(&self, buf: &[u8]) -> std::io::Result<()>;
}

/// What the CLI hands to `RemoteFs::start_event_pump`. Cheap and non-blocking:
/// a full queue must never stall the connection's reader task.
#[derive(Clone)]
pub struct EventSink {
    tx: Sender<Vec<FsEvent>>,
}

impl EventSink {
    pub fn push(&self, batch: &[FsEvent]) {
        // Send failure means the notify thread is gone (unmounting): the
        // events have nowhere to go and nothing left to tell.
        let _ = self.tx.send(batch.to_vec());
    }
}

/// The queue between the event pump and the notification thread.
pub fn channel() -> (EventSink, Receiver<Vec<FsEvent>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    (EventSink { tx }, rx)
}

/// Drain queued batches, translate, and write. Ends when `stop` is set or
/// every `EventSink` has been dropped.
pub fn run(rx: Receiver<Vec<FsEvent>>, server: Arc<Server>, frames: impl Frames, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let batch = match rx.recv_timeout(IDLE_TICK) {
            Ok(batch) => batch,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        for ev in &batch {
            let Some(note) = server.notification_for(ev) else {
                continue; // nothing cached under that parent, or nothing to say
            };
            let Some(frame) = note.encode() else {
                tracing::warn!(path = %ev.path, "event does not fit the notify ABI; dropped");
                continue;
            };
            if let Err(e) = frames.write_frame(&frame) {
                // The kernel refuses malformed frames and reports ENODEV once
                // the mount is gone; neither is worth retrying.
                tracing::debug!(error = %e, path = %ev.path, "notification not delivered");
                if e.raw_os_error() == Some(crate::abi::ENODEV) {
                    return;
                }
            }
        }
    }
}
