//! Who has a file open for writing, across every session of one export.
//!
//! Byte-range locks are how a filesystem normally stops two writers colliding,
//! and on Windows they never reach us: WinFsp services `IRP_MJ_LOCK_CONTROL`
//! inside its own driver and exposes no callback, so a Windows client's locks
//! are enforced on its own machine and nowhere else (see
//! `docs/guides/locking.md`). Two Windows machines writing one SQLite database
//! through the same export therefore believe they are serialised and are not.
//!
//! The agent cannot fix that — it is never asked for the lock — but it is the
//! one process that can SEE it. Every session's opens pass through here, so
//! "two different sessions hold this path open for writing" is a fact
//! available exactly once, in the middle, and nowhere else in the system.
//!
//! So this reports rather than prevents. Refusing the second open would be a
//! filesystem inventing a restriction the protocol never promised, and would
//! break the ordinary case of one client reopening a file it already has open.
//! A warning names the file and the sessions, which is what turns silent
//! corruption into something an operator can find in `alloyfs logs`.

use std::collections::HashMap;
use std::sync::Mutex;

use alloyfs_proto::RelPath;

/// One writable open: which session, which handle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Writer {
    session: u64,
    fh: u64,
}

#[derive(Default)]
pub struct WriterRegistry {
    open: Mutex<HashMap<RelPath, Vec<Writer>>>,
}

/// What a registration found. Returned rather than logged inside so the
/// caller decides how loud to be, and so tests can assert on it.
#[derive(Debug, PartialEq, Eq)]
pub enum Overlap {
    /// Nobody else is writing this path.
    None,
    /// Another handle IN THE SAME SESSION has it open for writing. Ordinary:
    /// one client may hold a file open twice, and its own machine's locks
    /// order those correctly.
    SameSession,
    /// A DIFFERENT session has it open for writing — the case no lock
    /// protects on Windows. Carries the other session's id.
    OtherSession(u64),
}

impl WriterRegistry {
    /// Record a writable open. The answer describes what was already there.
    pub fn opened(&self, path: &RelPath, session: u64, fh: u64) -> Overlap {
        let mut map = self.open.lock().unwrap();
        let entry = map.entry(path.clone()).or_default();
        // The FIRST foreign session is the one worth naming; a third would
        // add nothing an operator does not already know from the second.
        let overlap = match entry.iter().find(|w| w.session != session) {
            Some(other) => Overlap::OtherSession(other.session),
            None if !entry.is_empty() => Overlap::SameSession,
            None => Overlap::None,
        };
        entry.push(Writer { session, fh });
        overlap
    }

    /// Drop one writable open. Removing the whole path when the last writer
    /// goes keeps this map the size of what is actually open, not of
    /// everything ever opened.
    pub fn closed(&self, path: &RelPath, session: u64, fh: u64) {
        let mut map = self.open.lock().unwrap();
        if let Some(entry) = map.get_mut(path) {
            if let Some(i) = entry.iter().position(|w| w.session == session && w.fh == fh) {
                entry.remove(i);
            }
            if entry.is_empty() {
                map.remove(path);
            }
        }
    }

    /// Drop everything a session held. A client that vanishes never sends its
    /// releases, and without this its writers would be reported as live
    /// forever — every later open of those paths a false alarm.
    pub fn session_gone(&self, session: u64) {
        let mut map = self.open.lock().unwrap();
        map.retain(|_, entry| {
            entry.retain(|w| w.session != session);
            !entry.is_empty()
        });
    }

    /// Paths currently held for writing by more than one session.
    pub fn contended(&self) -> Vec<(RelPath, Vec<u64>)> {
        let map = self.open.lock().unwrap();
        map.iter()
            .filter_map(|(p, ws)| {
                let mut ids: Vec<u64> = ws.iter().map(|w| w.session).collect();
                ids.sort_unstable();
                ids.dedup();
                (ids.len() > 1).then(|| (p.clone(), ids))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath(s.into())
    }

    /// The case this exists for: two sessions, one file, both writing.
    #[test]
    fn a_second_session_writing_the_same_path_is_reported() {
        let r = WriterRegistry::default();
        assert_eq!(r.opened(&p("db.sqlite"), 1, 10), Overlap::None);
        assert_eq!(
            r.opened(&p("db.sqlite"), 2, 20),
            Overlap::OtherSession(1),
            "the other session is named, not just the fact of it"
        );
        assert_eq!(r.contended(), vec![(p("db.sqlite"), vec![1, 2])]);
    }

    /// One client holding a file open twice is ordinary, and its own machine
    /// orders those correctly. Reporting it would train people to ignore the
    /// warning that matters.
    #[test]
    fn a_second_handle_in_one_session_is_not_a_conflict() {
        let r = WriterRegistry::default();
        r.opened(&p("a.txt"), 7, 1);
        assert_eq!(r.opened(&p("a.txt"), 7, 2), Overlap::SameSession);
        assert!(r.contended().is_empty(), "one session is never contention");
    }

    #[test]
    fn closing_releases_the_path() {
        let r = WriterRegistry::default();
        r.opened(&p("a.txt"), 1, 10);
        r.opened(&p("a.txt"), 2, 20);
        assert_eq!(r.contended().len(), 1);
        r.closed(&p("a.txt"), 2, 20);
        assert!(r.contended().is_empty(), "one writer left is not contention");
        r.closed(&p("a.txt"), 1, 10);
        assert_eq!(
            r.opened(&p("a.txt"), 3, 30),
            Overlap::None,
            "and the path is clear again"
        );
    }

    /// A client that vanishes sends no releases. Its writers must not haunt
    /// the map, or every later open of those paths is a false alarm.
    #[test]
    fn a_vanished_session_stops_being_reported() {
        let r = WriterRegistry::default();
        r.opened(&p("a.txt"), 1, 10);
        r.opened(&p("b.txt"), 1, 11);
        r.opened(&p("a.txt"), 2, 20);
        assert_eq!(r.contended().len(), 1);

        r.session_gone(1);
        assert!(r.contended().is_empty());
        assert_eq!(
            r.opened(&p("b.txt"), 3, 30),
            Overlap::None,
            "the dead session's other paths are clear too"
        );
    }

    /// Closing a handle must not take a sibling's registration with it.
    #[test]
    fn closing_one_handle_leaves_the_others() {
        let r = WriterRegistry::default();
        r.opened(&p("a.txt"), 1, 10);
        r.opened(&p("a.txt"), 1, 11);
        r.closed(&p("a.txt"), 1, 10);
        assert_eq!(
            r.opened(&p("a.txt"), 2, 20),
            Overlap::OtherSession(1),
            "session 1 still holds it through fh 11"
        );
    }
}
