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
//! Writes are rare (a person toggling Hidden), so the sidecar saves
//! synchronously — tmp + rename, the same durability idiom the manifest
//! uses — rather than carrying a flusher for a file that changes once a
//! week. Renames move entries (subtrees included); removals drop them;
//! entries whose paths no longer exist are dropped at load.

use alloyfs_proto::RelPath;
#[cfg(unix)]
use alloyfs_proto::MODE_WIN_MASK;

#[cfg(unix)]
mod imp {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    pub struct WinAttrs {
        file: PathBuf,
        map: Mutex<BTreeMap<String, u32>>,
    }

    impl WinAttrs {
        pub fn load(root: &Path) -> Self {
            let file = root.join(".alloyfs").join("winattrs.json");
            let mut map: BTreeMap<String, u32> = std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            // Entries for paths that no longer exist are litter from
            // out-of-band deletes; drop them rather than resurrecting bits
            // onto a future file of the same name.
            map.retain(|p, bits| root.join(p).exists() && *bits & MODE_WIN_MASK != 0);
            Self {
                file,
                map: Mutex::new(map),
            }
        }

        pub fn get(&self, rel: &RelPath) -> u32 {
            self.map.lock().unwrap().get(&rel.0).copied().unwrap_or(0)
        }

        /// Apply a masked set/clear intent; returns the resulting bits.
        pub fn apply(&self, rel: &RelPath, set: u32, clear: u32) -> u32 {
            let mut map = self.map.lock().unwrap();
            let cur = map.get(&rel.0).copied().unwrap_or(0);
            let next = (cur | (set & MODE_WIN_MASK)) & !(clear & MODE_WIN_MASK);
            if next == 0 {
                map.remove(&rel.0);
            } else {
                map.insert(rel.0.clone(), next);
            }
            Self::save(&self.file, &map);
            next
        }

        pub fn remove(&self, rel: &RelPath) {
            let mut map = self.map.lock().unwrap();
            let prefix = format!("{}/", rel.0);
            let before = map.len();
            map.retain(|p, _| p != &rel.0 && !p.starts_with(&prefix));
            if map.len() != before {
                Self::save(&self.file, &map);
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
            Self::save(&self.file, &map);
        }

        fn save(file: &Path, map: &BTreeMap<String, u32>) {
            let Some(dir) = file.parent() else { return };
            if map.is_empty() {
                // An empty store earns no directory in the export.
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
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::path::Path;

    /// Stateless: NTFS is the store. `get` is unused because
    /// `attr_from_metadata` reads the bits straight from every `Metadata`.
    pub struct WinAttrs;

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
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileAttributesW(lpFileName: *const u16) -> u32;
        fn SetFileAttributesW(lpFileName: *const u16, dwFileAttributes: u32) -> i32;
    }
}

pub use imp::WinAttrs;
