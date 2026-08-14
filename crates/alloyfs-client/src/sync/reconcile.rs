//! Three-way reconciliation: baseline manifest vs current-local vs
//! current-remote → a list of actions. `plan` is a PURE function — every row
//! of its decision table is unit-tested below without any I/O.
//!
//! Change detection is size+mtime, never version inequality: server versions
//! reset on agent restart AND get bumped by the watcher echo of our own
//! pushes, so "version differs" is meaningless. Version EQUALITY (both
//! nonzero) is kept as a cheap positive freshness short-circuit.

use std::collections::BTreeMap;
use std::path::Path;

use alloyfs_common::ExcludeSet;
use alloyfs_proto::{FileKind, Request, Response};
use alloyfs_transport::MuxConnection;

use super::manifest::{mtime_ns_of, EntryKind, SyncManifest};

/// One side's view of one path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stat {
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ns: u128,
    /// Server version (remote snapshots only; 0 locally / when unknown).
    pub version: u64,
}

pub type Snapshot = BTreeMap<String, Stat>;

/// What reconciliation decided for one path.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Download remote → local (create or overwrite).
    Pull(String),
    /// Upload local → server (create or overwrite).
    Push(String),
    /// Remote won a delete: remove the local file/dir.
    DeleteLocal(String, EntryKind),
    /// Local won a delete: remove the server file/dir.
    DeleteRemote(String, EntryKind),
    /// Both sides already identical: record the baseline, transfer nothing.
    Adopt(String, Stat),
    /// Both sides changed and differ: apply the conflict policy (loser is
    /// preserved as a .sync-conflict copy by the engine).
    Conflict { path: String, local: Stat, remote: Stat },
    /// Gone on both sides: drop the baseline entry.
    Forget(String),
}

/// The decision table. `local`/`remote` are full snapshots; `manifest` is the
/// baseline. Returns actions in path order (parents before children); the
/// engine executes deletes children-first by reversing within delete groups.
pub fn plan(manifest: &SyncManifest, local: &Snapshot, remote: &Snapshot) -> Vec<Action> {
    let mut paths: BTreeMap<&str, ()> = BTreeMap::new();
    for k in manifest.entries.keys() {
        paths.insert(k, ());
    }
    for k in local.keys() {
        paths.insert(k, ());
    }
    for k in remote.keys() {
        paths.insert(k, ());
    }

    let mut actions = Vec::new();
    for (path, ()) in paths {
        let base = manifest.entries.get(path);
        let l = local.get(path);
        let r = remote.get(path);

        let l_changed = match (base, l) {
            (Some(b), Some(l)) => !same_stat(b.kind, b.size, b.mtime_ns, l),
            (Some(_), None) => true, // locally deleted
            (None, Some(_)) => true, // locally new
            (None, None) => false,
        };
        let r_changed = match (base, r) {
            (Some(b), Some(r)) => {
                // Version equality (both nonzero) = provably unchanged.
                if b.remote_version != 0 && b.remote_version == r.version {
                    false
                } else {
                    !same_stat(b.kind, b.size, b.mtime_ns, r)
                }
            }
            (Some(_), None) => true,
            (None, Some(_)) => true,
            (None, None) => false,
        };

        match (l, r) {
            (Some(l), Some(r)) => {
                // Dirs carry no content: presence on both sides = in sync.
                if l.kind == EntryKind::Dir && r.kind == EntryKind::Dir {
                    if base.is_none() {
                        actions.push(Action::Adopt(path.to_string(), *r));
                    }
                    continue;
                }
                match (l_changed, r_changed) {
                    (false, false) => {}
                    (true, false) => actions.push(Action::Push(path.to_string())),
                    (false, true) => actions.push(Action::Pull(path.to_string())),
                    (true, true) => {
                        if l.kind == r.kind && l.size == r.size && l.mtime_ns == r.mtime_ns {
                            // Same content by the quick check (also absorbs
                            // the crash-between-apply-and-flush window).
                            actions.push(Action::Adopt(path.to_string(), *r));
                        } else {
                            actions.push(Action::Conflict {
                                path: path.to_string(),
                                local: *l,
                                remote: *r,
                            });
                        }
                    }
                }
            }
            (Some(l), None) => {
                if base.is_none() {
                    actions.push(Action::Push(path.to_string())); // local-new
                } else if l_changed {
                    // Delete-vs-edit: the edit wins — re-create remotely.
                    actions.push(Action::Push(path.to_string()));
                } else {
                    actions.push(Action::DeleteLocal(path.to_string(), l.kind));
                }
            }
            (None, Some(r)) => {
                if base.is_none() {
                    actions.push(Action::Pull(path.to_string())); // remote-new
                } else if r_changed {
                    // Delete-vs-edit: the edit wins — resurrect locally.
                    actions.push(Action::Pull(path.to_string()));
                } else {
                    actions.push(Action::DeleteRemote(path.to_string(), r.kind));
                }
            }
            (None, None) => actions.push(Action::Forget(path.to_string())),
        }
    }
    actions
}

fn same_stat(kind: EntryKind, size: u64, mtime_ns: u128, s: &Stat) -> bool {
    s.kind == kind && (kind == EntryKind::Dir || (s.size == size && s.mtime_ns == mtime_ns))
}

/// Walk the local tree (blocking — call via spawn_blocking).
pub fn snapshot_local(root: &Path, exclude: &ExcludeSet) -> std::io::Result<Snapshot> {
    let mut snap = Snapshot::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for item in std::fs::read_dir(&dir)? {
            let item = item?;
            let Some(rel) = alloyfs_common::coalesce::rel_of(root, &item.path()) else {
                continue;
            };
            if exclude.is_excluded(&rel) || super::is_internal(&rel.0) {
                continue;
            }
            let md = match item.metadata() {
                Ok(md) => md,
                Err(_) => continue, // vanished mid-walk
            };
            if md.is_symlink() {
                continue; // not syncable (no wire representation)
            }
            if md.is_dir() {
                snap.insert(
                    rel.0.clone(),
                    Stat {
                        kind: EntryKind::Dir,
                        size: 0,
                        mtime_ns: 0,
                        version: 0,
                    },
                );
                stack.push(item.path());
            } else {
                snap.insert(
                    rel.0,
                    Stat {
                        kind: EntryKind::File,
                        size: md.len(),
                        mtime_ns: md.modified().map(mtime_ns_of).unwrap_or(0),
                        version: 0,
                    },
                );
            }
        }
    }
    Ok(snap)
}

/// Walk the remote tree via paginated Readdir (versions come along free).
pub async fn snapshot_remote(conn: &MuxConnection, exclude: &ExcludeSet) -> Result<Snapshot, crate::FsError> {
    let mut snap = Snapshot::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        let mut cursor = 0u64;
        loop {
            let resp = conn
                .request(Request::Readdir {
                    path: alloyfs_proto::RelPath(dir.clone()),
                    cursor,
                })
                .await??;
            let (entries, next) = match resp {
                Response::Dir { entries, next_cursor } => (entries, next_cursor),
                _ => return Err(alloyfs_proto::ErrorCode::Io.into()),
            };
            for e in entries {
                let path = if dir.is_empty() {
                    e.name.clone()
                } else {
                    format!("{dir}/{}", e.name)
                };
                if exclude.is_excluded(&alloyfs_proto::RelPath(path.clone())) || super::is_internal(&path) {
                    continue;
                }
                match e.attr.kind {
                    FileKind::Dir => {
                        snap.insert(
                            path.clone(),
                            Stat {
                                kind: EntryKind::Dir,
                                size: 0,
                                mtime_ns: 0,
                                version: e.attr.version,
                            },
                        );
                        dirs.push(path);
                    }
                    FileKind::File => {
                        snap.insert(
                            path,
                            Stat {
                                kind: EntryKind::File,
                                size: e.attr.size,
                                mtime_ns: mtime_ns_of(e.attr.mtime),
                                version: e.attr.version,
                            },
                        );
                    }
                    FileKind::Symlink => {} // not syncable
                }
            }
            match next {
                Some(c) => cursor = c,
                None => break,
            }
        }
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::manifest::SyncEntry;

    fn file(size: u64, mtime_ns: u128, version: u64) -> Stat {
        Stat {
            kind: EntryKind::File,
            size,
            mtime_ns,
            version,
        }
    }

    fn base(size: u64, mtime_ns: u128, version: u64) -> SyncEntry {
        SyncEntry {
            kind: EntryKind::File,
            remote_version: version,
            size,
            mtime_ns,
        }
    }

    fn manifest(entries: &[(&str, SyncEntry)]) -> SyncManifest {
        let mut m = SyncManifest::default();
        for (k, v) in entries {
            m.entries.insert(k.to_string(), v.clone());
        }
        m
    }

    fn snap(entries: &[(&str, Stat)]) -> Snapshot {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn decision_table() {
        let m = manifest(&[
            ("clean.txt", base(3, 100, 5)),
            ("local-edit.txt", base(3, 100, 5)),
            ("remote-edit.txt", base(3, 100, 5)),
            ("both-edit.txt", base(3, 100, 5)),
            ("local-del.txt", base(3, 100, 5)),
            ("remote-del.txt", base(3, 100, 5)),
            ("del-vs-edit-r.txt", base(3, 100, 5)),
            ("del-vs-edit-l.txt", base(3, 100, 5)),
            ("both-del.txt", base(3, 100, 5)),
        ]);
        let local = snap(&[
            ("clean.txt", file(3, 100, 0)),
            ("local-edit.txt", file(4, 200, 0)),
            ("remote-edit.txt", file(3, 100, 0)),
            ("both-edit.txt", file(4, 200, 0)),
            ("remote-del.txt", file(3, 100, 0)),
            ("del-vs-edit-l.txt", file(9, 900, 0)),
            ("local-new.txt", file(1, 50, 0)),
            ("both-new-same.txt", file(2, 60, 0)),
            ("both-new-diff.txt", file(2, 60, 0)),
        ]);
        let remote = snap(&[
            ("clean.txt", file(3, 100, 5)),
            ("local-edit.txt", file(3, 100, 5)),
            ("remote-edit.txt", file(4, 300, 6)),
            ("both-edit.txt", file(5, 300, 6)),
            ("local-del.txt", file(3, 100, 5)),
            ("del-vs-edit-r.txt", file(9, 900, 7)),
            ("remote-new.txt", file(1, 70, 2)),
            ("both-new-same.txt", file(2, 60, 3)),
            ("both-new-diff.txt", file(3, 61, 3)),
        ]);

        let actions = plan(&m, &local, &remote);
        let find = |p: &str| {
            actions
                .iter()
                .find(|a| {
                    matches!(a,
                    Action::Pull(x) | Action::Push(x) | Action::Forget(x)
                    | Action::DeleteLocal(x, _) | Action::DeleteRemote(x, _)
                    | Action::Adopt(x, _) | Action::Conflict { path: x, .. } if x == p)
                })
                .cloned()
        };

        assert_eq!(find("clean.txt"), None, "row 1: untouched");
        assert_eq!(
            find("local-edit.txt"),
            Some(Action::Push("local-edit.txt".into()))
        );
        assert_eq!(
            find("remote-edit.txt"),
            Some(Action::Pull("remote-edit.txt".into()))
        );
        assert!(
            matches!(find("both-edit.txt"), Some(Action::Conflict { .. })),
            "row 4"
        );
        assert_eq!(
            find("local-new.txt"),
            Some(Action::Push("local-new.txt".into())),
            "row 5"
        );
        assert_eq!(
            find("remote-new.txt"),
            Some(Action::Pull("remote-new.txt".into())),
            "row 6"
        );
        assert!(
            matches!(find("both-new-same.txt"), Some(Action::Adopt(..))),
            "row 7a"
        );
        assert!(
            matches!(find("both-new-diff.txt"), Some(Action::Conflict { .. })),
            "row 7b"
        );
        assert_eq!(
            find("local-del.txt"),
            Some(Action::DeleteRemote("local-del.txt".into(), EntryKind::File)),
            "row 8"
        );
        assert_eq!(
            find("del-vs-edit-r.txt"),
            Some(Action::Pull("del-vs-edit-r.txt".into())),
            "row 9: edit beats local delete"
        );
        assert_eq!(
            find("remote-del.txt"),
            Some(Action::DeleteLocal("remote-del.txt".into(), EntryKind::File)),
            "row 10"
        );
        assert_eq!(
            find("del-vs-edit-l.txt"),
            Some(Action::Push("del-vs-edit-l.txt".into())),
            "row 11: edit beats remote delete"
        );
        assert_eq!(
            find("both-del.txt"),
            Some(Action::Forget("both-del.txt".into())),
            "row 12"
        );
    }

    #[test]
    fn version_equality_short_circuits_mtime_noise() {
        // Remote mtime drifted (e.g. server touch) but version matches the
        // baseline: provably our own last-synced content — no pull.
        let m = manifest(&[("f.txt", base(3, 100, 5))]);
        let local = snap(&[("f.txt", file(3, 100, 0))]);
        let remote = snap(&[("f.txt", file(3, 999, 5))]);
        assert!(plan(&m, &local, &remote).is_empty());
    }

    #[test]
    fn version_inequality_alone_is_not_a_change() {
        // Version bumped (watcher echo of our own push / server restart) but
        // size+mtime match the baseline: no transfer.
        let m = manifest(&[("f.txt", base(3, 100, 5))]);
        let local = snap(&[("f.txt", file(3, 100, 0))]);
        let remote = snap(&[("f.txt", file(3, 100, 11))]);
        assert!(plan(&m, &local, &remote).is_empty());
    }
}
