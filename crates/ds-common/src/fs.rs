//! std::fs ↔ wire-type bridges used by both sides.

use std::fs::{File, Metadata};
use std::io;
use std::time::SystemTime;

use ds_proto::{Attr, FileKind};

/// Attr synthesis from local metadata. The server passes its per-file version
/// counter; the client's overlay passes 0 (overlay files ARE the source of
/// truth and never join version-based freshness).
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

/// Positional read that works on both platforms (pread on Unix; seek_read on
/// Windows — it moves the cursor, fine because every access passes an
/// explicit offset).
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
