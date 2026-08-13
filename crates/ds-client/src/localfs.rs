//! Client-side clones of the small std::fs bridges the agent has in
//! fsutil.rs — attr synthesis, positional I/O, errno mapping. Duplicated on
//! purpose (they're ~80 lines) so ds-client doesn't depend on ds-agent.

use std::fs::{File, Metadata};
use std::io;
use std::time::SystemTime;

use ds_proto::{Attr, ErrorCode, FileKind};

pub(crate) fn io_to_code(e: &io::Error) -> ErrorCode {
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

/// Overlay files never participate in version-based freshness — they ARE the
/// source of truth — so their version is always 0.
pub(crate) fn attr_from_metadata(md: &Metadata) -> Attr {
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
        version: 0,
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

pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
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

pub(crate) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
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

pub(crate) fn read_fully(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match read_at(file, &mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

pub(crate) fn write_fully(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
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
