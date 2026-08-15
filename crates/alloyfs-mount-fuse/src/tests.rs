//! The translation layer only.
//!
//! Everything past this needs a real FUSE mount — a kernel, `/dev/fuse`, and
//! usually root — so these cover the pure conversions between the kernel's
//! view and the wire's. That is where a mistake is quiet rather than loud: a
//! wrong errno turns "the server said no" into a filesystem that looks broken,
//! and a wrong open flag silently drops O_APPEND or O_EXCL.

use super::*;
use alloyfs_proto::ErrorCode;

#[test]
fn kinds_map_to_the_kernel_file_types() {
    assert_eq!(file_type(FileKind::File), FileType::RegularFile);
    assert_eq!(file_type(FileKind::Dir), FileType::Directory);
    assert_eq!(file_type(FileKind::Symlink), FileType::Symlink);
}

/// Every wire error must reach userspace as the errno an application expects
/// to branch on. `EIO` for everything would compile and pass a smoke test,
/// and would make `mkdir` on an existing directory indistinguishable from a
/// disk failure.
#[test]
fn wire_errors_become_the_matching_errno() {
    let cases: &[(ErrorCode, Errno)] = &[
        (ErrorCode::NotFound, Errno::ENOENT),
        (ErrorCode::PermissionDenied, Errno::EACCES),
        (ErrorCode::AlreadyExists, Errno::EEXIST),
        (ErrorCode::NotADirectory, Errno::ENOTDIR),
        (ErrorCode::IsADirectory, Errno::EISDIR),
        (ErrorCode::NotEmpty, Errno::ENOTEMPTY),
        (ErrorCode::InvalidPath, Errno::EINVAL),
        (ErrorCode::BadHandle, Errno::EBADF),
        (ErrorCode::ReadOnly, Errno::EROFS),
        (ErrorCode::WouldBlock, Errno::EAGAIN),
        (ErrorCode::CrossDevice, Errno::EXDEV),
    ];
    for (code, want) in cases {
        assert_eq!(
            errno(&FsError::Remote(*code)).code(),
            want.code(),
            "{code:?} mapped wrong"
        );
    }
}

/// Codes with no specific mapping fall through to EIO rather than to
/// something plausible-but-wrong. The operation may or may not have happened,
/// and EIO is the only honest answer to that.
#[test]
fn unmapped_codes_fall_through_to_eio() {
    for code in [ErrorCode::Io, ErrorCode::Conflict, ErrorCode::NotAttached] {
        assert_eq!(
            errno(&FsError::Remote(code)).code(),
            Errno::EIO.code(),
            "{code:?}"
        );
    }
}

/// "The server is too old for this operation" is not an I/O error, and
/// reporting it as one sends people looking at the wrong layer. It did
/// exactly that once, during the first live symlink attempt.
#[test]
fn an_old_server_is_not_an_io_error() {
    assert_eq!(
        errno(&FsError::Remote(ErrorCode::VersionMismatch)).code(),
        Errno::EOPNOTSUPP.code()
    );
}

fn flags(raw: i32) -> FuseOpenFlags {
    FuseOpenFlags(raw)
}

#[test]
fn access_modes_convert() {
    let ro = DsFuse::open_flags(flags(libc::O_RDONLY));
    assert!(ro.read && !ro.write);

    let wo = DsFuse::open_flags(flags(libc::O_WRONLY));
    assert!(!wo.read && wo.write);

    let rw = DsFuse::open_flags(flags(libc::O_RDWR));
    assert!(rw.read && rw.write);
}

/// The modifier bits ride alongside the access mode, and dropping one is
/// silent: losing O_EXCL turns an atomic create into a clobber, and losing
/// O_APPEND turns a log write into an overwrite.
#[test]
fn modifier_flags_are_not_dropped() {
    let f = DsFuse::open_flags(flags(libc::O_RDWR | libc::O_TRUNC));
    assert!(f.truncate && !f.append && !f.excl);

    let f = DsFuse::open_flags(flags(libc::O_WRONLY | libc::O_APPEND));
    assert!(f.append && !f.truncate);

    let f = DsFuse::open_flags(flags(libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL));
    assert!(f.excl);

    let all = DsFuse::open_flags(flags(
        libc::O_RDWR | libc::O_TRUNC | libc::O_APPEND | libc::O_EXCL,
    ));
    assert!(all.read && all.write && all.truncate && all.append && all.excl);

    let none = DsFuse::open_flags(flags(libc::O_RDONLY));
    assert!(!none.truncate && !none.append && !none.excl);
}
