//! Proof that invalidation is still working, so the kernel can be trusted to
//! cache without a time limit.
//!
//! The kernel's metadata timeout was never the correctness mechanism — the
//! event pump is. A server-side change reaches us as an event, we drop our own
//! caches and notify the kernel, and the kernel drops both its metadata AND
//! its cached data (measured: a file read 21 times reached the filesystem once,
//! and one remote change put the next read back through to us). The timeout
//! only ever bounded how long a mount stayed wrong if that pump stopped
//! working, which is why raising it to "never" needs something else to catch
//! exactly that.
//!
//! Two things can break invalidation and neither announces itself:
//!
//! - The pump task dies. Rare, but it survives reconnects by design, so if it
//!   is gone something unusual happened and nothing else will notice.
//! - **The server has no watcher.** `alloyfs serve` degrades an export to
//!   no-events when its watcher will not start — an inotify limit is enough,
//!   and this has been triggered in practice by running many agents against one
//!   box. The export then works perfectly and never reports a change. Clients
//!   are not told, and cannot infer it: an export with no watcher looks exactly
//!   like an export where nothing is happening.
//!
//! So rather than trusting either end, this checks the only thing that matters:
//! does what we hold still match the server? A handful of cached paths are
//! re-stated on a timer and compared against our own attrs. A disagreement
//! means a change happened that invalidation missed — whatever the reason —
//! and the answer is the same in every case: treat those paths as changed,
//! which pushes them through the ordinary event machinery and drops them from
//! every cache including the kernel's.
//!
//! It costs one bulk stat per interval, and it converts a silent, permanent
//! wrongness into a loud, self-correcting one.

use std::sync::Arc;
use std::time::Duration;

use alloyfs_proto::{EventKind, FsEvent, RelPath, Request, Response};

use crate::remote_fs::RemoteFs;

/// How often to check. Matches the metadata timeout this replaces, so the
/// worst-case staleness is what it always was — the difference is that it is
/// now bounded by a check that runs rather than by a clock the kernel keeps.
pub(crate) const INTERVAL: Duration = Duration::from_secs(30);

/// Paths sampled per round. Enough that a broken pump is caught within a few
/// rounds on any active export, small enough to ride in one `GetattrMany`.
const SAMPLE: usize = 8;

pub(crate) fn spawn(fs: Arc<RemoteFs>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        tick.tick().await; // the immediate first tick tells us nothing
        loop {
            tick.tick().await;
            let sample = fs.sample_cached_paths(SAMPLE);
            if sample.is_empty() {
                continue; // nothing cached yet; nothing to be wrong about
            }
            let diverged = match check(&fs, &sample).await {
                Ok(d) => d,
                // A failed check is not a failed cache. The connection is the
                // supervisor's problem; retrying next round is right.
                Err(()) => continue,
            };
            if diverged.is_empty() {
                continue;
            }
            tracing::error!(
                count = diverged.len(),
                paths = ?diverged.iter().take(3).collect::<Vec<_>>(),
                "cached metadata disagrees with the server — invalidation is not \
                 working (a dead event pump, or an export the server is not \
                 watching). Dropping these paths; check the agent's log for \
                 'file watching disabled'."
            );
            fs.force_invalidate(&diverged);
        }
    });
}

/// The paths whose server attrs differ from what we hold.
async fn check(fs: &Arc<RemoteFs>, paths: &[RelPath]) -> Result<Vec<RelPath>, ()> {
    let resp = fs
        .conn()
        .request(Request::GetattrMany {
            paths: paths.to_vec(),
        })
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    let Response::ManyOutcome(outcomes) = resp else {
        return Err(()); // pre-v12 server: no bulk stat, so no canary
    };
    let mut diverged = Vec::new();
    for (path, outcome) in paths.iter().zip(outcomes) {
        match outcome {
            // A path we cached that the server no longer has is a divergence
            // too — a delete we never heard about.
            Ok(None) | Err(_) => diverged.push(path.clone()),
            Ok(Some(server)) => {
                if fs.cached_attr_differs(path, &server) {
                    diverged.push(path.clone());
                }
            }
        }
    }
    Ok(diverged)
}

/// Synthetic events for paths the canary found stale, shaped exactly like the
/// server's own so every downstream path treats them identically.
pub(crate) fn events_for(paths: &[RelPath]) -> Vec<FsEvent> {
    paths
        .iter()
        .map(|p| FsEvent {
            // seq 0: these are not from the server's log and must never
            // advance the cursor the cache persists, or a restart would
            // believe it had covered events it never saw.
            seq: 0,
            kind: EventKind::Modified,
            path: p.clone(),
            new_version: None,
            // No origin: this is emphatically not our own write echoing back,
            // and marking it as one would make the client skip it.
            origin: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_events_never_advance_the_cursor() {
        let ev = events_for(&[RelPath("a.txt".into()), RelPath("b/c.txt".into())]);
        assert_eq!(ev.len(), 2);
        for e in &ev {
            assert_eq!(e.seq, 0, "a canary event must not look like server progress");
            assert!(e.origin.is_none(), "and must not be mistaken for a self-echo");
            assert!(matches!(e.kind, EventKind::Modified));
        }
        assert_eq!(ev[1].path, RelPath("b/c.txt".into()));
    }
}
