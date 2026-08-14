//! Agent-only path resolution primitive. Everything else that used to live
//! here (attr synthesis, positional I/O, errno mapping) moved to `alloyfs-common`.

use std::path::{Path, PathBuf};

use alloyfs_common::io_to_code;
use alloyfs_proto::{ErrorCode, RelPath};

/// Resolve `rel` inside `root_canon`, refusing anything that escapes it.
/// Symlinks are resolved server-side, so a link pointing outside the export
/// is caught by the prefix check.
///
/// UNCHECKED against export excludes — which is why request handlers must
/// never call this directly: `Export::resolve` / `Export::resolve_new` are
/// the security choke points that add exclusion enforcement on top.
pub(crate) fn resolve_unchecked(root_canon: &Path, rel: &RelPath) -> Result<PathBuf, ErrorCode> {
    rel.validate()?;
    if rel.is_root() {
        return Ok(root_canon.to_path_buf());
    }
    let mut full = root_canon.to_path_buf();
    for comp in rel.0.split('/') {
        full.push(comp);
    }
    let canon = std::fs::canonicalize(&full).map_err(|e| io_to_code(&e))?;
    if !canon.starts_with(root_canon) {
        tracing::warn!(path = %rel, "path escapes export root (symlink?)");
        return Err(ErrorCode::PermissionDenied);
    }
    Ok(canon)
}

/// Real (block_size, blocks, blocks_free) for the filesystem holding `path`.
/// None on failure — the caller keeps its placeholders.
#[cfg(unix)]
pub(crate) fn fs_space(path: &Path) -> Option<(u32, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid NUL-terminated path, st is a zeroed out-param.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // f_frsize is the fragment size blocks are counted in; fall back to
    // f_bsize where it's zero (some filesystems).
    let bs = if st.f_frsize > 0 { st.f_frsize } else { st.f_bsize };
    Some((bs as u32, st.f_blocks as u64, st.f_bavail as u64))
}

#[cfg(windows)]
pub(crate) fn fs_space(path: &Path) -> Option<(u32, u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: wide is NUL-terminated; the three out-params are valid u64s.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    if ok == 0 {
        return None;
    }
    const BS: u64 = 4096; // report in 4K blocks; callers only need ratios×size
    Some((BS as u32, total / BS, avail / BS))
}
