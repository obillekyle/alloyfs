//! The sync baseline: what (size, mtime, version) each path had the last
//! time both sides agreed. This is the third leg of the three-way merge —
//! without it, "local differs from remote" is ambiguous (who changed?);
//! with it, each side is compared against the last agreed state.
//!
//! Persistence follows the autocache manifest pattern: JSON, staged to a
//! .part file and atomically renamed, flushed after batches and on shutdown.
//! Losing it is safe — reconciliation degrades to the both-new rules (equal
//! files adopted by size+mtime, genuine divergence becomes a conflict copy).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEntry {
    pub kind: EntryKind,
    /// Server version at last sync. POSITIVE signal only (equality with a
    /// current nonzero version proves freshness); inequality proves nothing —
    /// the server bumps versions even for our own pushes' watcher echoes,
    /// and versions reset when the agent restarts. Size+mtime are primary.
    pub remote_version: u64,
    pub size: u64,
    pub mtime_ns: u128,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncManifest {
    format: u32,
    /// Highest remote event seq applied — resubscription point on restart.
    pub last_seq: u64,
    pub entries: BTreeMap<String, SyncEntry>,
}

impl SyncManifest {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "sync manifest unreadable; starting fresh");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Atomic persist (.part + rename): a crash never leaves a torn manifest.
    pub fn flush(&self, path: &Path) {
        let Ok(json) = serde_json::to_vec_pretty(self) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let part: PathBuf = path.with_extension("json.part");
        if std::fs::write(&part, json).is_ok() {
            let _ = std::fs::rename(&part, path);
        }
    }

    /// Move every entry under `from` (inclusive) to the same suffix under
    /// `to` — directory renames move whole subtrees.
    pub fn rename_prefix(&mut self, from: &str, to: &str) {
        let mut moved = Vec::new();
        let dir_prefix = format!("{from}/");
        self.entries.retain(|path, entry| {
            if path == from {
                moved.push((to.to_string(), entry.clone()));
                false
            } else if let Some(suffix) = path.strip_prefix(&dir_prefix) {
                moved.push((format!("{to}/{suffix}"), entry.clone()));
                false
            } else {
                true
            }
        });
        for (path, entry) in moved {
            self.entries.insert(path, entry);
        }
    }
}

/// SystemTime → nanoseconds since epoch, the manifest's mtime unit.
pub fn mtime_ns_of(t: SystemTime) -> u128 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: EntryKind) -> SyncEntry {
        SyncEntry {
            kind,
            remote_version: 1,
            size: 3,
            mtime_ns: 42,
        }
    }

    #[test]
    fn roundtrip_and_rename_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("m.json");
        let mut m = SyncManifest::default();
        m.last_seq = 9;
        m.entries.insert("a/b.txt".into(), entry(EntryKind::File));
        m.entries.insert("a".into(), entry(EntryKind::Dir));
        m.entries.insert("ax.txt".into(), entry(EntryKind::File));
        m.flush(&path);

        let loaded = SyncManifest::load(&path);
        assert_eq!(loaded.last_seq, 9);
        assert_eq!(loaded.entries.len(), 3);

        let mut m = loaded;
        m.rename_prefix("a", "z");
        assert!(m.entries.contains_key("z"), "dir entry moves");
        assert!(m.entries.contains_key("z/b.txt"), "children move");
        assert!(
            m.entries.contains_key("ax.txt"),
            "prefix match must be component-wise, not string-wise"
        );
    }
}
