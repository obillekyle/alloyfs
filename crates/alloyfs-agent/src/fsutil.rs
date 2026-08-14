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

/// Where a symlink at `link` pointing to `target` would land, as an
/// export-relative path — or `None` if it escapes the export.
///
/// This is deliberately LEXICAL. `canonicalize` is wrong here twice over: a
/// symlink is allowed to dangle, so the target may not exist yet and
/// canonicalize would fail on a perfectly legal link; and where the target
/// does exist, canonicalize would follow it, so a link to a link could be
/// judged by the wrong destination. Resolving the text is the only check that
/// matches what the kernel will actually do when someone walks the link.
///
/// An absolute target is refused outright. On the server it means a path in
/// the server's own root, which is outside the export by definition — and on
/// a client it would mean something different again, which is worse.
pub(crate) fn symlink_lands_inside(link: &RelPath, target: &str) -> Option<String> {
    if target.is_empty() {
        return None;
    }
    // Absolute in either dialect, or a Windows drive letter.
    let bytes = target.as_bytes();
    if target.starts_with('/') || target.starts_with('\\') {
        return None;
    }
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return None;
    }

    // Start in the link's own directory: a relative target is resolved from
    // there, not from the export root.
    let (parent, _) = link.split()?;
    let mut stack: Vec<&str> = if parent.is_root() {
        Vec::new()
    } else {
        parent.0.split('/').collect()
    };
    for comp in target.split(['/', '\\']) {
        match comp {
            "" | "." => {}
            ".." => {
                // Popping past the export root is the escape we are here to
                // catch: "../../etc/passwd" from one level down.
                stack.pop()?;
            }
            other => stack.push(other),
        }
    }
    Some(stack.join("/"))
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
