//! Small bridges between std::fs and the wire types.

use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ds_proto::{Attr, ErrorCode, FileKind, RelPath};

pub fn io_to_code(e: &io::Error) -> ErrorCode {
    use io::ErrorKind::*;
    match e.kind() {
        NotFound => ErrorCode::NotFound,
        PermissionDenied => ErrorCode::PermissionDenied,
        AlreadyExists => ErrorCode::AlreadyExists,
        NotADirectory => ErrorCode::NotADirectory,
        IsADirectory => ErrorCode::IsADirectory,
        DirectoryNotEmpty => ErrorCode::NotEmpty,
        _ => ErrorCode::Io,
    }
}

/// Resolve `rel` inside `root_canon` (an already-canonicalized export root),
/// refusing anything that escapes it. Symlinks are resolved server-side, so a
/// link pointing outside the export is caught by the prefix check.
pub fn resolve(root_canon: &Path, rel: &RelPath) -> Result<PathBuf, ErrorCode> {
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

pub fn attr_from_metadata(md: &Metadata, version: u64) -> Attr {
    let kind = if md.is_dir() {
        FileKind::Dir
    } else if md.is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::File
    };
    let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let ctime = md.created().unwrap_or(mtime);
    Attr {
        kind,
        size: md.len(),
        mtime,
        ctime,
        mode: mode_of(md),
        version,
    }
}

#[cfg(unix)]
fn mode_of(md: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o7777
}

#[cfg(windows)]
fn mode_of(md: &Metadata) -> u32 {
    let base = if md.is_dir() { 0o755 } else { 0o644 };
    if md.permissions().readonly() {
        base & !0o222
    } else {
        base
    }
}

/// Apply Unix permission bits to an open file. On Windows only the write bit
/// is meaningful (read-only attribute); best-effort on both.
pub fn set_mode(file: &File, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777));
    }
    #[cfg(windows)]
    {
        if let Ok(md) = file.metadata() {
            let mut perms = md.permissions();
            perms.set_readonly(mode & 0o200 == 0);
            let _ = file.set_permissions(perms);
        }
    }
}

/// Positional write mirroring `read_at`.
pub fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_write(buf, offset)
    }
}

/// Write all of `buf` at `offset` (loops over short writes).
pub fn write_fully(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match write_at(file, &buf[done..], offset + done as u64) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Positional read that works on both platforms (pread on Unix; a seek+read
/// on Windows, where `seek_read` moves the file cursor — fine because every
/// access goes through this helper with an explicit offset).
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, offset)
    }
}

/// Fill `buf` as far as possible from `offset` (loops over short reads).
pub fn read_fully(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match read_at(file, &mut buf[done..], offset + done as u64) {
            Ok(0) => break, // EOF
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}
