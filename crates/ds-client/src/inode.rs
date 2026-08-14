use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use ds_proto::RelPath;

pub const ROOT_INO: u64 = 1;

/// Bidirectional inode-number ↔ path map.
///
/// The wire protocol speaks paths; the FUSE kernel interface speaks inode
/// numbers. This table is where the two meet: an inode is allocated the first
/// time a path is seen and stays stable for the lifetime of the mount (rename
/// handling retargets entries — see `rename` below).
pub struct InodeTable {
    by_ino: DashMap<u64, RelPath>,
    by_path: DashMap<RelPath, u64>,
    next: AtomicU64,
}

impl InodeTable {
    pub fn new() -> Self {
        let t = Self {
            by_ino: DashMap::new(),
            by_path: DashMap::new(),
            next: AtomicU64::new(ROOT_INO + 1),
        };
        t.by_ino.insert(ROOT_INO, RelPath(String::new()));
        t.by_path.insert(RelPath(String::new()), ROOT_INO);
        t
    }

    pub fn get_or_alloc(&self, path: RelPath) -> u64 {
        if let Some(ino) = self.by_path.get(&path) {
            return *ino;
        }
        let ino = self.next.fetch_add(1, Ordering::Relaxed);
        // Two racers may both allocate; the entry API makes one win and the
        // loser's number is simply never used.
        let winner = *self.by_path.entry(path.clone()).or_insert(ino);
        if winner == ino {
            self.by_ino.insert(ino, path);
        }
        winner
    }

    pub fn path_of(&self, ino: u64) -> Option<RelPath> {
        self.by_ino.get(&ino).map(|p| p.clone())
    }

    pub fn ino_of(&self, path: &RelPath) -> Option<u64> {
        self.by_path.get(path).map(|i| *i)
    }

    /// Retarget an inode after a rename: same number, new path. Also fixes
    /// descendants when a directory moves.
    pub fn rename(&self, from: &RelPath, to: &RelPath) {
        let prefix = format!("{}/", from.0);
        // Collect first: mutating while iterating a DashMap can deadlock.
        let affected: Vec<(u64, RelPath)> = self
            .by_ino
            .iter()
            .filter_map(|e| {
                let (ino, path) = (*e.key(), e.value().clone());
                if path == *from {
                    Some((ino, to.clone()))
                } else if path.0.starts_with(&prefix) {
                    Some((ino, RelPath(format!("{}{}", to.0, &path.0[from.0.len()..]))))
                } else {
                    None
                }
            })
            .collect();
        for (ino, new_path) in affected {
            if let Some((_, old_path)) = self.by_ino.remove(&ino) {
                self.by_path.remove(&old_path);
            }
            self.by_path.insert(new_path.clone(), ino);
            self.by_ino.insert(ino, new_path);
        }
    }

    pub fn forget(&self, ino: u64) {
        if ino == ROOT_INO {
            return;
        }
        if let Some((_, path)) = self.by_ino.remove(&ino) {
            self.by_path.remove(&path);
        }
    }
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}
