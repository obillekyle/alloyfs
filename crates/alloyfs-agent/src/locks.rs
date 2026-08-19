//! POSIX byte-range advisory locks, shared by every session of an export.
//!
//! A lock is an interval `[start, end)` on a path, held by an owner, either
//! Shared or Exclusive. Two locks conflict when their ranges OVERLAP, at least
//! one of them is Exclusive, and their owners DIFFER. `wait: true` parks the
//! caller; a release re-tests the queue.
//!
//! Locks are tied to `(session, fh, owner)`. Keying on the handle rather than
//! the process gives open-file-description semantics — closing one descriptor
//! does not drop locks taken through another — which is both stricter than
//! POSIX's drop-all-on-any-close and the only thing that can be meant across a
//! network, where a peer's pid identifies nothing.
//!
//! Splitting is the operation everything else falls out of. Releasing the
//! middle of a held range leaves two fragments; re-locking part of one with a
//! different kind replaces exactly that part. That is what the whole-file
//! coarsening this replaces could not express: it turned any range into the
//! whole file, so taking a lock claimed more than was asked for and releasing
//! one dropped everything the handle held. SQLite walks into precisely that on
//! every read transaction, holding a read lock on one range while taking a
//! write lock on another a byte away.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use alloyfs_proto::{ErrorCode, LockConflict, LockKind, RelPath};
use tokio::sync::oneshot;

/// Who holds a lock. Two locks with equal owners never conflict, which is what
/// makes an upgrade an upgrade rather than a deadlock against oneself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub session: u64,
    pub fh: u64,
    /// The client's own lock-owner id (FUSE's `lock_owner`). Zero for a
    /// pre-v7 whole-file lock, which has no notion of one.
    pub owner: u64,
}

impl Owner {
    pub fn new(session: u64, fh: u64, owner: u64) -> Self {
        Self { session, fh, owner }
    }
}

/// One held interval. `end` is exclusive; `u64::MAX` is "to EOF", which is
/// what `fcntl`'s `l_len == 0` asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Held {
    owner: Owner,
    kind: LockKind,
    start: u64,
    end: u64,
}

impl Held {
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start < end && start < self.end
    }
}

struct Waiter {
    owner: Owner,
    kind: LockKind,
    start: u64,
    end: u64,
    wake: oneshot::Sender<()>,
}

#[derive(Default)]
struct Entry {
    held: Vec<Held>,
    waiters: VecDeque<Waiter>,
}

impl Entry {
    /// The first lock that would block `owner` taking `kind` over
    /// `[start, end)`, or `None` if it would be granted.
    fn conflict(&self, owner: Owner, kind: LockKind, start: u64, end: u64) -> Option<&Held> {
        self.held.iter().find(|h| {
            h.owner != owner
                && h.overlaps(start, end)
                && (kind == LockKind::Exclusive || h.kind == LockKind::Exclusive)
        })
    }

    /// Remove `[start, end)` from everything `owner` holds, keeping whatever
    /// lies outside it. Both an unlock and the room-making half of a re-lock.
    fn carve_out(&mut self, owner: Owner, start: u64, end: u64) {
        let mut fragments = Vec::new();
        self.held.retain(|h| {
            if h.owner != owner || !h.overlaps(start, end) {
                return true;
            }
            if h.start < start {
                fragments.push(Held {
                    start: h.start,
                    end: start,
                    ..*h
                });
            }
            if h.end > end {
                fragments.push(Held {
                    start: end,
                    end: h.end,
                    ..*h
                });
            }
            false
        });
        self.held.extend(fragments);
    }

    /// Join this owner's touching or overlapping same-kind ranges, so a
    /// process locking a file byte by byte does not accumulate a record per
    /// byte.
    fn coalesce(&mut self, owner: Owner) {
        let mut mine: Vec<Held> = Vec::new();
        self.held.retain(|h| {
            if h.owner == owner {
                mine.push(*h);
                false
            } else {
                true
            }
        });
        mine.sort_by_key(|h| (h.start, h.end));
        let mut merged: Vec<Held> = Vec::new();
        for h in mine {
            match merged.last_mut() {
                Some(prev) if prev.kind == h.kind && prev.end >= h.start => {
                    prev.end = prev.end.max(h.end);
                }
                _ => merged.push(h),
            }
        }
        self.held.extend(merged);
    }

    fn is_idle(&self) -> bool {
        self.held.is_empty() && self.waiters.is_empty()
    }
}

#[derive(Default)]
pub struct LockManager {
    entries: Mutex<HashMap<RelPath, Entry>>,
}

impl LockManager {
    /// Take `[start, end)`. On contention either fail with `WouldBlock`
    /// (`wait == false`) or park until it can be granted.
    pub async fn lock_range(
        &self,
        path: &RelPath,
        owner: Owner,
        kind: LockKind,
        start: u64,
        end: u64,
        wait: bool,
    ) -> Result<(), ErrorCode> {
        if start >= end {
            return Err(ErrorCode::InvalidPath);
        }
        loop {
            let rx = {
                let mut entries = self.entries.lock().unwrap();
                let entry = entries.entry(path.clone()).or_default();
                // Tested BEFORE anything is carved out. An owner never
                // conflicts with itself, and what it already holds has to
                // survive an attempt that fails — carving first is what the
                // whole-file version did, and a refused upgrade left the
                // caller holding less than it started with.
                if entry.conflict(owner, kind, start, end).is_none() {
                    entry.carve_out(owner, start, end);
                    entry.held.push(Held {
                        owner,
                        kind,
                        start,
                        end,
                    });
                    entry.coalesce(owner);
                    return Ok(());
                }
                if !wait {
                    return Err(ErrorCode::WouldBlock);
                }
                let (tx, rx) = oneshot::channel();
                entry.waiters.push_back(Waiter {
                    owner,
                    kind,
                    start,
                    end,
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

    /// Release exactly `[start, end)`, splitting whatever it cuts through.
    pub fn unlock_range(&self, path: &RelPath, owner: Owner, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(path) {
            entry.carve_out(owner, start, end);
            Self::wake_waiters(entry);
            if entry.is_idle() {
                entries.remove(path);
            }
        }
    }

    /// `fcntl(F_GETLK)`: what would block this lock, if anything.
    pub fn test_range(
        &self,
        path: &RelPath,
        owner: Owner,
        kind: LockKind,
        start: u64,
        end: u64,
    ) -> Option<LockConflict> {
        let entries = self.entries.lock().unwrap();
        entries
            .get(path)
            .and_then(|e| e.conflict(owner, kind, start, end))
            .map(|h| LockConflict {
                kind: h.kind,
                start: h.start,
                // Back to fcntl's spelling: a range running to EOF is
                // `l_len == 0`, not a length of `u64::MAX - start`.
                len: if h.end == u64::MAX { 0 } else { h.end - h.start },
                // Always 0: the holder is a handle on another machine, and its
                // process id means nothing here.
                pid: 0,
            })
    }

    /// Pre-v7 whole-file lock.
    pub async fn lock(
        &self,
        path: &RelPath,
        session: u64,
        fh: u64,
        kind: LockKind,
        wait: bool,
    ) -> Result<(), ErrorCode> {
        self.lock_range(path, Owner::new(session, fh, 0), kind, 0, u64::MAX, wait)
            .await
    }

    /// Drop every lock a handle holds on a path.
    ///
    /// This is both the pre-v7 `Unlock` and what closing a handle does. For a
    /// v7 client it is reachable only by closing: `UnlockRange` releases
    /// exactly what it names, which is the entire point of the version.
    pub fn unlock(&self, path: &RelPath, session: u64, fh: u64) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(path) {
            entry
                .held
                .retain(|h| !(h.owner.session == session && h.owner.fh == fh));
            Self::wake_waiters(entry);
            if entry.is_idle() {
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
            let before = entry.held.len();
            entry.held.retain(|h| h.owner.session != session);
            released += before - entry.held.len();
            entry.waiters.retain(|w| w.owner.session != session);
            Self::wake_waiters(entry);
            !entry.is_idle()
        });
        released
    }

    /// Wake every waiter whose range is now free.
    ///
    /// Unlike the whole-file version there is no single "compatible" state to
    /// test: two waiters on disjoint ranges can both be grantable while a
    /// third is not, so this walks the queue instead of stopping at the head.
    /// Fairness is kept per range — a waiter is never woken past an earlier
    /// waiter whose range it overlaps — which stops a stream of readers
    /// starving a writer on the same bytes while leaving unrelated ranges free
    /// to proceed.
    fn wake_waiters(entry: &mut Entry) {
        let mut blocked: Vec<(u64, u64)> = Vec::new();
        let mut i = 0;
        while i < entry.waiters.len() {
            let (owner, kind, start, end) = {
                let w = &entry.waiters[i];
                (w.owner, w.kind, w.start, w.end)
            };
            let behind_earlier = blocked.iter().any(|&(bs, be)| start < be && bs < end);
            if !behind_earlier && entry.conflict(owner, kind, start, end).is_none() {
                if let Some(w) = entry.waiters.remove(i) {
                    // Waking is advisory — the waiter re-checks under the mutex.
                    let _ = w.wake.send(());
                }
            } else {
                blocked.push((start, end));
                i += 1;
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

    /// (session, fh) with no client-side owner id, as a coarse lock has.
    fn who(session: u64, fh: u64) -> Owner {
        Owner::new(session, fh, 0)
    }

    // SQLite's byte offsets, which are the whole reason ranges exist here.
    const PENDING: u64 = 0x4000_0000;
    const RESERVED: u64 = PENDING + 1;
    const SHARED_FIRST: u64 = PENDING + 2;
    const SHARED_SIZE: u64 = 510;

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
        let waiter =
            tokio::spawn(async move { lm2.lock(&path(), 2, 1, LockKind::Exclusive, true).await });
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
        lm.lock(&RelPath("y".into()), 7, 2, LockKind::Exclusive, false)
            .await
            .unwrap();
        assert_eq!(lm.release_session(7), 2);
        lm.lock(&RelPath("x".into()), 8, 1, LockKind::Exclusive, false)
            .await
            .unwrap();
    }

    /// A refused upgrade leaves the caller holding exactly what it held.
    ///
    /// The whole-file version released the caller's own hold BEFORE testing
    /// whether the new one could be granted, and never restored it on the
    /// refusal path — so a handle that asked for more and was told no came
    /// away holding LESS than before the call, while its client went on
    /// believing it held the original.
    #[tokio::test]
    async fn a_refused_upgrade_keeps_the_existing_lock() {
        let lm = LockManager::default();
        lm.lock(&path(), 1, 10, LockKind::Shared, false).await.unwrap();
        lm.lock(&path(), 2, 20, LockKind::Shared, false).await.unwrap();
        assert_eq!(
            lm.lock(&path(), 1, 10, LockKind::Exclusive, false).await,
            Err(ErrorCode::WouldBlock)
        );
        // Session 1's shared lock survived: with 2 gone, a third party still
        // cannot take the file exclusively.
        lm.unlock(&path(), 2, 20);
        assert_eq!(
            lm.lock(&path(), 3, 30, LockKind::Exclusive, false).await,
            Err(ErrorCode::WouldBlock)
        );
    }

    /// ...and an owner is never blocked by its own hold, so an uncontended
    /// upgrade still succeeds. Ignoring the caller in the conflict test is
    /// what satisfies both; restoring the hold around a failed attempt would
    /// fix the case above and deadlock the parked one below.
    #[tokio::test]
    async fn a_handle_can_upgrade_and_downgrade_its_own_lock() {
        let lm = LockManager::default();
        lm.lock(&path(), 1, 10, LockKind::Shared, false).await.unwrap();
        lm.lock(&path(), 1, 10, LockKind::Exclusive, false).await.unwrap();
        assert_eq!(
            lm.lock(&path(), 2, 20, LockKind::Shared, false).await,
            Err(ErrorCode::WouldBlock)
        );
        lm.lock(&path(), 1, 10, LockKind::Shared, false).await.unwrap();
        lm.lock(&path(), 2, 20, LockKind::Shared, false).await.unwrap();
    }

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

    // ---------------------------------------------------------- byte ranges

    #[tokio::test]
    async fn disjoint_ranges_do_not_conflict() {
        let lm = LockManager::default();
        lm.lock_range(&path(), who(1, 10), LockKind::Exclusive, 0, 100, false)
            .await
            .unwrap();
        // Adjacent, not overlapping: [0,100) and [100,200) share no byte.
        lm.lock_range(&path(), who(2, 20), LockKind::Exclusive, 100, 200, false)
            .await
            .unwrap();
        // Overlapping by exactly one byte is a conflict.
        assert_eq!(
            lm.lock_range(&path(), who(3, 30), LockKind::Exclusive, 99, 150, false)
                .await,
            Err(ErrorCode::WouldBlock)
        );
    }

    #[tokio::test]
    async fn unlocking_the_middle_splits_a_held_range() {
        let lm = LockManager::default();
        let a = who(1, 10);
        lm.lock_range(&path(), a, LockKind::Exclusive, 0, 300, false)
            .await
            .unwrap();
        lm.unlock_range(&path(), a, 100, 200);

        // The hole is free...
        lm.lock_range(&path(), who(2, 20), LockKind::Exclusive, 100, 200, false)
            .await
            .unwrap();
        // ...and both fragments are still held.
        assert_eq!(
            lm.lock_range(&path(), who(3, 30), LockKind::Exclusive, 50, 60, false)
                .await,
            Err(ErrorCode::WouldBlock)
        );
        assert_eq!(
            lm.lock_range(&path(), who(3, 30), LockKind::Exclusive, 250, 260, false)
                .await,
            Err(ErrorCode::WouldBlock)
        );
    }

    #[tokio::test]
    async fn test_range_reports_the_blocking_lock() {
        let lm = LockManager::default();
        lm.lock_range(&path(), who(1, 10), LockKind::Exclusive, 40, 50, false)
            .await
            .unwrap();
        // Free for a range that does not touch it.
        assert!(lm
            .test_range(&path(), who(2, 20), LockKind::Exclusive, 0, 40)
            .is_none());
        // ...and reports the holder for one that does.
        let c = lm
            .test_range(&path(), who(2, 20), LockKind::Shared, 45, 60)
            .expect("should be blocked");
        assert_eq!(c.kind, LockKind::Exclusive);
        assert_eq!((c.start, c.len), (40, 10));
        // An owner never blocks itself, which is what makes F_GETLK usable for
        // deciding whether an upgrade would be granted.
        assert!(lm
            .test_range(&path(), who(1, 10), LockKind::Exclusive, 40, 50)
            .is_none());
    }

    /// A range running to EOF round-trips as fcntl spells it: `l_len == 0`.
    #[tokio::test]
    async fn a_to_eof_range_reports_length_zero() {
        let lm = LockManager::default();
        lm.lock_range(&path(), who(1, 10), LockKind::Exclusive, 4096, u64::MAX, false)
            .await
            .unwrap();
        let c = lm
            .test_range(&path(), who(2, 20), LockKind::Shared, 8192, 8193)
            .expect("blocked");
        assert_eq!((c.start, c.len), (4096, 0));
    }

    /// The sequence whole-file coarsening broke, run exactly as SQLite runs it.
    ///
    /// Taking a SHARED lock is: read-lock PENDING, read-lock the shared range,
    /// then UNLOCK PENDING. Coarsened, that last step released the second — so
    /// the connection believed it held SHARED while the server held nothing.
    /// Then RESERVED, a write lock one byte away from the shared range, has to
    /// be grantable while another connection still reads, which coarsening
    /// also made impossible.
    #[tokio::test]
    async fn the_sqlite_locking_sequence_works() {
        let lm = LockManager::default();
        let a = who(1, 10);
        let b = who(2, 20);

        // A takes SHARED.
        lm.lock_range(&path(), a, LockKind::Shared, PENDING, PENDING + 1, false)
            .await
            .unwrap();
        lm.lock_range(
            &path(),
            a,
            LockKind::Shared,
            SHARED_FIRST,
            SHARED_FIRST + SHARED_SIZE,
            false,
        )
        .await
        .unwrap();
        lm.unlock_range(&path(), a, PENDING, PENDING + 1);

        // The shared range is STILL held: releasing PENDING released only
        // PENDING. This is the assertion the old behaviour could not pass.
        assert!(lm
            .test_range(&path(), b, LockKind::Exclusive, SHARED_FIRST, SHARED_FIRST + 1)
            .is_some());
        // ...and PENDING really is free again.
        assert!(lm
            .test_range(&path(), b, LockKind::Exclusive, PENDING, PENDING + 1)
            .is_none());

        // B takes SHARED too: two readers coexist.
        lm.lock_range(&path(), b, LockKind::Shared, PENDING, PENDING + 1, false)
            .await
            .unwrap();
        lm.lock_range(
            &path(),
            b,
            LockKind::Shared,
            SHARED_FIRST,
            SHARED_FIRST + SHARED_SIZE,
            false,
        )
        .await
        .unwrap();
        lm.unlock_range(&path(), b, PENDING, PENDING + 1);

        // A now takes RESERVED while B keeps reading. Disjoint from the shared
        // range by one byte, so it is granted — this is how a writer stages a
        // transaction without stopping readers, and what coarsening turned
        // into a permanent SQLITE_BUSY.
        lm.lock_range(&path(), a, LockKind::Exclusive, RESERVED, RESERVED + 1, false)
            .await
            .unwrap();

        // Only one RESERVED at a time, though.
        assert_eq!(
            lm.lock_range(&path(), b, LockKind::Exclusive, RESERVED, RESERVED + 1, false)
                .await,
            Err(ErrorCode::WouldBlock)
        );

        // A cannot go EXCLUSIVE while B still reads the shared range...
        assert!(lm
            .test_range(
                &path(),
                a,
                LockKind::Exclusive,
                SHARED_FIRST,
                SHARED_FIRST + SHARED_SIZE
            )
            .is_some());
        // ...and can once B is gone.
        lm.release_session(2);
        lm.lock_range(
            &path(),
            a,
            LockKind::Exclusive,
            SHARED_FIRST,
            SHARED_FIRST + SHARED_SIZE,
            false,
        )
        .await
        .unwrap();
    }

    /// Random lock/unlock sequences checked against a naive per-byte model.
    ///
    /// The interval code splits, merges and coalesces; the model just marks
    /// bytes. If the two ever disagree about whether a byte is held, and by
    /// whom, the clever version is the one that is wrong. A fixed LCG rather
    /// than a random seed, so a failure is reproducible from the printed step.
    #[tokio::test]
    async fn interval_bookkeeping_matches_a_per_byte_model() {
        const SIZE: usize = 64;
        const OWNERS: usize = 3;
        let lm = LockManager::default();
        // model[byte][owner] = Some(is_exclusive). Per OWNER rather than one
        // holder per byte: several owners can hold the same byte shared at
        // once, and a model that cannot say so reports a byte as free the
        // moment a second reader takes it.
        let mut model: Vec<[Option<bool>; OWNERS]> = vec![[None; OWNERS]; SIZE];
        let owners = [who(1, 10), who(2, 20), who(3, 30)];

        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };

        for step in 0..400 {
            let oi = next() % owners.len();
            let owner = owners[oi];
            let start = next() % SIZE;
            let len = 1 + next() % 8;
            let end = (start + len).min(SIZE);
            let unlock = next() % 3 == 0;
            let exclusive = next() % 2 == 0;
            let kind = if exclusive {
                LockKind::Exclusive
            } else {
                LockKind::Shared
            };

            if unlock {
                lm.unlock_range(&path(), owner, start as u64, end as u64);
                for b in model.iter_mut().take(end).skip(start) {
                    b[oi] = None;
                }
                continue;
            }

            // Would the model grant it? Conflict iff some byte in range is
            // held by ANOTHER owner and either side is exclusive.
            let blocked = (start..end).any(|b| {
                (0..OWNERS).any(|o| o != oi && matches!(model[b][o], Some(ex) if ex || exclusive))
            });
            let got = lm
                .lock_range(&path(), owner, kind, start as u64, end as u64, false)
                .await;
            assert_eq!(
                got.is_err(),
                blocked,
                "step {step}: owner {oi} {kind:?} [{start},{end}) — model says blocked={blocked}, \
                 manager said {got:?}"
            );
            if !blocked {
                for b in model.iter_mut().take(end).skip(start) {
                    b[oi] = Some(exclusive);
                }
            }
        }

        // Final agreement, byte by byte: whatever the model says is held must
        // block an outsider, and whatever it says is free must not.
        let outsider = who(9, 99);
        for (b, held) in model.iter().enumerate() {
            let free_in_model = held.iter().all(|h| h.is_none());
            let free_for_write = lm
                .test_range(&path(), outsider, LockKind::Exclusive, b as u64, b as u64 + 1)
                .is_none();
            assert_eq!(
                free_for_write, free_in_model,
                "byte {b}: model {held:?} but manager says free_for_write={free_for_write}"
            );
        }
    }
}
