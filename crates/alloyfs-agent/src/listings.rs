//! Directory-listing snapshots: build a listing once, page from the copy.
//!
//! `readdir` is paged (1024 entries per reply), and both serve paths used to
//! redo their full work on EVERY page. The disk path re-read and re-stat the
//! whole directory per page — O(N) stats per page, O(N²) to list a big
//! directory, at 50-80 µs a stat on this class of disk. The index path
//! re-walked the directory's whole subtree region per page to re-find the
//! offset. So the first page of a 10k-entry directory cost the same as the
//! tenth, and the whole listing cost pages × that.
//!
//! Now the first page builds the COMPLETE listing once and parks it here;
//! continuation pages are a lookup and a slice. Bonus: a multi-page listing
//! is served from one consistent snapshot instead of re-deriving the world
//! per page, so a mutation mid-pagination can no longer duplicate or drop
//! entries around a page boundary — the churn cases fall to rebuilds
//! instead.
//!
//! Freshness is not guessed at:
//! - Index-built snapshots carry the tree token they were built under and
//!   are dropped the moment the live token differs.
//! - Every agent-mediated mutation (the version funnel: bump /
//!   forget_version / rename_version — the watcher feeds the same funnel)
//!   drops the snapshots it could have changed: the path's parent and the
//!   path itself.
//! - Disk-built snapshots (unindexed exports have no token) additionally
//!   age out after [`DISK_SNAPSHOT_TTL`] — long enough to page a huge
//!   directory over a WAN, short enough that out-of-band server-side edits
//!   are seen on the next listing.
//!
//! Snapshots are pure performance: dropping one at any moment is always
//! correct, so eviction is allowed to be crude.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloyfs_proto::{DirEntry, RelPath};
use dashmap::DashMap;

/// How long a DISK-built snapshot may serve continuation pages. A 100k-entry
/// directory pages ~100 replies; at WAN round-trip pacing that is tens of
/// seconds, and expiring mid-listing would re-pay the O(N) stat sweep per
/// remaining page — the exact cost this exists to kill.
const DISK_SNAPSHOT_TTL: Duration = Duration::from_secs(30);

/// Total entries parked across all snapshots before the cache clears itself.
/// Listings under one page are never parked, so this only bounds the rare
/// several-huge-directories case; clearing is safe (see module doc).
const MAX_TOTAL_ENTRIES: usize = 262_144;

pub struct Snapshot {
    pub entries: Arc<Vec<DirEntry>>,
    /// Tree token the listing was built under; 0 = built from disk.
    pub token: u64,
    born: Instant,
}

impl Snapshot {
    /// Valid against the CURRENT tree token (index builds), or young enough
    /// (disk builds).
    pub fn still_fresh(&self, live_token: u64) -> bool {
        if self.token != 0 {
            self.token == live_token
        } else {
            self.born.elapsed() <= DISK_SNAPSHOT_TTL
        }
    }
}

#[derive(Default)]
pub struct ListingCache {
    map: DashMap<RelPath, Snapshot>,
    total: AtomicUsize,
}

impl ListingCache {
    pub fn get(&self, dir: &RelPath) -> Option<Arc<Vec<DirEntry>>> {
        self.map.get(dir).map(|s| s.entries.clone())
    }

    /// `get` that also checks freshness, dropping a stale snapshot in place.
    pub fn get_fresh(&self, dir: &RelPath, live_token: u64) -> Option<Arc<Vec<DirEntry>>> {
        let hit = self.map.get(dir)?;
        if hit.still_fresh(live_token) {
            return Some(hit.entries.clone());
        }
        drop(hit);
        self.remove(dir);
        None
    }

    pub fn put(&self, dir: RelPath, entries: Arc<Vec<DirEntry>>, token: u64) {
        let added = entries.len();
        // Crude by design: blowing the budget clears everything rather than
        // tracking recency. Snapshots exist for directories big enough to
        // page, several of those at once is already rare, and a clear only
        // costs the next listing a rebuild.
        if self.total.fetch_add(added, Ordering::Relaxed) + added > MAX_TOTAL_ENTRIES {
            self.clear();
            self.total.store(added, Ordering::Relaxed);
        }
        if let Some(old) = self.map.insert(
            dir,
            Snapshot {
                entries,
                token,
                born: Instant::now(),
            },
        ) {
            self.total.fetch_sub(old.entries.len(), Ordering::Relaxed);
        }
    }

    pub fn remove(&self, dir: &RelPath) {
        if let Some((_, old)) = self.map.remove(dir) {
            self.total.fetch_sub(old.entries.len(), Ordering::Relaxed);
        }
    }

    /// Invalidation hook for the version funnel: a mutation at `path` can
    /// change the listing of its parent (membership, the entry's attrs) and
    /// of the path itself (when it is a directory being listed).
    pub fn drop_for(&self, path: &RelPath) {
        self.remove(path);
        let parent = match path.0.rsplit_once('/') {
            Some((dir, _)) => RelPath(dir.to_string()),
            None => RelPath(String::new()),
        };
        self.remove(&parent);
    }

    pub fn clear(&self) {
        self.map.clear();
        self.total.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloyfs_proto::{Attr, FileKind};

    fn entry(name: &str) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            attr: Attr {
                kind: FileKind::File,
                size: 0,
                mtime: std::time::UNIX_EPOCH,
                ctime: std::time::UNIX_EPOCH,
                mode: 0o644,
                version: 0,
            },
        }
    }

    #[test]
    fn mutations_drop_the_parent_and_the_path_itself() {
        let cache = ListingCache::default();
        let root_listing = Arc::new(vec![entry("d"), entry("f")]);
        let d_listing = Arc::new(vec![entry("x")]);
        cache.put(RelPath(String::new()), root_listing, 7);
        cache.put(RelPath("d".into()), d_listing, 7);

        // A mutation at d/x invalidates d's listing (membership) but not
        // the root's (d itself did not change name or existence… its attrs
        // may — which is why drop_for of "d" also removes root).
        cache.drop_for(&RelPath("d/x".into()));
        assert!(cache.get(&RelPath("d".into())).is_none());
        assert!(cache.get(&RelPath(String::new())).is_some());

        cache.drop_for(&RelPath("d".into()));
        assert!(cache.get(&RelPath(String::new())).is_none());
    }

    #[test]
    fn token_mismatch_is_stale_and_disk_builds_age_instead() {
        let cache = ListingCache::default();
        cache.put(RelPath("a".into()), Arc::new(vec![entry("x")]), 5);
        assert!(cache.get_fresh(&RelPath("a".into()), 5).is_some());
        assert!(
            cache.get_fresh(&RelPath("a".into()), 6).is_none(),
            "index snapshot must die with its token"
        );
        assert!(
            cache.get(&RelPath("a".into())).is_none(),
            "the stale hit is dropped in place, not left to lie again"
        );

        // Disk-built (token 0): fresh regardless of the live token while
        // young — TTL is its only clock.
        cache.put(RelPath("b".into()), Arc::new(vec![entry("y")]), 0);
        assert!(cache.get_fresh(&RelPath("b".into()), 999).is_some());
    }

    #[test]
    fn the_entry_budget_clears_rather_than_grows() {
        let cache = ListingCache::default();
        let big: Arc<Vec<DirEntry>> = Arc::new((0..200_000).map(|i| entry(&format!("f{i}"))).collect());
        cache.put(RelPath("one".into()), big.clone(), 1);
        assert!(cache.get(&RelPath("one".into())).is_some());
        cache.put(RelPath("two".into()), big, 1);
        assert!(
            cache.get(&RelPath("one".into())).is_none(),
            "second oversized snapshot must clear the first, not stack on it"
        );
        assert!(cache.get(&RelPath("two".into())).is_some());
    }
}
