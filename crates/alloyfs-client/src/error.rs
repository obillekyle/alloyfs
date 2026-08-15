//! The client's error type, and its translation to a Linux errno.
//!
//! Both live here rather than beside `RemoteFs` because neither needs it, and
//! because this crate compiles on every platform while the backends that
//! consume the errno table do not — see the note on the test module below.

use alloyfs_proto::ErrorCode;
use alloyfs_transport::TransportError;

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// The server answered with an error — maps 1:1 onto an errno/NTSTATUS.
    #[error(transparent)]
    Remote(#[from] ErrorCode),
    /// The connection itself failed — surfaces as EIO on the mount.
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
}

/// Linux errno for a filesystem error, shared by both Linux mount backends.
///
/// FUSE and the kernel-module daemon are two front-ends onto one platform, and
/// each used to carry its own copy of this table. They had already drifted:
/// the kernel backend mapped `NoSuchExport` to ENOENT while FUSE let it fall
/// through to EIO, and only FUSE had tests. Unified on ENOENT — "that export
/// does not exist" is a miss, not a device failure.
///
/// WinFsp keeps its own mapping, and correctly so: NTSTATUS is a genuinely
/// different target, not a second spelling of the same one.
pub fn posix_errno(err: &FsError) -> i32 {
    // Values rather than libc constants: this crate compiles on Windows too,
    // and the kernel daemon's abi module already spells them out for the same
    // reason.
    match err {
        FsError::Remote(code) => match code {
            ErrorCode::NotFound | ErrorCode::NoSuchExport => 2, // ENOENT
            ErrorCode::PermissionDenied => 13,                  // EACCES
            ErrorCode::AlreadyExists => 17,                     // EEXIST
            ErrorCode::NotADirectory => 20,                     // ENOTDIR
            ErrorCode::IsADirectory => 21,                      // EISDIR
            ErrorCode::NotEmpty => 39,                          // ENOTEMPTY
            ErrorCode::InvalidPath => 22,                       // EINVAL
            ErrorCode::BadHandle => 9,                          // EBADF
            ErrorCode::ReadOnly => 30,                          // EROFS
            ErrorCode::WouldBlock => 11,                        // EAGAIN
            ErrorCode::CrossDevice => 18,                       // EXDEV
            // An old server, not a broken disk.
            ErrorCode::VersionMismatch => 95, // EOPNOTSUPP
            _ => 5,                           // EIO
        },
        // The operation may or may not have happened; EIO is the honest answer.
        FsError::Transport(_) => 5,
    }
}

/// These tests used to live in `alloyfs-mount-fuse`, the first backend to call
/// this function. That crate is `#![cfg(unix)]` and compiles to an empty
/// library on Windows, so the table's only coverage was unreachable from a
/// Windows `cargo test` — while the table itself sat in this crate, which
/// builds everywhere. A regression took exactly that route: `VersionMismatch`
/// was left in the EIO fall-through, the local Windows run went green, and CI
/// on Linux was the first thing to notice.
///
/// The errnos are spelled as named constants rather than reusing the literals
/// above, so the test is a second opinion instead of an echo.
#[cfg(test)]
mod tests {
    use super::*;

    const ENOENT: i32 = 2;
    const EBADF: i32 = 9;
    const EAGAIN: i32 = 11;
    const EACCES: i32 = 13;
    const EEXIST: i32 = 17;
    const EXDEV: i32 = 18;
    const ENOTDIR: i32 = 20;
    const EISDIR: i32 = 21;
    const EINVAL: i32 = 22;
    const EROFS: i32 = 30;
    const ENOTEMPTY: i32 = 39;
    const EOPNOTSUPP: i32 = 95;
    const EIO: i32 = 5;

    /// Every wire error must reach userspace as the errno an application
    /// expects to branch on. `EIO` for everything would compile and pass a
    /// smoke test, and would make `mkdir` on an existing directory
    /// indistinguishable from a disk failure.
    #[test]
    fn wire_errors_become_the_matching_errno() {
        let cases: &[(ErrorCode, i32)] = &[
            (ErrorCode::NotFound, ENOENT),
            // Both spell "it is not there"; the kernel backend already did
            // this while FUSE reported a device failure.
            (ErrorCode::NoSuchExport, ENOENT),
            (ErrorCode::PermissionDenied, EACCES),
            (ErrorCode::AlreadyExists, EEXIST),
            (ErrorCode::NotADirectory, ENOTDIR),
            (ErrorCode::IsADirectory, EISDIR),
            (ErrorCode::NotEmpty, ENOTEMPTY),
            (ErrorCode::InvalidPath, EINVAL),
            (ErrorCode::BadHandle, EBADF),
            (ErrorCode::ReadOnly, EROFS),
            (ErrorCode::WouldBlock, EAGAIN),
            (ErrorCode::CrossDevice, EXDEV),
        ];
        for (code, want) in cases {
            assert_eq!(
                posix_errno(&FsError::Remote(*code)),
                *want,
                "{code:?} mapped wrong"
            );
        }
    }

    /// Codes with no specific mapping fall through to EIO rather than to
    /// something plausible-but-wrong. The operation may or may not have
    /// happened, and EIO is the only honest answer to that.
    #[test]
    fn unmapped_codes_fall_through_to_eio() {
        for code in [ErrorCode::Io, ErrorCode::Conflict, ErrorCode::NotAttached] {
            assert_eq!(posix_errno(&FsError::Remote(code)), EIO, "{code:?}");
        }
    }

    /// "The server is too old for this operation" is not an I/O error, and
    /// reporting it as one sends people looking at the wrong layer. It did
    /// exactly that once, during the first live symlink attempt.
    #[test]
    fn an_old_server_is_not_an_io_error() {
        assert_eq!(
            posix_errno(&FsError::Remote(ErrorCode::VersionMismatch)),
            EOPNOTSUPP
        );
    }
}
