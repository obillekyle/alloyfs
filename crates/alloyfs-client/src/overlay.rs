//! The local overlay: client-excluded paths live here as real files, on this
//! machine only. The server never hears about them — their I/O simply never
//! leaves the client. Routing is purely pattern-based (see RemoteFs), so an
//! excluded name always means the local copy, deterministically.

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use alloyfs_proto::{Attr, ErrorCode, OpenFlags, RelPath};
use dashmap::DashMap;

use crate::remote_fs::FsError;
use alloyfs_common::ExcludeSet;
use alloyfs_common::{attr_from_metadata, read_fully, write_fully, OrCode};

/// Overlay file handles carry this bit; server handles are a small counter
/// (asserted clear on the remote path), so the namespaces can't collide.
pub(crate) const OVERLAY_FH_BIT: u64 = 1 << 63;

struct OverlayHandle {
    file: File,
}

pub(crate) struct Overlay {
    matcher: ExcludeSet,
    root: PathBuf,
    handles: DashMap<u64, OverlayHandle>,
    next_fh: AtomicU64,
}

impl Overlay {
    pub fn new(root: PathBuf, patterns: &[String]) -> anyhow::Result<Self> {
        // The mounted volume presents case-insensitively on Windows, so the
        // matcher must too — "Node_Modules" still routes local there.
        let matcher = ExcludeSet::compile(patterns, cfg!(windows))?;
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            matcher,
            root,
            handles: DashMap::new(),
            next_fh: AtomicU64::new(1),
        })
    }

    pub fn excluded(&self, p: &RelPath) -> bool {
        self.matcher.is_excluded(p)
    }

    fn abs(&self, p: &RelPath) -> PathBuf {
        let mut full = self.root.clone();
        for comp in p.0.split('/').filter(|c| !c.is_empty()) {
            full.push(comp);
        }
        full
    }

    fn handle(&self, fh: u64) -> Result<dashmap::mapref::one::Ref<'_, u64, OverlayHandle>, FsError> {
        self.handles.get(&fh).ok_or(FsError::Remote(ErrorCode::BadHandle))
    }

    pub fn getattr(&self, p: &RelPath) -> Result<Attr, FsError> {
        let md = std::fs::symlink_metadata(self.abs(p)).or_code()?;
        Ok(attr_from_metadata(&md, 0))
    }

    pub fn open(&self, p: &RelPath, flags: OpenFlags) -> Result<(u64, Attr), FsError> {
        let wants_write = flags.write || flags.truncate || flags.append;
        let file = File::options()
            .read(true)
            .write(wants_write)
            .truncate(flags.truncate)
            .open(self.abs(p))
            .or_code()?;
        let md = file.metadata().or_code()?;
        if md.is_dir() {
            return Err(ErrorCode::IsADirectory.into());
        }
        let fh = OVERLAY_FH_BIT | self.next_fh.fetch_add(1, Ordering::Relaxed);
        let attr = attr_from_metadata(&md, 0);
        self.handles.insert(fh, OverlayHandle { file });
        Ok((fh, attr))
    }

    pub fn create(&self, p: &RelPath, flags: OpenFlags, mode: u32) -> Result<(u64, Attr), FsError> {
        let full = self.abs(p);
        if let Some(parent) = full.parent() {
            // First write into a fresh excluded subtree materializes it.
            std::fs::create_dir_all(parent).or_code()?;
        }
        let mut opts = File::options();
        opts.read(true).write(true);
        if flags.excl {
            opts.create_new(true);
        } else {
            opts.create(true).truncate(flags.truncate);
        }
        let file = opts.open(&full).or_code()?;
        let _ = mode; // local umask/ACL decides
        let md = file.metadata().or_code()?;
        let fh = OVERLAY_FH_BIT | self.next_fh.fetch_add(1, Ordering::Relaxed);
        let attr = attr_from_metadata(&md, 0);
        self.handles.insert(fh, OverlayHandle { file });
        Ok((fh, attr))
    }

    pub fn read(&self, fh: u64, offset: u64, len: u32) -> Result<Vec<u8>, FsError> {
        let h = self.handle(fh)?;
        let mut buf = vec![0u8; len as usize];
        let n = read_fully(&h.file, &mut buf, offset).or_code()?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, FsError> {
        let h = self.handle(fh)?;
        write_fully(&h.file, data, offset).or_code()?;
        Ok(data.len() as u32)
    }

    /// Direct children present on local disk under `dir` (empty if the
    /// directory doesn't exist locally yet).
    pub fn readdir_children(&self, dir: &RelPath) -> Vec<(String, Attr)> {
        let Ok(rd) = std::fs::read_dir(self.abs(dir)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for item in rd.flatten() {
            let Ok(name) = item.file_name().into_string() else {
                continue;
            };
            let Ok(md) = item
                .metadata()
                .or_else(|_| std::fs::symlink_metadata(item.path()))
            else {
                continue;
            };
            out.push((name, attr_from_metadata(&md, 0)));
        }
        out
    }

    pub fn mkdir(&self, p: &RelPath) -> Result<Attr, FsError> {
        let full = self.abs(p);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).or_code()?;
        }
        std::fs::create_dir(&full).or_code()?;
        let md = std::fs::metadata(&full).or_code()?;
        Ok(attr_from_metadata(&md, 0))
    }

    pub fn unlink(&self, p: &RelPath) -> Result<(), FsError> {
        Ok(std::fs::remove_file(self.abs(p)).or_code()?)
    }

    pub fn rmdir(&self, p: &RelPath) -> Result<(), FsError> {
        Ok(std::fs::remove_dir(self.abs(p)).or_code()?)
    }

    pub fn rename(&self, from: &RelPath, to: &RelPath, replace: bool) -> Result<(), FsError> {
        let from_full = self.abs(from);
        let to_full = self.abs(to);
        if !replace && to_full.exists() {
            return Err(ErrorCode::AlreadyExists.into());
        }
        if let Some(parent) = to_full.parent() {
            std::fs::create_dir_all(parent).or_code()?;
        }
        // std::fs::rename replaces atomically on both platforms (see the
        // matching note in alloyfs-agent's rename).
        Ok(std::fs::rename(&from_full, &to_full).or_code()?)
    }

    pub fn setattr(
        &self,
        p: &RelPath,
        size: Option<u64>,
        mtime: Option<SystemTime>,
        _mode: Option<u32>,
    ) -> Result<Attr, FsError> {
        let full = self.abs(p);
        if let Some(size) = size {
            let f = File::options().write(true).open(&full).or_code()?;
            f.set_len(size).or_code()?;
        }
        if let Some(mtime) = mtime {
            let f = File::options().write(true).open(&full).or_code()?;
            f.set_modified(mtime).or_code()?;
        }
        let md = std::fs::metadata(&full).or_code()?;
        Ok(attr_from_metadata(&md, 0))
    }

    pub fn link(&self, target: &RelPath, link: &RelPath) -> Result<Attr, FsError> {
        let link_full = self.abs(link);
        if let Some(parent) = link_full.parent() {
            std::fs::create_dir_all(parent).or_code()?;
        }
        std::fs::hard_link(self.abs(target), &link_full).or_code()?;
        let md = std::fs::metadata(&link_full).or_code()?;
        Ok(attr_from_metadata(&md, 0))
    }

    /// Symlink inside the overlay. No escape check here: the overlay is this
    /// machine's own directory, not a boundary being defended — the same
    /// reasoning that makes advisory locks a no-op on overlay handles.
    pub fn symlink(&self, target: &str, link: &RelPath) -> Result<Attr, FsError> {
        let link_full = self.abs(link);
        if let Some(parent) = link_full.parent() {
            std::fs::create_dir_all(parent).or_code()?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &link_full).or_code()?;
        #[cfg(windows)]
        {
            let resolved = link_full.parent().map(|p| p.join(target));
            if resolved.is_some_and(|p| p.is_dir()) {
                std::os::windows::fs::symlink_dir(target, &link_full).or_code()?;
            } else {
                std::os::windows::fs::symlink_file(target, &link_full).or_code()?;
            }
        }
        let md = std::fs::symlink_metadata(&link_full).or_code()?;
        Ok(attr_from_metadata(&md, 0))
    }

    pub fn readlink(&self, path: &RelPath) -> Result<String, FsError> {
        let target = std::fs::read_link(self.abs(path)).or_code()?;
        let target = target.to_str().ok_or(ErrorCode::InvalidPath)?;
        Ok(target.replace('\\', "/"))
    }

    pub fn flush(&self, fh: u64) -> Result<(), FsError> {
        let h = self.handle(fh)?;
        let _ = h.file.sync_data(); // best-effort, matches remote Flush
        Ok(())
    }

    pub fn release(&self, fh: u64) {
        self.handles.remove(&fh);
    }

    /// On-disk top-level entries that no longer match any exclude pattern —
    /// invisible orphans worth warning about at mount time.
    pub fn orphans(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|item| item.file_name().into_string().ok())
            .filter(|name| !self.matcher.is_excluded(&RelPath(name.clone())))
            .collect()
    }
}
