//! Whole-file advisory locks shared by every session of an export.
//!
//! Semantics mirror POSIX advisory locking coarsened to whole files:
//! N shared holders XOR 1 exclusive holder. `wait: true` parks the caller in
//! a FIFO waiter queue; lock release wakes compatible waiters. Locks are tied
//! to (session, fh): closing the handle, disconnecting, or lease expiry frees
//! them — a crashed client can block others for at most the lease window.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use alloyfs_proto::{ErrorCode, LockKind, RelPath};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Holder {
    pub session: u64,
    pub fh: u64,
    pub kind: LockKind,
}

struct Waiter {
    session: u64,
    fh: u64,
    kind: LockKind,
    wake: oneshot::Sender<()>,
}

#[derive(Default)]
struct Entry {
    holders: Vec<Holder>,
    waiters: VecDeque<Waiter>,
}

impl Entry {
    /// Can `(session, fh)` hold `kind`, given who holds this path now?
    ///
    /// A handle NEVER conflicts with itself. Re-locking a handle is an upgrade
    /// or a downgrade, so its own existing hold has to be excluded from the
    /// test — and excluded by ignoring it, not by removing it first. Removing
    /// first is what the old code did, and it lost the caller's lock outright
    /// whenever the new one could not be granted: a handle that asked for more
    /// and was refused ended up holding LESS than before the call, while its
    /// client still believed it held the original. Restoring it around a
    /// failed attempt is not enough either, because a caller parked waiting to
    /// upgrade would then be blocked by its own retained hold, forever.
    fn compatible_for(&self, kind: LockKind, session: u64, fh: u64) -> bool {
        let mine = |h: &Holder| h.session == session && h.fh == fh;
        match kind {
            LockKind::Shared => self
                .holders
                .iter()
                .all(|h| h.kind == LockKind::Shared || mine(h)),
            LockKind::Exclusive => self.holders.iter().all(mine),
        }
    }
}

#[derive(Default)]
pub struct LockManager {
    entries: Mutex<HashMap<RelPath, Entry>>,
}

impl LockManager {
    /// Try to take the lock; on contention either fail with `WouldBlock`
    /// (wait=false) or await our turn (wait=true, FIFO).
    pub async fn lock(
        &self,
        path: &RelPath,
        session: u64,
        fh: u64,
        kind: LockKind,
        wait: bool,
    ) -> Result<(), ErrorCode> {
        loop {
            let rx = {
                let mut entries = self.entries.lock().unwrap();
                let entry = entries.entry(path.clone()).or_default();
                // Tested BEFORE anything is removed: a handle does not
                // conflict with itself, and its own hold must survive an
                // attempt that fails. Only once the new lock is actually
                // granted does the previous one go, which is the
                // upgrade/downgrade replacing it.
                if entry.compatible_for(kind, session, fh) {
                    entry.holders.retain(|h| !(h.session == session && h.fh == fh));
                    entry.holders.push(Holder { session, fh, kind });
                    return Ok(());
                }
                if !wait {
                    return Err(ErrorCode::WouldBlock);
                }
                let (tx, rx) = oneshot::channel();
                entry.waiters.push_back(Waiter {
                    session,
                    fh,
                    kind,
                    wake: tx,
                });
                rx
            };
            // Parked until a release wakes us; then retry the take. A dropped
            // sender (session reaped while waiting) surfaces as Err -> retry
            // resolves it naturally.
            let _ = rx.await;
        }
    }

    pub fn unlock(&self, path: &RelPath, session: u64, fh: u64) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(path) {
            entry.holders.retain(|h| !(h.session == session && h.fh == fh));
            Self::wake_waiters(entry);
            if entry.holders.is_empty() && entry.waiters.is_empty() {
                entries.remove(path);
            }
        }
    }

    /// Free everything a session holds or waits for (disconnect / lease
    /// expiry). Returns how many locks were released.
    pub fn release_session(&self, session: u64) -> usize {
        let mut entries = self.entries.lock().unwrap();
        let mut released = 0;
        entries.retain(|_, entry| {
            let before = entry.holders.len();
            entry.holders.retain(|h| h.session != session);
            released += before - entry.holders.len();
            entry.waiters.retain(|w| w.session != session);
            Self::wake_waiters(entry);
            !(entry.holders.is_empty() && entry.waiters.is_empty())
        });
        released
    }

    /// Wake the longest-waiting waiters that are now compatible. Shared
    /// waiters wake in a batch; an exclusive waiter wakes alone.
    fn wake_waiters(entry: &mut Entry) {
        while let Some(front) = entry.waiters.front() {
            if entry.compatible_for(front.kind, front.session, front.fh) {
                let w = entry.waiters.pop_front().unwrap();
                // Waking is advisory — the waiter re-checks under the mutex.
                let _ = w.wake.send(());
                // After waking an exclusive candidate, stop: anyone behind
                // them must keep waiting for fairness.
                if w.kind == LockKind::Exclusive {
                    break;
                }
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> RelPath {
        RelPath("a.txt".into())
    }

    #[tokio::test]
    async fn shared_locks_coexist_exclusive_blocks() {
        let lm = LockManager::default();
        lm.lock(&path(), 1, 1, LockKind::Shared, false).await.unwrap();
        lm.lock(&path(), 2, 1, LockKind::Shared, false).await.unwrap();
        assert_eq!(
            lm.lock(&path(), 3, 1, LockKind::Exclusive, false).await,
            Err(ErrorCode::WouldBlock)
        );
        lm.unlock(&path(), 1, 1);
        lm.unlock(&path(), 2, 1);
        lm.lock(&path(), 3, 1, LockKind::Exclusive, false).await.unwrap();
    }

    #[tokio::test]
    async fn waiter_wakes_on_release() {
        let lm = std::sync::Arc::new(LockManager::default());
        lm.lock(&path(), 1, 1, LockKind::Exclusive, false).await.unwrap();
        let lm2 = lm.clone();
        let waiter = tokio::spawn(async move { lm2.lock(&path(), 2, 1, LockKind::Exclusive, true).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        lm.unlock(&path(), 1, 1);
        assert_eq!(waiter.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn session_release_frees_all() {
        let lm = LockManager::default();
        lm.lock(&RelPath("x".into()), 7, 1, LockKind::Exclusive, false)
            .await
            .unwrap();
        lm.lock(&RelPath("y".into()), 7, 2, LockKind::Shared, false)
            .await
            .unwrap();
        assert_eq!(lm.release_session(7), 2);
        lm.lock(&RelPath("x".into()), 8, 1, LockKind::Exclusive, false)
            .await
            .unwrap();
    }

    /// A refused upgrade leaves the caller holding exactly what it held.
    ///
    /// The previous code released the caller's hold BEFORE testing whether
    /// the new one could be granted, and never put it back on the refusal
    /// path. So a handle that asked for more and was told no came away
    /// holding LESS than before the call, while its client went on believing
    /// it held the original — a silent loss of mutual exclusion rather than a
    /// failed operation.
    #[tokio::test]
    async fn a_refused_upgrade_keeps_the_existing_lock() {
        let lm = LockManager::default();
        lm.lock(&path(), 1, 10, LockKind::Shared, false).await.unwrap();
        lm.lock(&path(), 2, 20, LockKind::Shared, false).await.unwrap();

        // Session 1 tries to upgrade and loses to session 2's reader.
        assert_eq!(
            lm.lock(&path(), 1, 10, LockKind::Exclusive, false).await,
            Err(ErrorCode::WouldBlock)
        );

        // Session 1's shared lock must have survived: with session 2 gone, a
        // third party still cannot take the file exclusively.
        lm.unlock(&path(), 2, 20);
        assert_eq!(
            lm.lock(&path(), 3, 30, LockKind::Exclusive, false).await,
            Err(ErrorCode::WouldBlock)
        );
    }

    /// ...and a handle is never blocked by its own hold, so an uncontended
    /// upgrade still succeeds. Testing compatibility while ignoring the
    /// caller is what makes both of these true at once; restoring the hold
    /// around a failed attempt would fix the case above and deadlock this one
    /// as soon as the caller parked to wait.
    #[tokio::test]
    async fn a_handle_can_upgrade_and_downgrade_its_own_lock() {
        let lm = LockManager::default();
        lm.lock(&path(), 1, 10, LockKind::Shared, false).await.unwrap();
        lm.lock(&path(), 1, 10, LockKind::Exclusive, false).await.unwrap();
        // Exclusive really took: nobody else can share it now.
        assert_eq!(
            lm.lock(&path(), 2, 20, LockKind::Shared, false).await,
            Err(ErrorCode::WouldBlock)
        );
        // And back down again, after which a reader can join.
        lm.lock(&path(), 1, 10, LockKind::Shared, false).await.unwrap();
        lm.lock(&path(), 2, 20, LockKind::Shared, false).await.unwrap();
    }

    /// A blocked upgrade is woken by the conflicting reader leaving, rather
    /// than waiting on itself forever.
    #[tokio::test]
    async fn a_blocked_upgrade_wakes_when_the_other_reader_leaves() {
        let lm = std::sync::Arc::new(LockManager::default());
        lm.lock(&path(), 1, 10, LockKind::Shared, false).await.unwrap();
        lm.lock(&path(), 2, 20, LockKind::Shared, false).await.unwrap();
        let lm2 = lm.clone();
        let upgrade =
            tokio::spawn(async move { lm2.lock(&path(), 1, 10, LockKind::Exclusive, true).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!upgrade.is_finished(), "upgrade should be parked");
        lm.unlock(&path(), 2, 20);
        assert_eq!(upgrade.await.unwrap(), Ok(()));
    }
}
