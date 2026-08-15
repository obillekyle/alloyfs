//! The translation layer only.
//!
//! Everything past this needs a real FUSE mount — a kernel, `/dev/fuse`, and
//! usually root — so these cover the pure conversions between the kernel's
//! view and the wire's. That is where a mistake is quiet rather than loud: a
//! wrong errno turns "the server said no" into a filesystem that looks broken,
//! and a wrong open flag silently drops O_APPEND or O_EXCL.
//!
//! The ErrorCode → errno table itself is tested in `alloyfs_client::error`,
//! where the function lives. It was tested here for as long as this was the
//! only backend calling it, and that outlived its usefulness: this crate is
//! `#![cfg(unix)]`, so a Windows `cargo test` compiled it to nothing and the
//! table went unchecked until CI. Only the wrapping is this crate's business.

use super::*;
use alloyfs_proto::ErrorCode;

#[test]
fn kinds_map_to_the_kernel_file_types() {
    assert_eq!(file_type(FileKind::File), FileType::RegularFile);
    assert_eq!(file_type(FileKind::Dir), FileType::Directory);
    assert_eq!(file_type(FileKind::Symlink), FileType::Symlink);
}

/// `Errno::from_i32` is the only thing this crate adds to the shared table, so
/// it is the only thing worth asserting here: one mapped code and one
/// fall-through, to catch the wrapper being wired to the wrong function or
/// silently swallowing a value.
#[test]
fn the_shared_errno_table_reaches_fuse_intact() {
    assert_eq!(
        errno(&FsError::Remote(ErrorCode::NotFound)).code(),
        Errno::ENOENT.code()
    );
    assert_eq!(errno(&FsError::Remote(ErrorCode::Io)).code(), Errno::EIO.code());
    assert_eq!(
        errno(&FsError::Remote(ErrorCode::NotFound)).code(),
        alloyfs_client::posix_errno(&FsError::Remote(ErrorCode::NotFound))
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
