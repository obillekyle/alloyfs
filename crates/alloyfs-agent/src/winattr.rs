//! Windows attribute bits (Hidden, System) per export — the v11 feature.
//!
//! Two backends behind one surface, chosen by the platform the AGENT runs on:
//!
//! - **Windows**: the filesystem already holds the truth. Reads come free
//!   through `attr_from_metadata` (the NTFS bits are in every `Metadata`),
//!   and writes go straight to `SetFileAttributesW`. Nothing is stored here.
//! - **Linux**: the bits have no native home, so they live in the export's
//!   own `.alloyfs/winattrs.json` — INSIDE the export on purpose: a backup,
//!   an rsync, or a re-serve from another machine carries the attributes
//!   with the data, where an agent-side store would strand them. The
//!   `.alloyfs` directory is auto-excluded on every export, so no client
//!   ever lists it.
//!
//! Writes are rare (a person toggling Hidden), so single mutations save
//! synchronously — tmp + rename, the manifest's durability idiom. Bulk
//! handlers hold a [`SidecarBatch`] instead: a save is O(whole map), and a
//! WriteMany carrying a thousand executables would otherwise serialize the
//! store a thousand times. Renames move entries (subtrees included);
//! removals drop them; entries whose paths no longer exist drop at load.

use alloyfs_proto::RelPath;
#[cfg(unix)]
use alloyfs_proto::MODE_WIN_MASK;

/// A path → bits map persisted in the export's `.alloyfs` directory: the
/// shared machinery for metadata one platform has natively and the other
/// must carry. Two instances exist, one per direction:
///
/// - `winattrs.json` on a LINUX export: Windows Hidden/System bits.
/// - `posix.json` on a WINDOWS export: POSIX modes, so a Linux client's
///   `chmod +x` survives — NTFS has no execute bit to hold it.
///
/// Same rules for both: writes are rare and save synchronously (tmp +
/// rename), renames move entries subtree-and-all, removals drop them,
/// entries for vanished paths are dropped at load, and an empty store
/// removes its own file and directory.
pub struct SidecarMap {
    file: std::path::PathBuf,
    map: std::sync::Mutex<std::collections::BTreeMap<String, u32>>,
    /// While > 0, mutations mark `dirty` instead of saving — the bulk
    /// handlers hold a [`SidecarBatch`] across their loop, because a save is
    /// O(whole map) and a WriteMany carrying a thousand executables would
    /// otherwise serialize the store a thousand times (O(n²) for the burst).
    /// JSON is not the cost; save-per-mutation was.
    suspend: std::sync::atomic::AtomicU32,
    dirty: std::sync::atomic::AtomicBool,
    /// How many times the file has been written — what the batching test
    /// pins, so the O(n²) can never quietly return.
    saves: std::sync::atomic::AtomicU64,
    /// Entry count, written only under the map lock. `get` runs once per
    /// entry of every served listing, against a map that is EMPTY on
    /// almost every real export — this is what lets it say "no" without
    /// buying the mutex each time.
    len: std::sync::atomic::AtomicUsize,
}

/// Suspends sidecar saves while alive; the drop performs one save if
/// anything changed. Nesting is fine (bulk inside bulk saves once, at the
/// outermost drop). Concurrent single mutations from other sessions fold
/// into the same final save — their dirty mark survives until the last
/// guard drops.
pub struct SidecarBatch<'a>(&'a SidecarMap);

impl Drop for SidecarBatch<'_> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        // The lock comes BEFORE the counter drop. Every mutation checks
        // `suspend` while holding this same lock, so it lands wholly before
        // this drop (its dirty mark is visible here and saved) or wholly
        // after (it reads suspend == 0 and saves itself). Checking outside
        // the lock left a window where both sides declined to save and a
        // mutation sat in memory until the next unrelated save.
        let map = self.0.map.lock().unwrap();
        if self.0.suspend.fetch_sub(1, Ordering::AcqRel) == 1 && self.0.dirty.swap(false, Ordering::AcqRel) {
            self.0.saves.fetch_add(1, Ordering::Relaxed);
            SidecarMap::save(&self.0.file, &map);
        }
    }
}

impl SidecarMap {
    pub fn load(root: &std::path::Path, name: &str, keep: impl Fn(u32) -> bool) -> Self {
        let file = root.join(".alloyfs").join(name);
        let mut map: std::collections::BTreeMap<String, u32> = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // Entries for paths that no longer exist are litter from out-of-band
        // deletes; drop them rather than resurrecting bits onto a future
        // file of the same name.
        map.retain(|p, bits| root.join(p).exists() && keep(*bits));
        Self {
            file,
            len: std::sync::atomic::AtomicUsize::new(map.len()),
            map: std::sync::Mutex::new(map),
            suspend: std::sync::atomic::AtomicU32::new(0),
            dirty: std::sync::atomic::AtomicBool::new(false),
            saves: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Suspend saves for the guard's lifetime; one save happens at the
    /// outermost drop if anything changed. Bulk handlers hold this across
    /// their loops.
    pub fn batch(&self) -> SidecarBatch<'_> {
        self.suspend.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        SidecarBatch(self)
    }

    /// Total file writes so far — exists for the batching test's pin, so
    /// it compiles only there (on Linux nothing else can reach it: the
    /// map sits behind `WinAttrs`' private field).
    #[cfg(test)]
    pub fn save_count(&self) -> u64 {
        self.saves.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Save now unless suspended (then just mark dirty for the guard).
    fn save_or_defer(&self, map: &std::collections::BTreeMap<String, u32>) {
        use std::sync::atomic::Ordering;
        if self.suspend.load(Ordering::Acquire) > 0 {
            self.dirty.store(true, Ordering::Release);
            return;
        }
        self.saves.fetch_add(1, Ordering::Relaxed);
        Self::save(&self.file, map);
    }

    pub fn get(&self, rel: &RelPath) -> Option<u32> {
        // The empty-store fast path. A zero read here is exactly as fresh
        // as taking the lock would have been: `len` only changes under the
        // map lock, so a racing insert lands either wholly before (len
        // visible) or wholly after (this get was ordered first anyway).
        if self.len.load(std::sync::atomic::Ordering::Acquire) == 0 {
            return None;
        }
        self.map.lock().unwrap().get(&rel.0).copied()
    }

    /// Insert or replace; `None` removes.
    pub fn put(&self, rel: &RelPath, value: Option<u32>) {
        let mut map = self.map.lock().unwrap();
        match value {
            Some(v) => {
                map.insert(rel.0.clone(), v);
            }
            None => {
                map.remove(&rel.0);
            }
        }
        self.len.store(map.len(), std::sync::atomic::Ordering::Release);
        self.save_or_defer(&map);
    }

    pub fn remove(&self, rel: &RelPath) {
        let mut map = self.map.lock().unwrap();
        let prefix = format!("{}/", rel.0);
        let before = map.len();
        map.retain(|p, _| p != &rel.0 && !p.starts_with(&prefix));
        if map.len() != before {
            self.len.store(map.len(), std::sync::atomic::Ordering::Release);
            self.save_or_defer(&map);
        }
    }

    pub fn rename(&self, from: &RelPath, to: &RelPath) {
        let mut map = self.map.lock().unwrap();
        let prefix = format!("{}/", from.0);
        let moved: Vec<(String, u32)> = map
            .iter()
            .filter(|(p, _)| *p == &from.0 || p.starts_with(&prefix))
            .map(|(p, b)| (p.clone(), *b))
            .collect();
        if moved.is_empty() {
            return;
        }
        for (p, bits) in moved {
            map.remove(&p);
            let tail = &p[from.0.len()..];
            map.insert(format!("{}{}", to.0, tail), bits);
        }
        self.len.store(map.len(), std::sync::atomic::Ordering::Release);
        self.save_or_defer(&map);
    }

    fn save(file: &std::path::Path, map: &std::collections::BTreeMap<String, u32>) {
        let Some(dir) = file.parent() else { return };
        if map.is_empty() {
            // An empty store earns no file — and no directory, unless the
            // sibling sidecar still needs it.
            let _ = std::fs::remove_file(file);
            let _ = std::fs::remove_dir(dir);
            return;
        }
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = file.with_extension("json.tmp");
        let Ok(text) = serde_json::to_string_pretty(map) else {
            return;
        };
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, file);
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::*;

    pub struct WinAttrs(SidecarMap);

    impl WinAttrs {
        pub fn load(root: &std::path::Path) -> Self {
            Self(SidecarMap::load(root, "winattrs.json", |bits| {
                bits & MODE_WIN_MASK != 0
            }))
        }

        pub fn get(&self, rel: &RelPath) -> u32 {
            self.0.get(rel).unwrap_or(0)
        }

        /// Apply a masked set/clear intent; returns the resulting bits.
        pub fn apply(&self, rel: &RelPath, set: u32, clear: u32) -> u32 {
            let cur = self.get(rel);
            let next = (cur | (set & MODE_WIN_MASK)) & !(clear & MODE_WIN_MASK);
            self.0.put(rel, (next != 0).then_some(next));
            next
        }

        pub fn remove(&self, rel: &RelPath) {
            self.0.remove(rel);
        }

        /// Suspend saves across a bulk of mutations.
        pub fn batch(&self) -> super::SidecarBatch<'_> {
            self.0.batch()
        }

        pub fn rename(&self, from: &RelPath, to: &RelPath) {
            self.0.rename(from, to);
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::path::Path;

    /// Stateless: NTFS is the store. `get` is unused because
    /// `attr_from_metadata` reads the bits straight from every `Metadata`.
    pub struct WinAttrs;

    /// What a no-op batch hands back — droppable, holds nothing.
    pub struct NoopBatch;

    impl WinAttrs {
        pub fn load(_root: &Path) -> Self {
            Self
        }

        pub fn get(&self, _rel: &RelPath) -> u32 {
            0 // never consulted on Windows; the metadata already carries bits
        }

        /// Apply against the REAL attributes, atomically enough for a bit
        /// toggle: read-modify-write on the native flags.
        pub fn apply_native(full: &Path, set: u32, clear: u32) -> std::io::Result<()> {
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<u16> = full.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            // MODE_WIN_HIDDEN (1<<20) ↔ FILE_ATTRIBUTE_HIDDEN (0x2),
            // MODE_WIN_SYSTEM (1<<21) ↔ FILE_ATTRIBUTE_SYSTEM (0x4).
            let to_native = |bits: u32| {
                let mut n = 0u32;
                if bits & alloyfs_proto::MODE_WIN_HIDDEN != 0 {
                    n |= 0x2;
                }
                if bits & alloyfs_proto::MODE_WIN_SYSTEM != 0 {
                    n |= 0x4;
                }
                n
            };
            unsafe {
                let cur = GetFileAttributesW(wide.as_ptr());
                if cur == u32::MAX {
                    return Err(std::io::Error::last_os_error());
                }
                let next = (cur | to_native(set)) & !to_native(clear);
                if next != cur && SetFileAttributesW(wide.as_ptr(), next) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        }

        pub fn remove(&self, _rel: &RelPath) {}
        pub fn rename(&self, _from: &RelPath, _to: &RelPath) {}

        /// Nothing to batch: NTFS is the store. The zero-sized guard keeps
        /// call sites uniform with the sidecar backend.
        pub fn batch(&self) -> NoopBatch {
            NoopBatch
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileAttributesW(lpFileName: *const u16) -> u32;
        fn SetFileAttributesW(lpFileName: *const u16, dwFileAttributes: u32) -> i32;
    }
}

pub use imp::WinAttrs;

#[cfg(test)]
mod tests {
    use super::*;

    /// The efficiency contract: a bulk of N mutations writes the file ONCE,
    /// and the batch's content is durably there on reload. Without the
    /// guard, N mutations were N O(map)-sized serializations — O(n²) for a
    /// WriteMany full of executables.
    #[test]
    fn a_batch_of_mutations_saves_once() {
        let dir = tempfile::TempDir::new().unwrap();
        for i in 0..300 {
            std::fs::write(dir.path().join(format!("f{i}")), b"x").unwrap();
        }
        let map = SidecarMap::load(dir.path(), "t.json", |_| true);
        {
            let _bulk = map.batch();
            for i in 0..300 {
                map.put(&RelPath(format!("f{i}")), Some(0o755));
            }
            assert_eq!(map.save_count(), 0, "suspended: nothing written yet");
        }
        assert_eq!(map.save_count(), 1, "one save for the whole batch");

        // Singles outside a batch still save immediately — durability for
        // the ordinary one-off chmod is unchanged.
        map.put(&RelPath("f0".into()), Some(0o700));
        assert_eq!(map.save_count(), 2);

        // And the batch actually landed: a fresh load sees it.
        let reloaded = SidecarMap::load(dir.path(), "t.json", |_| true);
        assert_eq!(reloaded.get(&RelPath("f299".into())), Some(0o755));
        assert_eq!(reloaded.get(&RelPath("f0".into())), Some(0o700));
    }
}
