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
//! continuation pages are a lookup and a slice. That also makes a multi-page
//! listing consistent — one snapshot rather than the world re-derived per
//! page — so a mutation mid-pagination cannot duplicate or drop entries
//! around a page boundary.
//!
//! A snapshot is NOT a cache of "the current listing", and that distinction
//! decides every rule below. It is one scan's state, read only by the scan
//! that parked it: a NEW scan (cursor 0) rebuilds unconditionally and never
//! looks in this map, so nothing here can answer with a stale directory no
//! matter how old it gets.
//!
//! Which is why there is no freshness check. There were three — the tree
//! token an index build ran under, a TTL on disk builds, and an eager drop
//! from every agent-mediated mutation — and all three answered the wrong
//! question. Pages are addressed by POSITION, so discarding a snapshot
//! mid-scan did not make that scan fresher: it made the next page rebuild a
//! SHORTER listing and apply the old position to it, skipping the entries in
//! between. Those entries had not been touched. POSIX lets a scan miss only
//! what is added or removed while it runs, not what sits still — and
//! `find -delete`, or any shell loop over a directory, is exactly a mutation
//! interleaved with a scan. The token made it worse than it looks: one
//! global token for the whole export, so a write to ANY file dropped every
//! in-flight listing.
//!
//! So a continuation names its snapshot by generation and pages that one.
//! A generation that is gone — evicted by the entry budget, or replaced by a
//! concurrent scan of the same directory — still falls back to a rebuild at
//! the old position, and that fallback can still skip. What changed is that
//! reaching it now requires the snapshot to be genuinely lost, instead of
//! happening on every mutation anywhere in the export.
//!
//! Eviction is allowed to be crude: it costs a scan its consistency, which
//! is the same price the fallback pays, and the budget is sized so that
//! several simultaneous huge listings are what it takes to trip it.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use alloyfs_proto::{DirEntry, RelPath};
use dashmap::DashMap;

/// Total entries parked across all snapshots before the cache clears itself.
/// Listings under one page are never parked, so this only bounds the rare
/// several-huge-directories case; clearing is safe (see module doc).
const MAX_TOTAL_ENTRIES: usize = 262_144;

pub struct Snapshot {
    pub entries: Arc<Vec<DirEntry>>,
    /// Which scan parked this. A continuation carries the generation it was
    /// issued under, so it can tell "my listing" from "someone else's
    /// listing of the same directory" — the one thing a bare path key
    /// cannot express.
    generation: u64,
}

#[derive(Default)]
pub struct ListingCache {
    map: DashMap<RelPath, Snapshot>,
    total: AtomicUsize,
    generations: AtomicU64,
}

impl ListingCache {
    /// Whatever is parked for `dir`, for tests and diagnostics. Serving a
    /// continuation goes through [`Self::get_generation`] instead: it is the
    /// generation, not the path, that says this listing belongs to the scan
    /// asking for it.
    pub fn get(&self, dir: &RelPath) -> Option<Arc<Vec<DirEntry>>> {
        self.map.get(dir).map(|s| s.entries.clone())
    }

    /// The listing a continuation is paging, or `None` when that scan's
    /// snapshot is gone and the caller has to rebuild.
    pub fn get_generation(&self, dir: &RelPath, generation: u64) -> Option<Arc<Vec<DirEntry>>> {
        let hit = self.map.get(dir)?;
        (hit.generation == generation).then(|| hit.entries.clone())
    }

    /// Park a freshly built listing; the returned generation is what names it
    /// in the cursors handed to the client.
    pub fn put(&self, dir: RelPath, entries: Arc<Vec<DirEntry>>) -> u64 {
        // Never 0 — a cursor with generation 0 means "no snapshot", which is
        // what a pre-generation cursor and a single-page listing both look
        // like. Wrapping at 32 bits keeps the generation inside the cursor's
        // high half; a collision would need the same directory to still hold
        // a snapshot from 4 billion listings ago, and `put` replaces.
        let generation = (self.generations.fetch_add(1, Ordering::Relaxed) % 0xffff_fffe) + 1;
        let added = entries.len();
        // Crude by design: blowing the budget clears everything rather than
        // tracking recency. Snapshots exist for directories big enough to
        // page, several of those at once is already rare, and a clear only
        // costs the next listing a rebuild.
        if self.total.fetch_add(added, Ordering::Relaxed) + added > MAX_TOTAL_ENTRIES {
            self.clear();
            self.total.store(added, Ordering::Relaxed);
        }
        if let Some(old) = self.map.insert(dir, Snapshot { entries, generation }) {
            self.total.fetch_sub(old.entries.len(), Ordering::Relaxed);
        }
        generation
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
    fn a_scan_pages_its_own_snapshot_however_old_it_is() {
        let cache = ListingCache::default();
        let generation = cache.put(RelPath("d".into()), Arc::new(vec![entry("x"), entry("y")]));
        assert!(generation != 0, "0 is reserved for 'no snapshot'");

        // Nothing invalidates this: age and concurrent mutation elsewhere
        // were both grounds for dropping it, and both dropped a scan that
        // was still running. The only reader is the scan that parked it.
        let paged = cache
            .get_generation(&RelPath("d".into()), generation)
            .expect("the scan that parked it must still be able to page it");
        assert_eq!(paged.len(), 2);
    }

    #[test]
    fn a_concurrent_scan_of_the_same_directory_does_not_alias() {
        let cache = ListingCache::default();
        let first = cache.put(RelPath("d".into()), Arc::new(vec![entry("x"), entry("y")]));
        // Another client starts its own scan of the same directory; one
        // snapshot per path means this replaces the first.
        let second = cache.put(RelPath("d".into()), Arc::new(vec![entry("y")]));
        assert_ne!(first, second);

        assert!(
            cache.get_generation(&RelPath("d".into()), first).is_none(),
            "the first scan must rebuild rather than silently page the second scan's listing"
        );
        assert!(cache.get_generation(&RelPath("d".into()), second).is_some());
    }

    #[test]
    fn an_unknown_generation_is_a_miss_not_a_wrong_answer() {
        let cache = ListingCache::default();
        cache.put(RelPath("d".into()), Arc::new(vec![entry("x")]));
        assert!(cache.get_generation(&RelPath("d".into()), 0).is_none());
        assert!(cache.get_generation(&RelPath("d".into()), u64::MAX).is_none());
        assert!(cache.get_generation(&RelPath("other".into()), 1).is_none());
    }

    #[test]
    fn the_entry_budget_clears_rather_than_grows() {
        let cache = ListingCache::default();
        let big: Arc<Vec<DirEntry>> = Arc::new((0..200_000).map(|i| entry(&format!("f{i}"))).collect());
        cache.put(RelPath("one".into()), big.clone());
        assert!(cache.get(&RelPath("one".into())).is_some());
        cache.put(RelPath("two".into()), big);
        assert!(
            cache.get(&RelPath("one".into())).is_none(),
            "second oversized snapshot must clear the first, not stack on it"
        );
        assert!(cache.get(&RelPath("two".into())).is_some());
    }
}
