//! The metadata snapshot: every directory listing the walker's tree pass saw,
//! persisted beside the auto-cache manifest so a remount can answer lookup,
//! getattr and readdir locally — the blobs' warm cold-start, applied to
//! metadata. Without this a token-matched remount skipped the walk but the
//! first navigation still paid one round trip per directory.
//!
//! The trust model is the tree token, exactly as for the blobs. A snapshot is
//! tagged with the token its listings were read at, and it is only ever
//! served when the server reports that token UNCHANGED at mount; matching
//! proves the export is still what the snapshot describes, so the listings
//! need no TTL — events keep them honest from there, the same way they keep
//! a skipped-walk blob cache honest.
//!
//! The completeness discipline carries over too: the snapshot is written only
//! by a walk that completed, and any mount that decides to walk deletes it
//! first. A proven-looking snapshot beside unfinished (or superseded) work is
//! worse than none — its listings answer hard NotFound for names they lack,
//! so serving a stale one is an error, not staleness. Unlike the manifest
//! there is no per-entry bookkeeping to protect: the file is one atomic
//! temp+rename write, wholly right or wholly absent.

use std::collections::BTreeMap;
use std::path::PathBuf;

use alloyfs_proto::{Attr, FileKind, RelPath};
use serde::{Deserialize, Serialize};

/// Bump on any incompatible change. Unlike the manifest's additive format
/// history, an unrecognised snapshot is simply deleted: it is a pure
/// optimisation, and the walk that follows rewrites it in the new shape.
const META_FORMAT: u32 = 1;

/// The whole snapshot: directory path → complete child list, tagged with the
/// tree token the listings were read at.
///
/// Keyed by PATH, never by ino — ino numbering is per-process (the
/// InodeTable allocates on first sight), so a number from the mount that
/// wrote this means nothing to the mount that reads it.
///
/// Listings are the SERVER's view, unfiltered: exclude patterns are client
/// configuration and can differ between the mount that wrote the snapshot
/// and the mount reading it, so overlay routing is applied at serve time,
/// where wire listings get it too.
#[derive(Serialize, Deserialize)]
pub(crate) struct MetaSnapshot {
    format: u32,
    pub tree_token: u64,
    pub dirs: BTreeMap<String, Vec<(String, Attr)>>,
}

/// Group a completed tree pass into per-directory listings.
///
/// Every directory gets a bucket even when childless — the root included —
/// because absence has to mean "not snapshotted", never "no children": a
/// warm listing answers a hard NotFound for names it lacks, and an empty
/// directory's listing is exactly as complete as a full one's.
pub(crate) fn snapshot_from_tree(entries: &[(RelPath, Attr)], token: u64) -> MetaSnapshot {
    let mut dirs: BTreeMap<String, Vec<(String, Attr)>> = BTreeMap::new();
    dirs.insert(String::new(), Vec::new());
    for (path, attr) in entries {
        if attr.kind == FileKind::Dir {
            dirs.entry(path.0.clone()).or_default();
        }
        if let Some((parent, name)) = path.split() {
            dirs.entry(parent.0).or_default().push((name.to_string(), *attr));
        }
    }
    MetaSnapshot {
        format: META_FORMAT,
        tree_token: token,
        dirs,
    }
}

/// The snapshot file: load-if-proven, atomic save, delete.
pub(crate) struct MetaCache {
    path: PathBuf,
}

impl MetaCache {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The snapshot, if one exists AND was taken at exactly `token`.
    ///
    /// Anything else — no file, unparseable, an unknown format, a different
    /// token — deletes the file and answers nothing. Deleting on mismatch is
    /// not tidiness: the file claims token-proven knowledge of an export
    /// state the export has left, and the one thing worse than a cold mount
    /// is a later reader finding a proven-looking snapshot it has no way to
    /// date.
    pub fn load_for(&self, token: u64) -> Option<MetaSnapshot> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        match serde_json::from_str::<MetaSnapshot>(&text) {
            Ok(s) if s.format == META_FORMAT && s.tree_token == token && token != 0 => Some(s),
            _ => {
                self.discard();
                None
            }
        }
    }

    /// Atomic write — temp file + rename, the manifest's own pattern — so a
    /// crash mid-write leaves the previous snapshot (or nothing), never half
    /// a file posing as a complete one.
    pub fn save(&self, snap: &MetaSnapshot) {
        let json = match serde_json::to_string(snap) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "metadata snapshot serialize failed");
                return;
            }
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.part");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// Delete the snapshot. Errors are ignored: a file that cannot be
    /// removed does not become more trustworthy by trying harder, and
    /// `load_for` re-proves the token on every read anyway.
    pub fn discard(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn attr(kind: FileKind, size: u64) -> Attr {
        Attr {
            kind,
            size,
            mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(size + 1),
            ctime: SystemTime::UNIX_EPOCH,
            mode: 0o644,
            version: size,
        }
    }

    fn tree() -> Vec<(RelPath, Attr)> {
        vec![
            (RelPath("a".into()), attr(FileKind::Dir, 0)),
            (RelPath("a/one.txt".into()), attr(FileKind::File, 3)),
            (RelPath("a/empty".into()), attr(FileKind::Dir, 0)),
            (RelPath("top.txt".into()), attr(FileKind::File, 7)),
        ]
    }

    #[test]
    fn the_tree_groups_into_listings_and_empty_dirs_get_buckets() {
        let snap = snapshot_from_tree(&tree(), 42);
        assert_eq!(snap.tree_token, 42);
        let root: Vec<&str> = snap.dirs[""].iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(root, ["a", "top.txt"]);
        let a: Vec<&str> = snap.dirs["a"].iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(a, ["one.txt", "empty"]);
        // The childless directory HAS a listing. Absence means "not
        // snapshotted"; an empty Vec means "no children, provably".
        assert!(snap.dirs["a/empty"].is_empty());
        assert_eq!(snap.dirs.len(), 3, "root, a, a/empty — files get no bucket");
    }

    #[test]
    fn a_snapshot_roundtrips_and_a_moved_token_discards_it() {
        let dir = std::env::temp_dir().join(format!("ds-meta-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("t.metadata.json");
        let mc = MetaCache::new(file.clone());
        mc.save(&snapshot_from_tree(&tree(), 42));

        let loaded = mc.load_for(42).expect("the matching token loads");
        assert_eq!(loaded.tree_token, 42);
        assert_eq!(loaded.dirs, snapshot_from_tree(&tree(), 42).dirs);

        // A moved token does not merely fail to load — it deletes the file,
        // so nothing later can mistake it for current.
        assert!(mc.load_for(43).is_none());
        assert!(!file.exists(), "a mismatched snapshot must be deleted");
        assert!(mc.load_for(42).is_none(), "and it stays gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_format_is_deleted_not_trusted() {
        let dir = std::env::temp_dir().join(format!("ds-meta-fmt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.metadata.json");
        std::fs::write(&file, r#"{"format":999,"tree_token":42,"dirs":{}}"#).unwrap();
        let mc = MetaCache::new(file.clone());
        assert!(mc.load_for(42).is_none());
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
