//! Advisory locks: the client's mirror of what each handle holds on the
//! server, and the lock operations themselves.
//!
//! The mirror exists for one consumer — the reconnect supervisor, which
//! replays exactly what was held onto the new session. Keeping the shapes
//! identical to the agent's is what makes that replay honest.

use alloyfs_proto::{ErrorCode, Request, Response};

use crate::error::FsError;
use crate::overlay::OVERLAY_FH_BIT;

use super::RemoteFs;

/// One advisory lock a handle holds, in the client's mirror of server state.
///
/// `end` is exclusive, `u64::MAX` meaning "to the end of the file" — the same
/// convention the agent keeps, so nothing has to be reinterpreted when a
/// reconnect replays it. The wire spells that as `len == 0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HeldRange {
    pub owner: u64,
    pub kind: alloyfs_proto::LockKind,
    pub start: u64,
    pub end: u64,
}

impl HeldRange {
    /// Back to the wire's spelling.
    pub(super) fn wire_len(&self) -> u64 {
        if self.end == u64::MAX {
            0
        } else {
            self.end - self.start
        }
    }
}

/// Exclusive end from the wire's `(start, len)`, where `len == 0` is to-EOF.
pub(crate) fn range_end(start: u64, len: u64) -> u64 {
    if len == 0 {
        u64::MAX
    } else {
        start.saturating_add(len)
    }
}

/// Record that `owner` holds `[start, end)` as `kind`, replacing whatever it
/// held there.
///
/// A trimmed mirror of the agent's carve-then-insert: only this handle's own
/// ranges matter here, since conflicts are the server's business. Keeping the
/// shapes the same is what lets a reconnect replay exactly what the server
/// has, rather than a superset that would claim more than was held.
pub(crate) fn record_lock(
    held: &mut Vec<HeldRange>,
    owner: u64,
    kind: alloyfs_proto::LockKind,
    start: u64,
    end: u64,
) {
    record_unlock(held, owner, start, end);
    held.push(HeldRange {
        owner,
        kind,
        start,
        end,
    });
}

/// Drop `[start, end)` from what `owner` holds, splitting anything it cuts
/// through.
pub(crate) fn record_unlock(held: &mut Vec<HeldRange>, owner: u64, start: u64, end: u64) {
    let mut fragments = Vec::new();
    held.retain(|h| {
        if h.owner != owner || !(h.start < end && start < h.end) {
            return true;
        }
        if h.start < start {
            fragments.push(HeldRange { end: start, ..*h });
        }
        if h.end > end {
            fragments.push(HeldRange { start: end, ..*h });
        }
        false
    });
    held.extend(fragments);
}

impl RemoteFs {
    pub fn lock(&self, fh: u64, kind: alloyfs_proto::LockKind, wait: bool) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(()); // single-machine data: advisory lock is a no-op
        }
        self.check_poisoned(fh)?;
        let server_fh = self.server_fh_for_io(fh)?;
        let req = Request::Lock {
            fh: server_fh,
            kind,
            wait,
        };
        // A blocking wait may legitimately outlast any fixed timeout: bound
        // it by peer liveness instead (fails fast on disconnect, ~20 s on a
        // wedged-but-open server, forever-patient on a healthy busy one).
        let resp = if wait {
            self.rt.block_on(self.conn().request_keepalive(req))??
        } else {
            self.call(req)?
        };
        expect_resp!(resp, Response::Ok => ());
        if let Some(state) = self.open_files.get(&fh) {
            // Whole file, owner 0 — the coarse lock has no notion of either,
            // and recording it the same shape as a range keeps one replay path.
            let mut held = state.lock.lock().unwrap();
            held.clear(); // server did release-then-take
            record_lock(&mut held, 0, kind, 0, u64::MAX);
        }
        Ok(())
    }

    pub fn unlock(&self, fh: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(());
        }
        let server_fh = self.server_fh_for_io(fh)?;
        expect_resp!(self.call(Request::Unlock { fh: server_fh })?, Response::Ok => ());
        if let Some(state) = self.open_files.get(&fh) {
            state.lock.lock().unwrap().clear();
        }
        Ok(())
    }

    /// v7+: take a byte-range lock. `len == 0` is fcntl's "to the end of the
    /// file".
    pub fn lock_range(
        &self,
        fh: u64,
        owner: u64,
        kind: alloyfs_proto::LockKind,
        start: u64,
        len: u64,
        wait: bool,
    ) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(()); // single-machine data: advisory lock is a no-op
        }
        self.check_poisoned(fh)?;
        self.require_proto(7, "byte-range lock")?;
        let server_fh = self.server_fh_for_io(fh)?;
        let req = Request::LockRange {
            fh: server_fh,
            owner,
            kind,
            start,
            len,
            wait,
        };
        // A blocking wait may legitimately outlast any fixed timeout: bound it
        // by peer liveness instead, as the whole-file path does.
        let resp = if wait {
            self.rt.block_on(self.conn().request_keepalive(req))??
        } else {
            self.call(req)?
        };
        expect_resp!(resp, Response::Ok => ());
        if let Some(state) = self.open_files.get(&fh) {
            let mut held = state.lock.lock().unwrap();
            record_lock(&mut held, owner, kind, start, range_end(start, len));
        }
        Ok(())
    }

    /// v7+: release exactly `[start, len)` and nothing else.
    pub fn unlock_range(&self, fh: u64, owner: u64, start: u64, len: u64) -> Result<(), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(());
        }
        self.require_proto(7, "byte-range unlock")?;
        let server_fh = self.server_fh_for_io(fh)?;
        expect_resp!(
            self.call(Request::UnlockRange {
                fh: server_fh,
                owner,
                start,
                len,
            })?,
            Response::Ok => ()
        );
        if let Some(state) = self.open_files.get(&fh) {
            let mut held = state.lock.lock().unwrap();
            record_unlock(&mut held, owner, start, range_end(start, len));
        }
        Ok(())
    }

    /// v7+: `fcntl(F_GETLK)` — what would block this lock, if anything.
    pub fn test_lock(
        &self,
        fh: u64,
        owner: u64,
        kind: alloyfs_proto::LockKind,
        start: u64,
        len: u64,
    ) -> Result<Option<alloyfs_proto::LockConflict>, FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return Ok(None); // nothing else can hold overlay-routed data
        }
        self.check_poisoned(fh)?;
        self.require_proto(7, "test lock")?;
        let server_fh = self.server_fh_for_io(fh)?;
        let resp = self.call(Request::TestLock {
            fh: server_fh,
            owner,
            kind,
            start,
            len,
        })?;
        Ok(expect_resp!(resp, Response::LockStatus(c) => c))
    }
}
