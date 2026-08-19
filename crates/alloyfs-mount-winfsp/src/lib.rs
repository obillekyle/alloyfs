//! Windows mount backend: adapts WinFsp's callback dialect to `RemoteFs`.
//!
//! winfsp-rs 0.13 API notes: `FileSystemContext` methods take `&self` and the
//! per-handle `FileContext` is only ever seen behind a shared reference, so
//! anything mutable in it needs interior mutability. Paths arrive as
//! backslash-separated `U16CStr`s ("\dir\sub\file.txt"); the wire protocol
//! wants forward-slash `RelPath`s ("dir/sub/file.txt", empty string for the
//! root). Errors travel back as NTSTATUS via `FspError::NTSTATUS`.
#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloyfs_client::{FsError, RemoteFs};
use alloyfs_proto::{Attr, ErrorCode, FileKind, OpenFlags, RelPath};
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{
    DirBuffer, DirBufferLock, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp::{FspError, U16CStr};

// ---------------------------------------------------------------------------
// Win32 constants. Kept as plain integers instead of pulling in the whole
// `windows` crate: winfsp-sys types these fields as bare u32/i32 anyway.
// ---------------------------------------------------------------------------

const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

/// NtCreateFile create-option: caller demands a directory.
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;

// Access rights relevant to deciding whether we need a server file handle.
const FILE_READ_DATA: u32 = 0x0000_0001;
const FILE_WRITE_DATA: u32 = 0x0000_0002;
const FILE_APPEND_DATA: u32 = 0x0000_0004;
const FILE_EXECUTE: u32 = 0x0000_0020;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_EXECUTE: u32 = 0x2000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;

// NTSTATUS values (from ntstatus.h), stored as i32 like winfsp-sys expects.
const STATUS_INVALID_HANDLE: i32 = 0xC000_0008u32 as i32;
const STATUS_END_OF_FILE: i32 = 0xC000_0011u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_OBJECT_NAME_INVALID: i32 = 0xC000_0033u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
const STATUS_MEDIA_WRITE_PROTECTED: i32 = 0xC000_00A2u32 as i32;
const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101u32 as i32;
const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;
const STATUS_SHARING_VIOLATION: i32 = 0xC000_0043u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC000_0185u32 as i32;
/// What NTFS returns for ReadFile/WriteFile on a directory handle. Programs
/// (bun!) probe paths by reading them and key "is a directory" off exactly
/// this status — anything else is treated as a real error.
const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC000_0010u32 as i32;
const STATUS_NOT_SAME_DEVICE: i32 = 0xC000_00D4u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;
const STATUS_NOT_A_REPARSE_POINT: i32 = 0xC000_0275u32 as i32;
const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BBu32 as i32;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Bytes per allocation unit as advertised in `VolumeParams` (sector_size 4096
///   sectors_per_allocation_unit 1). `FileInfo::allocation_size` is rounded up
///   to this.
const ALLOCATION_UNIT: u64 = 4096;

fn nt(code: i32) -> FspError {
    FspError::NTSTATUS(code)
}

/// Map wire/transport errors onto NTSTATUS.
fn fsp_err(e: FsError) -> FspError {
    tracing::trace!(error = %e, "op failed");
    let code = match e {
        FsError::Remote(code) => match code {
            ErrorCode::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
            ErrorCode::PermissionDenied => STATUS_ACCESS_DENIED,
            ErrorCode::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
            ErrorCode::NotADirectory => STATUS_NOT_A_DIRECTORY,
            ErrorCode::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
            ErrorCode::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
            ErrorCode::InvalidPath => STATUS_OBJECT_NAME_INVALID,
            ErrorCode::BadHandle => STATUS_INVALID_HANDLE,
            ErrorCode::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
            ErrorCode::WouldBlock => STATUS_SHARING_VIOLATION,
            ErrorCode::CrossDevice => STATUS_NOT_SAME_DEVICE,
            // "the server is too old for this" reads far better as "not
            // supported" than as an I/O device error.
            ErrorCode::VersionMismatch => STATUS_NOT_SUPPORTED,
            _ => STATUS_IO_DEVICE_ERROR,
        },
        FsError::Transport(_) => STATUS_IO_DEVICE_ERROR,
    };
    FspError::NTSTATUS(code)
}

/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01 (Unix epoch).
const FILETIME_UNIX_DIFF_SECS: u64 = 11_644_473_600;

/// SystemTime -> Windows FILETIME (100ns intervals since 1601-01-01).
fn to_filetime(t: SystemTime) -> u64 {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    (d.as_secs() + FILETIME_UNIX_DIFF_SECS) * 10_000_000 + u64::from(d.subsec_nanos()) / 100
}

/// Windows FILETIME -> SystemTime (clamped at the Unix epoch).
fn from_filetime(ft: u64) -> SystemTime {
    let unix_100ns = ft.saturating_sub(FILETIME_UNIX_DIFF_SECS * 10_000_000);
    UNIX_EPOCH + Duration::from_nanos(unix_100ns.saturating_mul(100))
}

/// Windows-facing attribute bits for a server `Attr`.
fn attributes(attr: &Attr) -> u32 {
    match attr.kind {
        FileKind::Dir => FILE_ATTRIBUTE_DIRECTORY,
        // A symlink is a file that also carries the reparse bit. Without it
        // Windows never asks for the reparse data, so the link would look
        // like an empty file to Explorer and to every API that follows one.
        FileKind::Symlink => FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_REPARSE_POINT,
        FileKind::File => {
            let mut bits = FILE_ATTRIBUTE_ARCHIVE;
            if attr.mode & 0o222 == 0 {
                bits |= FILE_ATTRIBUTE_READONLY;
            }
            bits
        }
    }
}

fn fill_file_info(fi: &mut FileInfo, ino: u64, attr: &Attr) {
    let mtime = to_filetime(attr.mtime);
    let ctime = to_filetime(attr.ctime);
    fi.file_attributes = attributes(attr);
    // The tag has to accompany the attribute bit: the driver reads it to
    // decide which reparse handler applies.
    fi.reparse_tag = match attr.kind {
        FileKind::Symlink => IO_REPARSE_TAG_SYMLINK,
        _ => 0,
    };
    fi.file_size = attr.size;
    fi.allocation_size = attr.size.div_ceil(ALLOCATION_UNIT) * ALLOCATION_UNIT;
    // The protocol only carries mtime/ctime; creation and access times are
    // synthesized from those.
    fi.creation_time = ctime;
    fi.last_access_time = mtime;
    fi.last_write_time = mtime;
    fi.change_time = mtime.max(ctime);
    fi.index_number = ino;
    fi.hard_links = 0;
    fi.ea_size = 0;
}

// --------------------------------------------------------------- reparse

const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const SYMLINK_FLAG_RELATIVE: u32 = 0x0000_0001;
/// `REPARSE_DATA_BUFFER` up to and including `Reserved`.
const REPARSE_HEADER_LEN: usize = 8;
/// The `SymbolicLinkReparseBuffer` fields before `PathBuffer`.
const SYMLINK_HEADER_LEN: usize = 12;

/// Is this target absolute in Windows terms? Relative links must set
/// SYMLINK_FLAG_RELATIVE or Explorer and the shell resolve them from the
/// volume root instead of the link's own directory.
fn target_is_absolute(target: &str) -> bool {
    let b = target.as_bytes();
    target.starts_with('\\')
        || target.starts_with('/')
        || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
}

/// Build a symlink `REPARSE_DATA_BUFFER` for `target`.
///
/// SubstituteName and PrintName are the same string here. They differ on NTFS
/// only because it stores substitute names in `\??\` form; ours are plain
/// paths the server understands, so there is nothing to translate.
fn encode_symlink_reparse(target: &str) -> Vec<u8> {
    let wide: Vec<u16> = target.replace('/', "\\").encode_utf16().collect();
    let name_bytes = wide.len() * 2;
    let data_len = SYMLINK_HEADER_LEN + name_bytes * 2;

    let mut buf = Vec::with_capacity(REPARSE_HEADER_LEN + data_len);
    buf.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    buf.extend_from_slice(&(data_len as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    buf.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    buf.extend_from_slice(&(name_bytes as u16).to_le_bytes()); // SubstituteNameLength
    buf.extend_from_slice(&(name_bytes as u16).to_le_bytes()); // PrintNameOffset
    buf.extend_from_slice(&(name_bytes as u16).to_le_bytes()); // PrintNameLength
    let flags = if target_is_absolute(target) {
        0
    } else {
        SYMLINK_FLAG_RELATIVE
    };
    buf.extend_from_slice(&flags.to_le_bytes());
    for unit in wide.iter().chain(wide.iter()) {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
    buf
}

/// Pull the target back out of a symlink `REPARSE_DATA_BUFFER`.
///
/// Every offset and length is bounds-checked against the buffer the driver
/// handed us before it is used to slice: these come from userspace via an
/// FSCTL, so a length that indexes past the end is a thing an application can
/// simply ask for.
fn decode_symlink_reparse(buf: &[u8]) -> Option<String> {
    if buf.len() < REPARSE_HEADER_LEN + SYMLINK_HEADER_LEN {
        return None;
    }
    let tag = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    if tag != IO_REPARSE_TAG_SYMLINK {
        return None;
    }
    let sub_off = u16::from_le_bytes(buf[8..10].try_into().ok()?) as usize;
    let sub_len = u16::from_le_bytes(buf[10..12].try_into().ok()?) as usize;
    if sub_len == 0 || !sub_len.is_multiple_of(2) {
        return None;
    }
    let path = &buf[REPARSE_HEADER_LEN + SYMLINK_HEADER_LEN..];
    let end = sub_off.checked_add(sub_len)?;
    if end > path.len() {
        return None;
    }
    let units: Vec<u16> = path[sub_off..end]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16(&units).ok()?;
    // NTFS-style substitute names arrive prefixed; the server has no idea
    // what \??\ means.
    let s = s.strip_prefix("\\??\\").unwrap_or(&s).to_string();
    // The wire always speaks forward slashes.
    Some(s.replace('\\', "/"))
}

/// "\dir\sub\file.txt" (or "\") -> RelPath("dir/sub/file.txt") / RelPath("").
fn rel_path(file_name: &U16CStr) -> winfsp::Result<RelPath> {
    let s = file_name
        .to_string()
        .map_err(|_| nt(STATUS_OBJECT_NAME_INVALID))?;
    let normalized = s.replace('\\', "/");
    Ok(RelPath(normalized.trim_matches('/').to_string()))
}

/// One open Windows handle. WinFsp hands this back by shared reference on
/// every callback, hence the atomics for the mutable bit.
pub struct FileContext {
    ino: u64,
    /// Server-side file handle; `None` for directories and metadata-only opens.
    fh: Option<u64>,
    is_dir: bool,
    /// Driver-managed enumeration buffer (interior-mutable by design).
    dir_buffer: DirBuffer,
    /// Set by `set_delete`; the actual unlink happens in `cleanup` when the
    /// FSD passes `FspCleanupDelete` (which it also does for
    /// FILE_DELETE_ON_CLOSE opens that never called `set_delete`).
    delete_pending: AtomicBool,
}

/// Adapter: implements the WinFsp callbacks on top of `Arc<RemoteFs>`.
///
/// `RemoteFs` methods internally `block_on` the tokio runtime; WinFsp calls us
/// on its own plain dispatcher threads, so that's fine as long as the runtime
/// itself stays alive for the duration of the mount (alloyfs-cli guarantees that).
struct WinFspFs {
    fs: Arc<RemoteFs>,
    volume_label: String,
    /// Server events waiting to be re-emitted as native change notifications
    /// (drained by the WinFsp notify timer; fed by the client event pump).
    pending_events: Arc<std::sync::Mutex<Vec<alloyfs_proto::FsEvent>>>,
}

impl WinFspFs {
    /// Path -> (ino, Attr), allocating an inode on first sight and rolling the
    /// allocation back if the server says the path doesn't exist.
    fn resolve(&self, path: &RelPath) -> Result<(u64, Attr), FsError> {
        let existing = self.fs.ino.ino_of(path);
        let ino = existing.unwrap_or_else(|| self.fs.ino.get_or_alloc(path.clone()));
        match self.fs.getattr(ino) {
            Ok(attr) => Ok((ino, attr)),
            Err(e) => {
                if existing.is_none() {
                    self.fs.ino.forget(ino);
                }
                Err(e)
            }
        }
    }

    fn write_dir_entry(
        &self,
        lock: &DirBufferLock,
        dirinfo: &mut DirInfo,
        name: &str,
        ino: u64,
        attr: &Attr,
    ) -> winfsp::Result<()> {
        dirinfo.reset();
        // Upstream bug workaround: `set_name` appends a NUL terminator AND
        // counts it in the entry's name length, which breaks kernel-side
        // pattern matching (`del file.txt`, `dir *.txt` → "File Not Found":
        // end-anchored patterns never match "file.txt\0"). Encode without the
        // terminator and use set_name_raw, whose size is exactly what we pass.
        let wide: Vec<u16> = name.encode_utf16().collect();
        if dirinfo.set_name_raw(wide.as_slice()).is_err() {
            // Name longer than the 255-unit DirInfo buffer: skip the entry
            // rather than failing the whole enumeration.
            tracing::warn!(name, "skipping directory entry: name too long");
            return Ok(());
        }
        fill_file_info(dirinfo.file_info_mut(), ino, attr);
        lock.write(dirinfo)
    }
}

impl FileSystemContext for WinFspFs {
    type FileContext = FileContext;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [std::ffi::c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = rel_path(file_name)?;
        tracing::trace!(path = %path, "get_security_by_name");
        let (_ino, attr) = self.resolve(&path).map_err(fsp_err)?;
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: write_security_descriptor(security_descriptor)?,
            attributes: attributes(&attr),
        })
    }

    /// The descriptor for an OPEN file — what `icacls` and `Get-Acl` ask.
    ///
    /// Separate from `get_security_by_name`, which answers the access check at
    /// open time. Left unimplemented this returns STATUS_INVALID_DEVICE_REQUEST
    /// and Windows reports "no permissions are set, all users have full
    /// control" — accidentally true here, and misleading everywhere it is
    /// read, since it describes a NULL DACL rather than the one actually
    /// enforced. Answering with the same descriptor keeps the two consistent.
    fn get_security(
        &self,
        _context: &Self::FileContext,
        security_descriptor: Option<&mut [std::ffi::c_void]>,
    ) -> winfsp::Result<u64> {
        write_security_descriptor(security_descriptor)
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<FileContext> {
        let path = rel_path(file_name)?;
        tracing::trace!(path = %path, granted_access = format_args!("{granted_access:#x}"), "open");
        let (ino, mut attr) = self.resolve(&path).map_err(fsp_err)?;
        let is_dir = attr.kind == FileKind::Dir;

        let mut fh = None;
        if !is_dir {
            let wants_write =
                granted_access & (FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE | GENERIC_ALL) != 0;
            let wants_read = granted_access
                & (FILE_READ_DATA | FILE_EXECUTE | GENERIC_READ | GENERIC_EXECUTE | GENERIC_ALL)
                != 0;
            // Metadata-only opens (attribute queries, delete, rename) skip the
            // server round-trip and get no data handle.
            if wants_read || wants_write {
                let flags = OpenFlags {
                    read: true,
                    write: wants_write,
                    ..OpenFlags::default()
                };
                let (h, a) = self.fs.open(ino, flags).map_err(fsp_err)?;
                fh = Some(h);
                attr = a;
            }
        }

        fill_file_info(file_info.as_mut(), ino, &attr);
        Ok(FileContext {
            ino,
            fh,
            is_dir,
            dir_buffer: DirBuffer::new(),
            delete_pending: AtomicBool::new(false),
        })
    }

    fn close(&self, context: FileContext) {
        if let Some(fh) = context.fh {
            self.fs.release(fh);
        }
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[std::ffi::c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<FileContext> {
        let path = rel_path(file_name)?;
        tracing::trace!(path = %path, create_options = format_args!("{create_options:#x}"), "create");
        let Some((parent_path, name)) = path.split() else {
            // Creating the root itself.
            return Err(nt(STATUS_OBJECT_NAME_COLLISION));
        };
        let (parent_ino, parent_attr) = self.resolve(&parent_path).map_err(fsp_err)?;
        if parent_attr.kind != FileKind::Dir {
            return Err(nt(STATUS_NOT_A_DIRECTORY));
        }

        if create_options & FILE_DIRECTORY_FILE != 0 {
            // 0o777 for the same reason `create` sends 0o666: hand over the
            // unrestricted mode and let the server's umask reduce it. The agent
            // discards this value today, so nothing changes on the wire — but a
            // dead argument that still carries a wrong number is a trap for
            // whoever makes it live.
            let (ino, attr) = self.fs.mkdir(parent_ino, name, 0o777).map_err(fsp_err)?;
            fill_file_info(file_info.as_mut(), ino, &attr);
            Ok(FileContext {
                ino,
                fh: None,
                is_dir: true,
                dir_buffer: DirBuffer::new(),
                delete_pending: AtomicBool::new(false),
            })
        } else {
            // WinFsp only routes true creations here (open-or-create
            // dispositions resolve via get_security_by_name first), so
            // exclusive semantics are correct.
            let excl = OpenFlags {
                read: true,
                write: true,
                excl: true,
                ..OpenFlags::default()
            };
            // 0o666, not 0o644 — the maximally permissive mode, exactly as
            // `fopen` and every `open(..., O_CREAT, 0666)` in C does it. Windows
            // has no umask and no group-write bit, so ANY mode this client picks
            // is fabricated; the only honest thing to send is "no restriction
            // from here" and let the SERVER's umask decide. `mkdir` below says
            // the same thing, and the agent has always ignored its mode for
            // precisely that reason.
            //
            // Sending 0o644 was the client inventing a umask it does not have,
            // and on a server with POSIX ACLs it did more than pick a mode: a
            // new file's ACL mask is intersected with the group bits of the
            // creation mode, so group `r--` clamped every ACL entry to
            // read-only. Measured against an export whose default ACL granted
            // www-data rwx: files written through the mount came out
            // `mask::r--` and Apache could no longer write what it had been
            // able to write before. With 0o666 and the server's umask of 002
            // the same write lands 0664 and the mask stays rw-.
            let (ino, fh, attr) = self.fs.create(parent_ino, name, 0o666, excl).map_err(fsp_err)?;
            fill_file_info(file_info.as_mut(), ino, &attr);
            Ok(FileContext {
                ino,
                fh: Some(fh),
                is_dir: false,
                dir_buffer: DirBuffer::new(),
                delete_pending: AtomicBool::new(false),
            })
        }
    }

    fn overwrite(
        &self,
        context: &FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let attr = self
            .fs
            .setattr(context.ino, Some(0), None, None)
            .map_err(fsp_err)?;
        fill_file_info(file_info, context.ino, &attr);
        Ok(())
    }

    fn cleanup(&self, context: &FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        // Deletion is a three-stage dance on Windows: open -> set_delete (or
        // FILE_DELETE_ON_CLOSE) -> cleanup with FspCleanupDelete. Only now may
        // the file actually be removed. Cleanup cannot report failure.
        if !FspCleanupFlags::FspCleanupDelete.is_flagged(flags) {
            return;
        }
        let Some(path) = self.fs.ino.path_of(context.ino) else {
            return;
        };
        let Some((parent_path, name)) = path.split() else {
            return; // never delete the root
        };
        let parent_ino = self
            .fs
            .ino
            .ino_of(&parent_path)
            .unwrap_or_else(|| self.fs.ino.get_or_alloc(parent_path.clone()));
        let result = if context.is_dir {
            self.fs.rmdir(parent_ino, name)
        } else {
            self.fs.unlink(parent_ino, name)
        };
        match result {
            Ok(()) => self.fs.ino.forget(context.ino),
            Err(e) => tracing::warn!(path = %path, error = %e, "delete-on-cleanup failed"),
        }
    }

    fn flush(&self, context: Option<&FileContext>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        let Some(ctx) = context else {
            return Ok(()); // volume flush: writes are already write-through
        };
        if let Some(fh) = ctx.fh {
            self.fs.flush(fh).map_err(fsp_err)?;
        }
        let attr = self.fs.getattr(ctx.ino).map_err(fsp_err)?;
        fill_file_info(file_info, ctx.ino, &attr);
        Ok(())
    }

    fn get_file_info(&self, context: &FileContext, file_info: &mut FileInfo) -> winfsp::Result<()> {
        let attr = self.fs.getattr(context.ino).map_err(fsp_err)?;
        fill_file_info(file_info, context.ino, &attr);
        Ok(())
    }

    fn read(&self, context: &FileContext, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        let Some(fh) = context.fh else {
            // NTFS parity matters here: reading a directory handle must be
            // STATUS_INVALID_DEVICE_REQUEST (bun probes paths by reading them
            // and detects directories from this exact status); a data read on
            // a metadata-only file handle is an access problem.
            return Err(nt(if context.is_dir {
                STATUS_INVALID_DEVICE_REQUEST
            } else {
                STATUS_ACCESS_DENIED
            }));
        };
        let data = self.fs.read(fh, offset, buffer.len() as u32).map_err(fsp_err)?;
        if data.is_empty() && !buffer.is_empty() {
            return Err(nt(STATUS_END_OF_FILE));
        }
        buffer[..data.len()].copy_from_slice(&data);
        Ok(data.len() as u32)
    }

    fn write(
        &self,
        context: &FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let Some(fh) = context.fh else {
            // Mirror NTFS: writes to a directory handle are invalid device
            // requests, not bad handles (see read()).
            return Err(nt(if context.is_dir {
                STATUS_INVALID_DEVICE_REQUEST
            } else {
                STATUS_ACCESS_DENIED
            }));
        };
        let attr = self.fs.getattr(context.ino).map_err(fsp_err)?;

        let (offset, data) = if write_to_eof {
            (attr.size, buffer)
        } else if constrained_io {
            // Paging I/O must never extend the file.
            if offset >= attr.size {
                fill_file_info(file_info, context.ino, &attr);
                return Ok(0);
            }
            let max = (attr.size - offset) as usize;
            (offset, &buffer[..buffer.len().min(max)])
        } else {
            (offset, buffer)
        };

        let (n, written_attr) = self.fs.write_at(fh, offset, data).map_err(fsp_err)?;

        // Windows wants the file's post-write state in the reply to the write
        // itself, which is why this used to be the point where a Getattr was
        // unavoidable — and why it was skipped, by extrapolating the size and
        // leaving the mtime at its pre-write value.
        //
        // Protocol v5 sends the real attributes back with the write, so there
        // is nothing left to guess. Against an older agent the extrapolation
        // stands, and the cache entry has already been dropped by `write_at`
        // — the next stat pays for the truth.
        let new_attr = written_attr.unwrap_or_else(|| {
            let mut guessed = attr;
            guessed.size = attr.size.max(offset + u64::from(n));
            guessed
        });
        fill_file_info(file_info, context.ino, &new_attr);
        Ok(n)
    }

    fn read_directory(
        &self,
        context: &FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        match context.dir_buffer.acquire(marker.is_none(), None) {
            Ok(lock) => {
                let mut dirinfo = DirInfo::<255>::new();
                let path = self
                    .fs
                    .ino
                    .path_of(context.ino)
                    .ok_or_else(|| nt(STATUS_INVALID_HANDLE))?;

                // WinFsp does not synthesize "." / ".." for us; provide them
                // for every directory except the root (like ntptfs does).
                if let Some((parent_path, _)) = path.split() {
                    let cur = self.fs.getattr(context.ino).map_err(fsp_err)?;
                    self.write_dir_entry(&lock, &mut dirinfo, ".", context.ino, &cur)?;
                    if let Ok((pino, pattr)) = self.resolve(&parent_path) {
                        self.write_dir_entry(&lock, &mut dirinfo, "..", pino, &pattr)?;
                    }
                }

                for (name, ino, attr) in self.fs.readdir(context.ino).map_err(fsp_err)? {
                    self.write_dir_entry(&lock, &mut dirinfo, &name, ino, &attr)?;
                }
            }
            // FALSE + STATUS_SUCCESS from acquire means "already filled, just
            // serve from the buffer" — anything else is a real error.
            Err(e) if e.to_ntstatus() == 0 => {}
            Err(e) => return Err(e),
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        _context: &FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let from = rel_path(file_name)?;
        let to = rel_path(new_file_name)?;
        tracing::trace!(from = %from, to = %to, replace_if_exists, "rename");
        let (Some((from_parent, from_name)), Some((to_parent, to_name))) = (from.split(), to.split()) else {
            return Err(nt(STATUS_ACCESS_DENIED)); // renaming the root
        };
        let (from_ino, _) = self.resolve(&from_parent).map_err(fsp_err)?;
        let (to_ino, _) = self.resolve(&to_parent).map_err(fsp_err)?;
        self.fs
            .rename(from_ino, from_name, to_ino, to_name, replace_if_exists)
            .map_err(fsp_err)
    }

    fn set_basic_info(
        &self,
        context: &FileContext,
        file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let cur = self.fs.getattr(context.ino).map_err(fsp_err)?;

        // 0 / INVALID_FILE_ATTRIBUTES mean "leave attributes alone"; the only
        // bit we can persist is READONLY, mapped onto the unix write bits.
        let mut mode = None;
        if file_attributes != 0 && file_attributes != INVALID_FILE_ATTRIBUTES {
            let readonly = file_attributes & FILE_ATTRIBUTE_READONLY != 0;
            let new_mode = if readonly {
                cur.mode & !0o222
            } else {
                cur.mode | 0o200
            };
            if new_mode != cur.mode {
                mode = Some(new_mode);
            }
        }
        // 0 means "don't change" for times (and -1 means "disable implicit
        // updates", which we also treat as no change).
        let mtime =
            (last_write_time != 0 && last_write_time != u64::MAX).then(|| from_filetime(last_write_time));

        let attr = if mode.is_some() || mtime.is_some() {
            self.fs.setattr(context.ino, None, mtime, mode).map_err(fsp_err)?
        } else {
            cur
        };
        fill_file_info(file_info, context.ino, &attr);
        Ok(())
    }

    fn set_delete(
        &self,
        context: &FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        if delete_file && context.is_dir {
            let entries = self.fs.readdir(context.ino).map_err(fsp_err)?;
            if !entries.is_empty() {
                return Err(nt(STATUS_DIRECTORY_NOT_EMPTY));
            }
        }
        // Record intent only: actual removal happens in cleanup, keyed off the
        // FspCleanupDelete flag the FSD passes there.
        context.delete_pending.store(delete_file, Ordering::Relaxed);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let attr = if set_allocation_size {
            // Allocation-size changes only ever shrink the visible file; a
            // larger allocation is a reservation we don't track server-side.
            let cur = self.fs.getattr(context.ino).map_err(fsp_err)?;
            if new_size >= cur.size {
                cur
            } else {
                self.fs
                    .setattr(context.ino, Some(new_size), None, None)
                    .map_err(fsp_err)?
            }
        } else {
            self.fs
                .setattr(context.ino, Some(new_size), None, None)
                .map_err(fsp_err)?
        };
        fill_file_info(file_info, context.ino, &attr);
        Ok(())
    }

    // ----------------------------------------------------------- symlinks
    //
    // Windows models a symlink as a reparse point, and creates one in two
    // steps: CreateSymbolicLinkW makes a zero-length placeholder file, then
    // issues FSCTL_SET_REPARSE_POINT on it. So `set_reparse_point` below is
    // handed a file that already exists and has to turn it into a link.

    fn get_reparse_point(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        let target = self.fs.readlink(context.ino).map_err(fsp_err)?;
        let encoded = encode_symlink_reparse(&target);
        if encoded.len() > buffer.len() {
            return Err(nt(STATUS_BUFFER_TOO_SMALL));
        }
        buffer[..encoded.len()].copy_from_slice(&encoded);
        Ok(encoded.len() as u64)
    }

    /// Used by the driver's own reparse resolution, which has a name rather
    /// than an open handle.
    fn get_reparse_point_by_name(
        &self,
        file_name: &U16CStr,
        _is_directory: bool,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        let rel = rel_path(file_name)?;
        let (ino, attr) = self.resolve(&rel).map_err(fsp_err)?;
        if !matches!(attr.kind, FileKind::Symlink) {
            return Err(nt(STATUS_NOT_A_REPARSE_POINT));
        }
        let target = self.fs.readlink(ino).map_err(fsp_err)?;
        let encoded = encode_symlink_reparse(&target);
        if encoded.len() > buffer.len() {
            return Err(nt(STATUS_BUFFER_TOO_SMALL));
        }
        buffer[..encoded.len()].copy_from_slice(&encoded);
        Ok(encoded.len() as u64)
    }

    fn set_reparse_point(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        buffer: &[u8],
    ) -> winfsp::Result<()> {
        let target = decode_symlink_reparse(buffer).ok_or_else(|| {
            // Junctions and every other reparse tag land here. We model one
            // thing, and claiming otherwise would create files nothing can
            // resolve.
            tracing::warn!("unsupported reparse point (only symlinks are handled)");
            nt(STATUS_NOT_SUPPORTED)
        })?;

        let rel = rel_path(file_name)?;
        let (parent_rel, name) = rel.split().ok_or_else(|| nt(STATUS_OBJECT_NAME_INVALID))?;
        // The drive-letter-absolute target that Windows tooling writes is
        // rewritten by RemoteFs::symlink, which every backend goes through.
        let (parent_ino, _) = self.resolve(&parent_rel).map_err(fsp_err)?;

        // Replace the placeholder — but ONLY if it is one. A non-empty file
        // here means something other than CreateSymbolicLinkW put it there,
        // and turning a file with contents into a symlink would destroy data
        // to satisfy a request we could just refuse.
        match self.resolve(&rel) {
            Ok((_, attr)) => {
                if attr.size != 0 || matches!(attr.kind, FileKind::Dir) {
                    tracing::warn!(%rel, size = attr.size, "refusing to replace a non-empty file with a symlink");
                    return Err(nt(STATUS_NOT_SUPPORTED));
                }
                self.fs.unlink(parent_ino, name).map_err(fsp_err)?;
            }
            // Nothing there: fine, we are creating the link outright.
            Err(FsError::Remote(ErrorCode::NotFound)) => {}
            Err(e) => return Err(fsp_err(e)),
        }

        self.fs.symlink(parent_ino, name, &target).map_err(fsp_err)?;
        Ok(())
    }

    /// Removing the reparse point would leave a regular empty file where a
    /// symlink was. Nothing in this filesystem can represent that transition,
    /// and silently deleting the link instead would be a surprising answer to
    /// "clear this attribute".
    fn delete_reparse_point(
        &self,
        _context: &Self::FileContext,
        _file_name: &U16CStr,
        _buffer: &[u8],
    ) -> winfsp::Result<()> {
        Err(nt(STATUS_NOT_SUPPORTED))
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let (block_size, blocks, blocks_free) = self.fs.statfs().map_err(fsp_err)?;
        out_volume_info.total_size = u64::from(block_size) * blocks;
        out_volume_info.free_size = u64::from(block_size) * blocks_free;
        out_volume_info.set_volume_label(&self.volume_label);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Native re-emission: server events → FspFileSystemNotify → real
// ReadDirectoryChangesW notifications for apps watching the drive.
// ---------------------------------------------------------------------------

const FILE_ACTION_ADDED: u32 = 1;
const FILE_ACTION_REMOVED: u32 = 2;
const FILE_ACTION_MODIFIED: u32 = 3;
const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;
const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x01;
const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x02;
const FILE_NOTIFY_CHANGE_ATTRIBUTES: u32 = 0x04;
const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x08;
const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x10;

/// Where the client event pump drops batches for the notify timer to emit.
#[derive(Clone)]
pub struct EventSink(Arc<std::sync::Mutex<Vec<alloyfs_proto::FsEvent>>>);

impl EventSink {
    pub fn push(&self, batch: &[alloyfs_proto::FsEvent]) {
        // A poisoned mutex here just means a prior panic elsewhere; dropping
        // notifications forever over it would silently disable the feature.
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(batch);
    }
}

// ------------------------------------------------------- security descriptors

/// The descriptor every file and directory on the mount reports.
///
/// SYSTEM, Administrators and Everyone, full access, protected (`P`) so
/// nothing is inherited from wherever the drive letter hangs.
///
/// This is deliberate rather than lax. **The server is the access boundary**:
/// an export's `read_only` flag and the serving machine's own filesystem
/// permissions decide what a client may do, and they are enforced on the far
/// side of the wire where the files actually live. Windows-side ACLs on top of
/// that would protect nothing — anyone who can read this drive can equally run
/// `alloyfs mount` themselves and get their own copy of it.
///
/// It also has to be explicit rather than absent. Returning no descriptor
/// makes WinFsp synthesize one from whoever mounted, which is fine when that
/// is the user at the keyboard and wrong the moment anything else does the
/// mounting — the reason `alloyfs service` goes to the trouble of launching
/// into the interactive session instead of mounting from the service.
const MOUNT_SDDL: &str = "O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)";

/// The parsed descriptor and its length, built once.
struct MountSecurity {
    descriptor: Vec<u8>,
}

// The pointer from ConvertStringSecurityDescriptorToSecurityDescriptorW is
// copied into an owned Vec immediately, so what is cached is plain bytes.
static MOUNT_SECURITY: std::sync::OnceLock<MountSecurity> = std::sync::OnceLock::new();

fn mount_security() -> &'static MountSecurity {
    MOUNT_SECURITY.get_or_init(|| {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{GetSecurityDescriptorLength, PSECURITY_DESCRIPTOR};

        let wide: Vec<u16> = MOUNT_SDDL.encode_utf16().chain(Some(0)).collect();
        let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                std::ptr::null_mut(),
            )
        };
        // A compile-time constant SDDL either parses on every machine or on
        // none, so a failure here is a bug in this file rather than something
        // to degrade around.
        assert!(
            ok != 0 && !psd.is_null(),
            "the built-in mount SDDL failed to parse: {}",
            std::io::Error::last_os_error()
        );
        let len = unsafe { GetSecurityDescriptorLength(psd) } as usize;
        let descriptor = unsafe { std::slice::from_raw_parts(psd as *const u8, len) }.to_vec();
        unsafe { LocalFree(psd as *mut _) };
        MountSecurity { descriptor }
    })
}

/// Copy the mount descriptor into WinFsp's buffer, returning its length.
///
/// WinFsp asks twice: once with no buffer to learn the size, then again with
/// one big enough. A buffer that is too small is not an error either — it is
/// the same "here is the size you need" answer, so this reports the length in
/// every case and only copies when there is room.
fn write_security_descriptor(buffer: Option<&mut [std::ffi::c_void]>) -> winfsp::Result<u64> {
    let security = mount_security();
    let len = security.descriptor.len();
    if let Some(buffer) = buffer {
        if buffer.len() >= len {
            // SAFETY: the destination is at least `len` bytes, and a security
            // descriptor is a plain byte blob to everyone but the SRM.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    security.descriptor.as_ptr(),
                    buffer.as_mut_ptr() as *mut u8,
                    len,
                );
            }
        }
    }
    Ok(len as u64)
}

/// Volume-rooted wide path (`\dir\file.txt`), no NUL terminator.
fn notify_name(rel: &alloyfs_proto::RelPath) -> Vec<u16> {
    let mut s = String::with_capacity(rel.0.len() + 1);
    s.push('\\');
    s.push_str(&rel.0.replace('/', "\\"));
    s.encode_utf16().collect()
}

fn emit_one(notifier: &winfsp::notify::Notifier, path: &alloyfs_proto::RelPath, action: u32, filter: u32) {
    let mut info = winfsp::notify::NotifyInfo::<255>::new();
    info.action = action;
    info.filter = filter;
    // Same upstream quirk as DirInfo: set_name would count a NUL terminator
    // into the length, so pass the exact wide slice ourselves.
    let wide = notify_name(path);
    if info.set_name_raw(wide.as_slice()).is_err() {
        return; // name too long for the notify buffer: skip, don't fail
    }
    notifier.notify(&info);
}

impl winfsp::notify::NotifyingFileSystemContext<Vec<alloyfs_proto::FsEvent>> for WinFspFs {
    fn should_notify(&self) -> Option<Vec<alloyfs_proto::FsEvent>> {
        let mut guard = self.pending_events.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *guard))
        }
    }

    fn notify(&self, events: Vec<alloyfs_proto::FsEvent>, notifier: &winfsp::notify::Notifier) {
        use alloyfs_proto::EventKind;
        // We can't cheaply know file-vs-dir for remote paths, so name-change
        // notifications carry both name filters; over-notifying only costs a
        // spurious refresh, under-notifying costs correctness.
        const NAME_BOTH: u32 = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME;
        for ev in &events {
            match &ev.kind {
                EventKind::Created => emit_one(notifier, &ev.path, FILE_ACTION_ADDED, NAME_BOTH),
                EventKind::Removed => emit_one(notifier, &ev.path, FILE_ACTION_REMOVED, NAME_BOTH),
                EventKind::Modified => emit_one(
                    notifier,
                    &ev.path,
                    FILE_ACTION_MODIFIED,
                    FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE,
                ),
                EventKind::AttrChanged => emit_one(
                    notifier,
                    &ev.path,
                    FILE_ACTION_MODIFIED,
                    FILE_NOTIFY_CHANGE_ATTRIBUTES,
                ),
                EventKind::RenamedFrom { to } => {
                    emit_one(notifier, &ev.path, FILE_ACTION_RENAMED_OLD_NAME, NAME_BOTH);
                    emit_one(notifier, to, FILE_ACTION_RENAMED_NEW_NAME, NAME_BOTH);
                }
                EventKind::ResyncRequired => {}
            }
        }
        tracing::debug!(n = events.len(), "re-emitted native change notifications");
    }
}

/// A mounted drive. Dropping it unmounts and stops the WinFsp dispatcher.
pub struct MountedDrive {
    host: FileSystemHost<WinFspFs>,
    sink: EventSink,
}

impl MountedDrive {
    /// The queue the event pump should feed; drained by the notify timer.
    pub fn event_sink(&self) -> EventSink {
        self.sink.clone()
    }

    /// Cleanly unmount and stop the filesystem host.
    pub fn unmount(mut self) {
        self.host.unmount();
        self.host.stop();
        // FileSystemHost::drop finishes teardown (delete + free); both calls
        // above are idempotent.
    }
}

/// Mount `fs` at `mountpoint` (a drive letter like "X:" or a directory path)
/// with the given volume label. Returns as soon as the WinFsp dispatcher is
/// running; the caller decides when to `unmount` (alloyfs-cli waits for Ctrl-C).
///
/// The tokio runtime that `fs` was attached on must stay alive while mounted:
/// every callback parks a WinFsp dispatcher thread on it via `block_on`.
pub fn mount(fs: Arc<RemoteFs>, mountpoint: &str, volume_label: &str) -> anyhow::Result<MountedDrive> {
    winfsp::winfsp_init()
        .map_err(|e| anyhow::anyhow!("WinFsp is not available (is it installed?): {}", e))?;

    let mut params = VolumeParams::new();
    params
        // Deliberately "NTFS": picky applications (bun among them) probe the
        // filesystem name and fail on anything else. WinFsp's own guidance is
        // to report NTFS for compatibility; our real identity stays visible
        // in the volume label and mtab-equivalents.
        .filesystem_name("NTFS")
        .sector_size(ALLOCATION_UNIT as u16)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .case_sensitive_search(true)
        .case_preserved_names(true)
        // Symlinks. `no_reparse_points_dir_check` is what winfsp documents as
        // needed for FUSE-shaped filesystems: without it the driver refuses to
        // open a reparse point unless the caller's FILE_DIRECTORY_FILE /
        // FILE_NON_DIRECTORY_FILE guess matches, and a link's target kind is
        // not something we know at open time.
        .reparse_points(true)
        .no_reparse_points_dir_check(true)
        .unicode_on_disk(true)
        .volume_creation_time(to_filetime(SystemTime::now()))
        .volume_serial_number(0x4453_594E) // "DSYN"
        // Short kernel-side metadata cache, mirroring the FUSE backend's 1s
        // TTL; real invalidation arrives with the event stream (the notify
        // timer re-emits pump batches as ReadDirectoryChangesW).
        .file_info_timeout(1000)
        // Only post Cleanup when something actually changed (or a delete is
        // pending) — saves a callback storm on read-only workloads.
        .post_cleanup_when_modified_only(true)
        // POSIX-semantics unlink/rename (replace-on-rename, unlink-while-open).
        // Modern tooling (bun's atomic lockfile replace, Node 20+, git) uses
        // FILE_RENAME_POSIX_SEMANTICS; without this flag WinFsp rejects those
        // with EINVAL. Our server is POSIX underneath, so semantics match.
        .supports_posix_unlink_rename(true);
    // Volume prefix stays empty: this is a "disk" filesystem, so plain drive
    // letter mounts work without the network-provider machinery.

    let pending = Arc::new(std::sync::Mutex::new(Vec::new()));
    let context = WinFspFs {
        fs,
        volume_label: volume_label.to_string(),
        pending_events: pending.clone(),
    };
    // new_with_timer: a 100 ms poll drains pending_events into
    // FspFileSystemNotify, which fans out to ReadDirectoryChangesW watchers.
    let mut host = FileSystemHost::<WinFspFs>::new_with_timer::<Vec<alloyfs_proto::FsEvent>, 100>(
        winfsp::host::FileSystemParams::default_params(params),
        context,
    )
    .map_err(|e| anyhow::anyhow!("creating WinFsp filesystem failed: {e}"))?;
    host.start()
        .map_err(|e| anyhow::anyhow!("starting WinFsp dispatcher failed: {e}"))?;
    // Drive letters: register with the Windows Mount Manager when possible
    // (mountpoint "\\.\X:"). Session-local DOS-device mounts (plain "X:")
    // break GetFinalPathNameByHandle round-trips, which is exactly why bun
    // (and friends) die with ENOENT on rclone-style drives. Mount Manager
    // registration needs Administrator, so fall back with a warning.
    let is_drive_letter =
        mountpoint.len() == 2 && mountpoint.ends_with(':') && mountpoint.as_bytes()[0].is_ascii_alphabetic();
    let mut mounted_via_mountmgr = false;
    if is_drive_letter {
        let mm = format!("\\\\.\\{mountpoint}");
        match host.mount(mm.as_str()) {
            Ok(()) => mounted_via_mountmgr = true,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Mount Manager registration failed (needs Administrator); \
                     falling back to a session drive. Note: some tools (bun) \
                     mis-handle session drives — run elevated if you hit \
                     spurious ENOENT errors"
                );
            }
        }
    }
    if !mounted_via_mountmgr {
        host.mount(mountpoint)
            .map_err(|e| anyhow::anyhow!("mounting at {mountpoint} failed: {e}"))?;
    }
    tracing::info!(
        mountpoint,
        mountmgr = mounted_via_mountmgr,
        "WinFsp volume mounted"
    );
    Ok(MountedDrive {
        host,
        sink: EventSink(pending),
    })
}

#[cfg(test)]
mod tests {
    //! The translation layer only.
    //!
    //! Anything past this needs the WinFsp driver and a real mount, so these
    //! cover the pure conversions between the wire's view of a file and
    //! Windows's — which is exactly where this backend's bugs have been. The
    //! NUL-length defect that produced `docs/upstream/winfsp-rs-set-name-nul-length.md`
    //! lived in this layer.

    use super::*;

    fn attr(kind: FileKind, mode: u32, size: u64) -> Attr {
        Attr {
            kind,
            size,
            mtime: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            ctime: UNIX_EPOCH + Duration::from_secs(1_600_000_000),
            mode,
            version: 3,
        }
    }

    #[test]
    fn paths_are_normalized_from_windows_to_the_wire() {
        let cases: &[(&str, &str)] = &[
            (r"\", ""),
            (r"\one.txt", "one.txt"),
            (r"\dir\sub\file.txt", "dir/sub/file.txt"),
            (r"\trailing\", "trailing"),
            ("", ""),
        ];
        for (win, want) in cases {
            let wide = winfsp::U16CString::from_str(win).unwrap();
            let got = rel_path(&wide).expect("valid name");
            assert_eq!(got.0, *want, "{win:?} normalized wrong");
        }
    }

    /// A directory is a directory; everything else is a file, and the write
    /// bit is the only thing that makes it read-only. Symlinks arrive already
    /// resolved, so they must not be marked as reparse points here.
    #[test]
    fn attribute_bits_follow_kind_and_the_write_bit() {
        assert_eq!(
            attributes(&attr(FileKind::Dir, 0o755, 0)),
            FILE_ATTRIBUTE_DIRECTORY
        );
        let writable = attributes(&attr(FileKind::File, 0o644, 10));
        assert_eq!(writable & FILE_ATTRIBUTE_READONLY, 0, "0644 is writable");
        assert_ne!(writable & FILE_ATTRIBUTE_ARCHIVE, 0);

        let readonly = attributes(&attr(FileKind::File, 0o444, 10));
        assert_ne!(readonly & FILE_ATTRIBUTE_READONLY, 0, "0444 has no write bit");

        // A symlink is NOT just a file any more: it carries the reparse bit,
        // which is what makes Windows ask for the target instead of treating
        // the link as an empty file. (It also drops the read-only bit — the
        // mode belongs to whatever the link points at, not to the link.)
        let link = attributes(&attr(FileKind::Symlink, 0o644, 10));
        assert_ne!(link, writable);
        assert_ne!(link & FILE_ATTRIBUTE_REPARSE_POINT, 0);
    }

    /// Round-tripping matters because the two directions are used on opposite
    /// sides of a `set_basic_info`: a client that reads a time and writes it
    /// back unchanged must not drift the file's timestamp.
    #[test]
    fn filetime_round_trips_to_the_second() {
        for secs in [0u64, 1, 1_000_000_000, 1_700_000_000, 2_000_000_000] {
            let t = UNIX_EPOCH + Duration::from_secs(secs);
            let back = from_filetime(to_filetime(t));
            assert_eq!(back, t, "{secs} did not survive the round trip");
        }
    }

    /// Windows counts from 1601 and Unix from 1970; getting this backwards
    /// puts every file's date centuries out.
    #[test]
    fn the_unix_epoch_is_the_windows_epoch_offset() {
        assert_eq!(to_filetime(UNIX_EPOCH), FILETIME_UNIX_DIFF_SECS * 10_000_000);
        assert_eq!(
            from_filetime(0),
            UNIX_EPOCH,
            "pre-epoch clamps rather than wrapping"
        );
    }

    #[test]
    fn file_info_reports_size_and_allocation() {
        let mut fi = FileInfo::default();
        fill_file_info(&mut fi, 42, &attr(FileKind::File, 0o644, 4097));
        assert_eq!(fi.index_number, 42);
        assert_eq!(fi.file_size, 4097);
        assert_eq!(
            fi.allocation_size,
            ALLOCATION_UNIT * 2,
            "allocation rounds up to whole units"
        );
        assert_eq!(fi.reparse_tag, 0);
        // ctime is older than mtime here, so change_time must take the newer.
        assert_eq!(fi.change_time, fi.last_write_time);
    }

    #[test]
    fn a_symlink_carries_the_reparse_bit_and_tag() {
        let a = attributes(&attr(FileKind::Symlink, 0o777, 0));
        assert_ne!(
            a & FILE_ATTRIBUTE_REPARSE_POINT,
            0,
            "without this bit Windows never asks for the reparse data"
        );
        let mut fi = FileInfo::default();
        fill_file_info(&mut fi, 5, &attr(FileKind::Symlink, 0o777, 0));
        assert_eq!(fi.reparse_tag, IO_REPARSE_TAG_SYMLINK);
        // A regular file must not claim to be one.
        let mut fi = FileInfo::default();
        fill_file_info(&mut fi, 5, &attr(FileKind::File, 0o644, 1));
        assert_eq!(fi.reparse_tag, 0);
        assert_eq!(
            attributes(&attr(FileKind::File, 0o644, 1)) & FILE_ATTRIBUTE_REPARSE_POINT,
            0
        );
    }

    #[test]
    fn symlink_reparse_buffers_round_trip() {
        for target in ["real.txt", "../up.txt", "sub/deep/file.txt", "a b c.txt"] {
            let buf = encode_symlink_reparse(target);
            let back = decode_symlink_reparse(&buf).expect("decodes");
            assert_eq!(back, target, "{target:?} did not survive the round trip");
        }
    }

    /// A relative target must set SYMLINK_FLAG_RELATIVE or the shell resolves
    /// it from the volume root instead of the link's own directory.
    #[test]
    fn the_relative_flag_tracks_the_target() {
        let rel = encode_symlink_reparse("../up.txt");
        let flags = u32::from_le_bytes(rel[16..20].try_into().unwrap());
        assert_eq!(flags, SYMLINK_FLAG_RELATIVE);

        for absolute in ["\\\\server\\share", "C:\\Windows"] {
            let abs = encode_symlink_reparse(absolute);
            let flags = u32::from_le_bytes(abs[16..20].try_into().unwrap());
            assert_eq!(flags, 0, "{absolute:?} should not be marked relative");
        }
    }

    /// The buffer arrives from userspace through an FSCTL, so every offset in
    /// it is attacker-controlled. None of these may panic.
    #[test]
    fn malformed_reparse_buffers_are_refused() {
        assert!(decode_symlink_reparse(&[]).is_none());
        assert!(decode_symlink_reparse(&[0u8; 4]).is_none());
        // Right shape, wrong tag (a junction).
        let mut junction = encode_symlink_reparse("x.txt");
        junction[0..4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
        assert!(decode_symlink_reparse(&junction).is_none());
        // A length that runs past the end of the buffer.
        let mut lying = encode_symlink_reparse("x.txt");
        lying[10..12].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert!(decode_symlink_reparse(&lying).is_none());
        // An odd byte length cannot be UTF-16.
        let mut odd = encode_symlink_reparse("x.txt");
        odd[10..12].copy_from_slice(&3u16.to_le_bytes());
        assert!(decode_symlink_reparse(&odd).is_none());
        // Zero-length target.
        let mut empty = encode_symlink_reparse("x.txt");
        empty[10..12].copy_from_slice(&0u16.to_le_bytes());
        assert!(decode_symlink_reparse(&empty).is_none());
    }

    /// NTFS stores substitute names in `\??\` form; the server has no idea
    /// what that means, so it must be stripped on the way in.
    #[test]
    fn an_nt_prefixed_substitute_name_is_stripped() {
        let buf = encode_symlink_reparse("\\??\\C:\\real.txt");
        assert_eq!(decode_symlink_reparse(&buf).unwrap(), "C:/real.txt");
    }

    #[test]
    fn an_empty_file_allocates_nothing() {
        let mut fi = FileInfo::default();
        fill_file_info(&mut fi, 1, &attr(FileKind::File, 0o644, 0));
        assert_eq!(fi.file_size, 0);
        assert_eq!(fi.allocation_size, 0);
    }
}
