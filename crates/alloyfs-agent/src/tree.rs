//! The export index: one walk, then maintained from the event feed.
//!
//! A mount is latency-bound, and walking an export the ordinary way costs one
//! `Readdir` round trip per directory. The client's cache walker measured 535
//! directories in 35.8 s against a 60 ms link — almost all of it waiting. The
//! server already has the answer locally: enumerating 3601 entries with stat
//! took 33 ms, and the result compressed to 44 KB. This module holds that
//! enumeration so one exchange can carry what used to take hundreds.
//!
//! Three properties make it simple rather than fiddly:
//!
//! **The walk happens once per process.** It is lazy — the first client to ask
//! for a tree pays for it, and an export nobody indexes is never walked — and
//! after that the existing event feed maintains it. A changed path costs one
//! local stat, not a re-walk.
//!
//! **Rebuilding is cheap, so rebuilding is the recovery strategy.** At 33 ms
//! there is no reason to write a reconciliation protocol: on watcher overflow,
//! on a token that will not settle, on anything doubtful at all, drop the index
//! and walk again. That decision is what keeps the rest of this file short.
//!
//! **The token is commutative**, so maintaining it costs nothing. A digest that
//! had to be recomputed over the whole map on every event would make the
//! incremental path O(n) and quietly undo the point.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use alloyfs_common::{attr_from_metadata, ExcludeSet};
use alloyfs_proto::{Attr, FileKind, RelPath};

/// Entries above which an export is left unindexed and clients keep using
/// `Readdir`.
///
/// Memory is roughly `path + Attr` per entry — about 150 bytes, so this export
/// costs ~500 KB at 3601 entries and the cap allows ~30 MB. The number is a
/// ceiling on a server's resident set, not a performance tuning knob: the box
/// serving this in practice has under a gigabyte of RAM, and an agent that
/// OOMs while indexing is strictly worse than one that answers a little slower.
pub const DEFAULT_MAX_ENTRIES: usize = 200_000;

/// Where an index's contents come from.
///
/// An interface rather than a function so a faster platform enumeration can
/// arrive as another source instead of a rewrite. One candidate has already
/// been tried and retired ON PAPER: `FSCTL_ENUM_USN_DATA` (the MFT) was
/// planned back when the walk here measured ~104 us/entry against ~9 on
/// ext4 — but that gap turned out to be a per-entry re-stat throwing away
/// the attributes FindFirstFile had already returned, not enumeration cost.
/// With `DirEntry::metadata` the walk runs at ~15 us/entry (see the comment
/// in `WalkSource`), and since USN records carry no size or mtime, an MFT
/// source would still pay a stat per entry — same floor, plus admin rights,
/// an NTFS-only gate, and a volume-wide scan. If a second source ever lands,
/// its case will have to be a durable change CURSOR (the USN journal),
/// not enumeration speed.
pub trait TreeSource: Send + Sync {
    /// Enumerate `root`, skipping anything `exclude` hides, stopping once more
    /// than `cap` entries have been produced.
    fn enumerate(
        &self,
        root: &Path,
        exclude: &ExcludeSet,
        cap: usize,
    ) -> std::io::Result<Option<Vec<(RelPath, Attr)>>>;
}

/// The portable source: an ordinary recursive directory walk.
pub struct WalkSource;

impl TreeSource for WalkSource {
    fn enumerate(
        &self,
        root: &Path,
        exclude: &ExcludeSet,
        cap: usize,
    ) -> std::io::Result<Option<Vec<(RelPath, Attr)>>> {
        let mut out = Vec::new();
        let mut stack = vec![(root.to_path_buf(), RelPath(String::new()))];
        while let Some((dir, rel)) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                // A directory that vanished or refuses to open is skipped, not
                // fatal. The index is an optimisation; half of it plus a
                // `Readdir` fallback beats none of it plus an error.
                Err(_) => continue,
            };
            for item in entries.flatten() {
                let Ok(name) = item.file_name().into_string() else {
                    continue;
                };
                let child = rel.join(&name);
                if exclude.is_excluded(&child) {
                    continue;
                }
                // DirEntry::metadata, for both of its properties. It does not
                // traverse symlinks — a link is indexed as a link, so a link
                // out of the export can neither pull foreign paths in nor
                // loop on a cycle. And on Windows it is FREE: FindFirstFile
                // already returned the attributes with the name, where the
                // `symlink_metadata(item.path())` this replaces re-opened
                // every file to ask again. Measured over the same 16k-entry
                // NTFS tree, interleaved: 73.2 us/entry warm (164.8 cold)
                // for the re-stat against 15.4 us/entry warm (22.1 cold) for
                // the find data — 4.8x, and within ~1.7x of the same walk on
                // ext4. That number is also what retired the planned
                // FSCTL_ENUM_USN_DATA source: USN records carry no size or
                // mtime, so an MFT enumeration still pays a per-entry stat —
                // its floor is this walk's floor, plus admin rights, an
                // NTFS-only gate, and a volume-wide scan.
                // (bench_walk_stat_strategies re-measures this.)
                //
                // Files only, though. What FindFirstFile serves is NTFS's
                // "duplicated information" — a copy kept in the parent's
                // index that settles asynchronously — and for a directory
                // with freshly written children it can lag the authoritative
                // value by whole milliseconds (caught by the index-vs-disk
                // equivalence test: the same dir listed 510 us apart carried
                // two different mtimes). A lagged FILE converges through the
                // event feed and matches what the disk readdir path serves
                // anyway; a lagged DIRECTORY would sit in the token until
                // rebuild and make an idle rebuild look like a tree change.
                // Directories are rare enough to stat authoritatively: with
                // them re-stated (17% of the bench tree), the walk still
                // measures ~3x the old strategy in every interleaved round
                // (42-63 us/entry against 89-254 under identical load).
                let md = if item.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    #[cfg(windows)]
                    let md = std::fs::symlink_metadata(item.path());
                    #[cfg(not(windows))]
                    let md = item.metadata();
                    md
                } else {
                    item.metadata()
                };
                let Ok(md) = md else {
                    continue;
                };
                if md.is_dir() {
                    stack.push((item.path(), child.clone()));
                }
                out.push((child, attr_from_metadata(&md, 0)));
                if out.len() > cap {
                    return Ok(None);
                }
            }
        }
        Ok(Some(out))
    }
}

/// Per-entry contribution to the token.
///
/// Covers what a client checks freshness against — path, size, mtime, kind —
/// so any change a client would care about moves the digest. Version is
/// deliberately absent: it is per-process and resets when the agent restarts,
/// and a token that changed on restart would be exactly as useless as the
/// sequence number this exists to replace.
fn entry_digest(path: &RelPath, attr: &Attr) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.0.hash(&mut h);
    attr.size.hash(&mut h);
    attr.mtime.hash(&mut h);
    (attr.kind as u8).hash(&mut h);
    h.finish()
}

#[derive(Default)]
struct Indexed {
    entries: BTreeMap<RelPath, Attr>,
    /// Sum of every entry digest. Commutative, so an insert or a removal
    /// updates it in constant time and the order entries arrived in cannot
    /// change the result.
    sum: u64,
}

impl Indexed {
    fn insert(&mut self, path: RelPath, attr: Attr) {
        if let Some(old) = self.entries.insert(path.clone(), attr) {
            self.sum = self.sum.wrapping_sub(entry_digest(&path, &old));
        }
        self.sum = self.sum.wrapping_add(entry_digest(&path, &attr));
    }

    fn remove(&mut self, path: &RelPath) {
        if let Some(old) = self.entries.remove(path) {
            self.sum = self.sum.wrapping_sub(entry_digest(path, &old));
        }
    }

    /// The export's token. Folds the entry COUNT in as well as the digest sum,
    /// so a pair of changes whose digests happen to cancel still moves it.
    /// Never 0 — that value is reserved for "not indexed".
    fn token(&self) -> u64 {
        let mixed = self.sum.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(31)
            ^ (self.entries.len() as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        if mixed == 0 {
            1
        } else {
            mixed
        }
    }
}

/// One page of a subtree: the entries, whether more follow, and the token the
/// page belongs to.
pub type Page = (Vec<(RelPath, Attr)>, bool, u64);

/// State of one export's index.
enum State {
    /// Never asked for. Nothing has been walked.
    Cold,
    /// Walked and maintained.
    Live(Indexed),
    /// Walked, and the export was too big. Clients keep using `Readdir`; the
    /// walk is not retried, because the size that defeated it will not have
    /// changed by the next request.
    TooLarge,
}

pub struct ExportTree {
    state: Mutex<State>,
    cap: usize,
    source: Box<dyn TreeSource>,
}

impl ExportTree {
    pub fn new(cap: usize) -> Self {
        Self {
            state: Mutex::new(State::Cold),
            cap,
            source: Box::new(WalkSource),
        }
    }

    /// Build the index if it has not been built, and return the token.
    ///
    /// `mark` is the event sequence taken BEFORE the walk starts, and the
    /// caller replays anything above it afterwards. That is what closes the
    /// window: the watcher is already running before any client can ask for a
    /// tree, so a change landing mid-walk is either seen by the walk or
    /// present in the replay, and applying it twice is harmless because both
    /// paths are idempotent overwrites of one path's entry.
    pub fn ensure(&self, root: &Path, exclude: &ExcludeSet) -> u64 {
        let mut st = self.state.lock().unwrap();
        match &*st {
            State::Live(idx) => return idx.token(),
            State::TooLarge => return 0,
            State::Cold => {}
        }
        match self.source.enumerate(root, exclude, self.cap) {
            Ok(Some(entries)) => {
                let mut idx = Indexed::default();
                for (path, attr) in entries {
                    idx.insert(path, attr);
                }
                let token = idx.token();
                tracing::info!(entries = idx.entries.len(), token, "export indexed");
                *st = State::Live(idx);
                token
            }
            Ok(None) => {
                tracing::info!(cap = self.cap, "export too large to index; clients use readdir");
                *st = State::TooLarge;
                0
            }
            Err(e) => {
                tracing::warn!(error = %e, "indexing failed; clients use readdir");
                *st = State::TooLarge;
                0
            }
        }
    }

    /// The current token without building anything. 0 when not indexed.
    pub fn token(&self) -> u64 {
        match &*self.state.lock().unwrap() {
            State::Live(idx) => idx.token(),
            _ => 0,
        }
    }

    /// One page of the subtree under `path`, plus the token it belongs to.
    ///
    /// `None` when the export is not indexed. Pagination is by entry offset
    /// over a sorted map, which is stable as long as the tree does not change
    /// underneath — and the token returned with each page is what lets a
    /// client notice when it did.
    pub fn page(&self, path: &RelPath, offset: usize, limit: usize) -> Option<Page> {
        let st = self.state.lock().unwrap();
        let State::Live(idx) = &*st else {
            return None;
        };
        let prefix = if path.is_root() {
            String::new()
        } else {
            format!("{}/", path.0)
        };
        // Seek, don't scan. The map is a BTreeMap ordered by path, so the
        // subtree under `prefix` is one contiguous range — `range(prefix..)`
        // jumps straight to it instead of testing every entry in the export
        // against the prefix on every page. `skip(offset)` still walks within
        // the subtree; an offset cursor is what the v6 wire shape gives us,
        // and the token already forces a restart if the map changed between
        // pages, which is the only case an offset could lie in.
        let mut hit = idx
            .entries
            .range(RelPath(prefix.clone())..)
            .take_while(|(p, _)| prefix.is_empty() || p.0.starts_with(&prefix))
            .skip(offset);
        let mut page = Vec::with_capacity(limit.min(1024));
        for (p, a) in hit.by_ref().take(limit) {
            page.push((p.clone(), *a));
        }
        let more = hit.next().is_some();
        Some((page, more, idx.token()))
    }

    /// The complete direct-child listing of `dir`, name-ordered, plus the
    /// token it was built under. The ops layer snapshots this once and pages
    /// from the snapshot, so a listing is one build — not one subtree scan
    /// per page.
    ///
    /// `None` means "answer from disk instead" — either the export is not
    /// indexed, or the index does not know `dir` as a directory. The second
    /// half is deliberate: the disk path owns every error case (NotFound,
    /// not-a-directory, the case-insensitive spellings a real volume resolves
    /// and this byte-keyed map cannot), so the index only ever answers the
    /// one question it answers exactly.
    pub fn readdir_all(&self, dir: &RelPath) -> Option<(Vec<(String, Attr)>, u64)> {
        let st = self.state.lock().unwrap();
        let State::Live(idx) = &*st else {
            return None;
        };
        if !dir.is_root() {
            match idx.entries.get(dir) {
                Some(a) if a.kind == FileKind::Dir => {}
                _ => return None,
            }
        }
        let prefix = if dir.is_root() {
            String::new()
        } else {
            format!("{}/", dir.0)
        };
        // The map orders FULL paths byte-wise, so a child's descendants do
        // not follow it directly: siblings whose next byte is below '/'
        // (0x2F) — "c!x", "c.txt" — sort between "c" and "c/…", and the
        // subtree "c/…" sits wholly before "c0" (0x30). Walking linearly and
        // filtering out descendants therefore costs O(subtree) per listing —
        // the old shape — where re-seeking past a subtree the moment the
        // walk lands in one costs O(log n) per directory child instead.
        // Survivors come out in byte order of their names, the same order
        // the disk path produces by sorting.
        let mut out: Vec<(String, Attr)> = Vec::new();
        let mut from = std::ops::Bound::Included(RelPath(prefix.clone()));
        'walk: loop {
            let range = idx.entries.range((from.clone(), std::ops::Bound::Unbounded));
            for (p, a) in range {
                if !(prefix.is_empty() || p.0.starts_with(&prefix)) {
                    break 'walk; // left the subtree
                }
                let rest = &p.0[prefix.len()..];
                if rest.is_empty() {
                    continue; // the dir's own entry (root has none)
                }
                match rest.find('/') {
                    None => out.push((rest.to_string(), *a)),
                    Some(slash) => {
                        // Landed inside a child's subtree: hop to the first
                        // key past it. '0' is the byte after '/', so
                        // "<child>0" bounds exactly the "<child>/…" range —
                        // and a real sibling named "<child>0…" is ≥ the
                        // bound, so nothing legitimate is skipped.
                        let child = &rest[..slash];
                        from = std::ops::Bound::Included(RelPath(format!("{prefix}{child}0")));
                        continue 'walk;
                    }
                }
            }
            break;
        }
        Some((out, idx.token()))
    }

    /// Apply one observed change. Cheap enough to call per event.
    pub fn note_change(&self, root: &Path, path: &RelPath, removed: bool) {
        let mut st = self.state.lock().unwrap();
        let State::Live(idx) = &mut *st else {
            return;
        };
        if removed {
            remove_with_subtree(idx, path);
            return;
        }
        match std::fs::symlink_metadata(root.join(&path.0)) {
            Ok(md) => idx.insert(path.clone(), attr_from_metadata(&md, 0)),
            // Gone between the event and the stat: treat as removed rather
            // than leaving an entry describing a file that is not there.
            Err(_) => idx.remove(path),
        }
    }

    /// Apply a batch of observed changes in two phases: every stat runs
    /// BEFORE the lock, then one acquisition applies the lot. The per-event
    /// form stats under the state lock, so a debounce-sized batch was that
    /// many disk stats serialized against every readdir and attach on the
    /// export — and that many separate acquisitions besides.
    pub fn note_changes(&self, root: &Path, changes: &[(RelPath, bool)]) {
        if changes.is_empty() {
            return;
        }
        // Cold index: nothing to maintain, so skip the stats too. Checked
        // again at apply — an invalidate can land in between, and applying
        // onto an index it doomed would resurrect exactly the staleness the
        // invalidate was for.
        if !matches!(&*self.state.lock().unwrap(), State::Live(_)) {
            return;
        }
        enum Fate {
            /// The event said removed: the entry goes, subtree and all —
            /// the watcher does not necessarily report the children.
            Gone,
            /// Vanished between the event and the stat: the entry alone
            /// goes, same as `note_change`'s stat-failure arm.
            Vanished,
            Present(Attr),
        }
        let stats: Vec<(&RelPath, Fate)> = changes
            .iter()
            .map(|(path, removed)| {
                let fate = if *removed {
                    Fate::Gone
                } else {
                    match std::fs::symlink_metadata(root.join(&path.0)) {
                        Ok(md) => Fate::Present(attr_from_metadata(&md, 0)),
                        Err(_) => Fate::Vanished,
                    }
                };
                (path, fate)
            })
            .collect();
        let mut st = self.state.lock().unwrap();
        let State::Live(idx) = &mut *st else {
            return;
        };
        for (path, fate) in stats {
            match fate {
                Fate::Gone => remove_with_subtree(idx, path),
                Fate::Vanished => idx.remove(path),
                Fate::Present(attr) => idx.insert(path.clone(), attr),
            }
        }
    }

    /// Drop the index. The next request walks again — which at 33 ms is the
    /// whole of the recovery story for a watcher that overflowed.
    pub fn invalidate(&self) {
        let mut st = self.state.lock().unwrap();
        if matches!(&*st, State::Live(_)) {
            tracing::info!("export index dropped; will rebuild on next request");
            *st = State::Cold;
        }
    }
}

/// Remove an entry and everything beneath it. A removed directory takes its
/// subtree with it; the watcher does not necessarily report the children.
fn remove_with_subtree(idx: &mut Indexed, path: &RelPath) {
    idx.remove(path);
    let prefix = format!("{}/", path.0);
    let doomed: Vec<RelPath> = idx
        .entries
        .range(RelPath(prefix.clone())..)
        .take_while(|(p, _)| p.0.starts_with(&prefix))
        .map(|(p, _)| p.clone())
        .collect();
    for p in doomed {
        idx.remove(&p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr_of(size: u64) -> Attr {
        Attr {
            kind: alloyfs_proto::FileKind::File,
            size,
            mtime: std::time::UNIX_EPOCH,
            ctime: std::time::UNIX_EPOCH,
            mode: 0o644,
            version: 0,
        }
    }

    #[test]
    fn the_token_is_order_independent() {
        let mut a = Indexed::default();
        a.insert(RelPath("x".into()), attr_of(1));
        a.insert(RelPath("y".into()), attr_of(2));
        let mut b = Indexed::default();
        b.insert(RelPath("y".into()), attr_of(2));
        b.insert(RelPath("x".into()), attr_of(1));
        assert_eq!(a.token(), b.token(), "insertion order must not matter");
    }

    /// The property the whole design rests on: a change any client could
    /// notice has to move the token, or a stale cache is served as current.
    #[test]
    fn every_kind_of_change_moves_the_token() {
        let mut idx = Indexed::default();
        idx.insert(RelPath("x".into()), attr_of(1));
        let base = idx.token();

        idx.insert(RelPath("x".into()), attr_of(2)); // size changed
        assert_ne!(base, idx.token(), "a resized file must move it");

        idx.insert(RelPath("x".into()), attr_of(1)); // back again
        assert_eq!(base, idx.token(), "and moving back must restore it");

        idx.insert(RelPath("y".into()), attr_of(1)); // added
        let with_y = idx.token();
        assert_ne!(base, with_y, "an added file must move it");

        idx.remove(&RelPath("y".into())); // removed
        assert_eq!(base, idx.token(), "removing it must restore it");
    }

    /// Two files whose digests would cancel must not produce the empty tree's
    /// token. This is why the count is folded in as well as the sum.
    #[test]
    fn a_populated_tree_never_looks_empty() {
        let empty = Indexed::default().token();
        let mut idx = Indexed::default();
        idx.insert(RelPath("a".into()), attr_of(1));
        idx.insert(RelPath("b".into()), attr_of(1));
        assert_ne!(empty, idx.token());
    }

    fn dir_attr() -> Attr {
        Attr {
            kind: FileKind::Dir,
            ..attr_of(0)
        }
    }

    /// `readdir_all` must serve exactly what a disk readdir of the same map
    /// would: direct children only, name order — and refuse (None) whenever
    /// the answer would need disk semantics it cannot supply.
    #[test]
    fn readdir_all_lists_direct_children_in_name_order() {
        let mut idx = Indexed::default();
        // "a.txt" sorts between "a" and "a/deep" in full-path byte order —
        // the interleaving the subtree hop has to survive: after emitting
        // dir "a" the walk must still visit "a.txt" (it sorts BEFORE the
        // "a/…" subtree), and hopping past "a/deep" must land on "b".
        idx.insert(RelPath("d".into()), dir_attr());
        idx.insert(RelPath("d/a".into()), dir_attr());
        idx.insert(RelPath("d/a/deep".into()), attr_of(9));
        idx.insert(RelPath("d/a.txt".into()), attr_of(1));
        idx.insert(RelPath("d/b".into()), attr_of(2));
        idx.insert(RelPath("plain".into()), attr_of(3));

        let tree = ExportTree::new(10);
        *tree.state.lock().unwrap() = State::Live(idx);

        let (kids, token) = tree.readdir_all(&RelPath("d".into())).unwrap();
        let names: Vec<&str> = kids.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "a.txt", "b"], "direct children, name-sorted");
        assert_eq!(
            token,
            tree.token(),
            "the listing carries the token it was built under"
        );

        // Root listing: the subtree stays out of it.
        let (root, _) = tree.readdir_all(&RelPath(String::new())).unwrap();
        let names: Vec<&str> = root.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["d", "plain"]);

        // Anything the index cannot answer exactly goes to disk: a path it
        // does not know, and a path it knows as a file.
        assert!(tree.readdir_all(&RelPath("missing".into())).is_none());
        assert!(tree.readdir_all(&RelPath("plain".into())).is_none());
    }

    #[test]
    fn readdir_all_subtree_hop_skips_nothing_legitimate() {
        // The hop after landing in "c/…" seeks to "c0" — every byte-order
        // neighbour a name can produce is planted around it: "c!x" and
        // "c.txt" sort BETWEEN "c" and "c/…" (bytes below 0x2F), and "c0"
        // itself is a legal sibling name sitting exactly on the seek bound.
        let mut idx = Indexed::default();
        idx.insert(RelPath("c".into()), dir_attr());
        idx.insert(RelPath("c/w".into()), attr_of(1));
        idx.insert(RelPath("c/w2".into()), attr_of(2));
        idx.insert(RelPath("c!x".into()), attr_of(3));
        idx.insert(RelPath("c.txt".into()), attr_of(4));
        idx.insert(RelPath("c0".into()), attr_of(5));
        idx.insert(RelPath("czz".into()), attr_of(6));

        let tree = ExportTree::new(10);
        *tree.state.lock().unwrap() = State::Live(idx);

        let (root, _) = tree.readdir_all(&RelPath(String::new())).unwrap();
        let names: Vec<&str> = root.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["c", "c!x", "c.txt", "c0", "czz"],
            "between-siblings and the on-bound name must all survive the hop"
        );
    }

    #[test]
    fn removing_a_directory_removes_its_subtree() {
        let mut idx = Indexed::default();
        idx.insert(RelPath("d".into()), attr_of(0));
        idx.insert(RelPath("d/one".into()), attr_of(1));
        idx.insert(RelPath("d/two".into()), attr_of(2));
        idx.insert(RelPath("keep".into()), attr_of(3));

        let tree = ExportTree::new(10);
        *tree.state.lock().unwrap() = State::Live(idx);
        tree.note_change(Path::new("/nonexistent"), &RelPath("d".into()), true);

        let (page, _, _) = tree.page(&RelPath(String::new()), 0, 100).unwrap();
        let names: Vec<&str> = page.iter().map(|(p, _)| p.0.as_str()).collect();
        assert_eq!(names, ["keep"], "the subtree went with its directory");
    }

    #[test]
    fn an_export_past_the_cap_is_not_indexed() {
        let dir = std::env::temp_dir().join("alloyfs-tree-cap-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            std::fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }
        let tree = ExportTree::new(2);
        let token = tree.ensure(&dir, &ExcludeSet::compile(&[], false).unwrap());
        assert_eq!(token, 0, "over the cap means unindexed, which reads as 0");
        assert!(tree.page(&RelPath(String::new()), 0, 10).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Measure `WalkSource` (find-data attrs) against the strategy it
    /// replaced (a `symlink_metadata` re-stat per entry) over the same tree.
    /// The numbers in `WalkSource`'s comment came from here; if they ever
    /// need re-deriving, this is the tool. Ignored: it is a measurement, not
    /// an assertion — run with
    /// `ALLOYFS_BENCH_TREE=<dir> cargo test --release -p alloyfs-agent bench_walk -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_walk_stat_strategies() {
        let Ok(root) = std::env::var("ALLOYFS_BENCH_TREE") else {
            eprintln!("set ALLOYFS_BENCH_TREE to a directory");
            return;
        };
        let root = std::path::PathBuf::from(root);
        let ex = ExcludeSet::compile(&[], false).unwrap();

        // The retired strategy, kept inline as the baseline.
        fn walk_restat(root: &Path, exclude: &ExcludeSet) -> Vec<(RelPath, Attr)> {
            let mut out = Vec::new();
            let mut stack = vec![(root.to_path_buf(), RelPath(String::new()))];
            while let Some((dir, rel)) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for item in entries.flatten() {
                    let Ok(name) = item.file_name().into_string() else {
                        continue;
                    };
                    let child = rel.join(&name);
                    if exclude.is_excluded(&child) {
                        continue;
                    }
                    let Ok(md) = std::fs::symlink_metadata(item.path()) else {
                        continue;
                    };
                    if md.is_dir() {
                        stack.push((item.path(), child.clone()));
                    }
                    out.push((child, attr_from_metadata(&md, 0)));
                }
            }
            out
        }

        // Interleaved A/B/A/B so cache drift hits both strategies equally.
        for round in 0..2 {
            let t = std::time::Instant::now();
            let a = walk_restat(&root, &ex);
            let t_a = t.elapsed();
            let t = std::time::Instant::now();
            let b = WalkSource.enumerate(&root, &ex, usize::MAX).unwrap().unwrap();
            let t_b = t.elapsed();
            eprintln!(
                "round {round}: re-stat {} entries in {:?} ({:.1} us/entry) | WalkSource {} entries in {:?} ({:.1} us/entry)",
                a.len(),
                t_a,
                t_a.as_micros() as f64 / a.len() as f64,
                b.len(),
                t_b,
                t_b.as_micros() as f64 / b.len() as f64,
            );
            assert_eq!(a.len(), b.len(), "both strategies must see the same tree");
        }
    }

    #[test]
    fn a_walk_indexes_what_is_there_and_honours_excludes() {
        let dir = std::env::temp_dir().join("alloyfs-tree-walk-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("keep.txt"), b"x").unwrap();
        std::fs::write(dir.join("sub/deep.txt"), b"yy").unwrap();
        std::fs::write(dir.join("skip.log"), b"zzz").unwrap();

        let tree = ExportTree::new(100);
        let ex = ExcludeSet::compile(&["*.log".to_string()], false).unwrap();
        let token = tree.ensure(&dir, &ex);
        assert_ne!(token, 0);

        let (page, more, _) = tree.page(&RelPath(String::new()), 0, 100).unwrap();
        assert!(!more);
        let mut names: Vec<&str> = page.iter().map(|(p, _)| p.0.as_str()).collect();
        names.sort();
        assert_eq!(names, ["keep.txt", "sub", "sub/deep.txt"]);

        // A subtree query is scoped to its prefix.
        let (sub, _, _) = tree.page(&RelPath("sub".into()), 0, 100).unwrap();
        let sub_names: Vec<&str> = sub.iter().map(|(p, _)| p.0.as_str()).collect();
        assert_eq!(sub_names, ["sub/deep.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
