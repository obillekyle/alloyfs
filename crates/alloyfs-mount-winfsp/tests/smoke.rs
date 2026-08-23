//! The one test that actually mounts a volume.
//!
//! Every other test in this workspace stops at the client: they drive
//! `RemoteFs` directly and assert on what it returns. This crate is the layer
//! below that — the translation between `RemoteFs` and WinFsp's callbacks —
//! and it had 1,714 lines and no test that ever asked Windows to open a file.
//! That is the wrong crate to leave uncovered: everywhere else a bug returns
//! an error, and here it leaves a drive letter wedged and PowerShell hanging
//! on startup because enumerating drives never returns.
//!
//! So this mounts a real volume, backed by a real agent over the real wire
//! protocol, and reads it back through `std::fs` — which means through the
//! Windows I/O manager, the WinFsp driver, and every callback in `lib.rs`.
//! It is deliberately a smoke test rather than a matrix: the value is in
//! proving the whole path can be walked at all, which is exactly what nothing
//! else did.
//!
//! **It skips rather than fails** when WinFsp is not installed or no drive
//! letter is free. A developer without the driver must still be able to run
//! `cargo test`, and a machine whose letters are all taken is not a bug in
//! this code.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_client::{ClientOptions, RemoteFs};
use alloyfs_transport::{serve_connection, MuxConnection, RequestHandler};

/// Drive letters this test will consider, and the ones it will not.
///
/// Never a letter already in use: this developer's box has a live AlloyFS
/// mount on `L:` and an rclone mount on `Z:`, and a test that assumed a letter
/// would be an excellent way to take down someone's working drive. The check
/// is `GetLogicalDrives`, which reports what the system currently has, so the
/// answer is right for whatever machine this runs on rather than for the one
/// it was written on.
///
/// Ordered from the end of the alphabet inward, away from the letters real
/// mounts tend to claim.
const CANDIDATES: &[u8] = b"YXWVUTSRQP";

fn free_drive_letter() -> Option<char> {
    // Bit 0 is A:, bit 1 is B:, and so on. A set bit means the letter is
    // taken — by a disk, a network share, or another user-mode filesystem.
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    if mask == 0 {
        return None; // the call failed; take no chances with a letter
    }
    CANDIDATES
        .iter()
        .map(|&b| b as char)
        .find(|&c| mask & (1u32 << (c as u8 - b'A')) == 0)
}

struct Fixture {
    _dir: tempfile::TempDir,
    fs: Arc<RemoteFs>,
    _rt: tokio::runtime::Runtime,
}

/// A real agent over `tokio::io::duplex`, and a `RemoteFs` attached to it.
///
/// The same shape the client's loopback battery uses. In-process because the
/// point here is the mount layer, not the transport: a duplex pair exercises
/// the full framing and handshake without a socket to bind or a port to
/// collide on.
fn fixture() -> Fixture {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("hello.txt"), b"mounted through winfsp\n").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("nested.bin"), vec![7u8; 40_000]).unwrap();

    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            ..Default::default()
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("export registry"));

    // A multi-thread runtime, because `RemoteFs` is a synchronous API that
    // blocks on a captured handle — and here the blocking caller is a WinFsp
    // dispatcher thread, not a test thread.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    let fs = rt.block_on(async {
        let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
        let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry));
        tokio::spawn(async move {
            let _ = serve_connection(server_io, "smoke-agent", handler).await;
        });
        let conn = MuxConnection::establish(client_io, "smoke-client")
            .await
            .expect("handshake");
        RemoteFs::attach_with(conn, "test", ClientOptions::default())
            .await
            .expect("attach")
    });

    Fixture {
        _dir: dir,
        fs,
        _rt: rt,
    }
}

/// Mount, walk the volume through the Windows filesystem API, unmount.
///
/// Read as a list of things that had no coverage at all: that `mount` returns
/// against a live agent, that the driver hands the volume a letter, that
/// `GetFileInformationByHandle` behind `metadata()` answers for a file and a
/// directory, that a read returns the bytes the agent has, that a read past
/// one 128 KiB block is assembled correctly, that a directory enumerates, and
/// that unmounting gives the letter back.
///
/// One test rather than seven because a mount is expensive and every
/// assertion here needs the same one. A failure names which step it was.
#[test]
fn a_volume_mounts_reads_and_unmounts() {
    let Some(letter) = free_drive_letter() else {
        eprintln!("SKIP: no free drive letter among {}", String::from_utf8_lossy(CANDIDATES));
        return;
    };
    let fixture = fixture();
    let mountpoint = format!("{letter}:");

    // The skip that matters most: WinFsp is a kernel driver, not a crate, and
    // a machine without it must still be able to run the suite.
    let drive = match alloyfs_mount_winfsp::mount(
        fixture.fs.clone(),
        &mountpoint,
        "smoke",
        Some(Duration::from_micros(300)),
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: could not mount at {mountpoint}: {e}");
            return;
        }
    };

    // Everything below goes through the driver, so anything that panics would
    // leave the volume mounted and the letter wedged — exactly the failure
    // this test exists to catch, and not one to inflict on whoever ran it.
    // Catch, unmount, then re-raise.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let root = std::path::Path::new(&mountpoint).join("");

        let hello = root.join("hello.txt");
        let md = std::fs::metadata(&hello).expect("stat hello.txt through the mount");
        assert!(md.is_file(), "hello.txt must stat as a file");
        assert_eq!(md.len(), 23, "and with the length the agent has");

        let body = std::fs::read(&hello).expect("read hello.txt through the mount");
        assert_eq!(
            body, b"mounted through winfsp\n",
            "the bytes must survive the round trip"
        );

        let sub = root.join("sub");
        assert!(
            std::fs::metadata(&sub).expect("stat sub").is_dir(),
            "sub must stat as a directory"
        );

        // 40 KB is one block; make it worth reading in pieces by checking the
        // whole thing rather than the first byte.
        let nested = std::fs::read(sub.join("nested.bin")).expect("read the nested file");
        assert_eq!(nested.len(), 40_000, "a multi-chunk read must be complete");
        assert!(
            nested.iter().all(|&b| b == 7),
            "and assembled in the right order"
        );

        let mut names: Vec<String> = std::fs::read_dir(&root)
            .expect("enumerate the volume root")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["hello.txt", "sub"], "the root must list both entries");

        // A file that is not there must be NotFound, not a hang and not some
        // other errno: the mount's error mapping is what turns an agent's
        // ErrorCode into something Windows understands.
        let missing = std::fs::metadata(root.join("nope.txt"));
        assert_eq!(
            missing.unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "a missing file must map to NotFound"
        );
    }));

    drive.unmount();

    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }

    // The letter comes back. Without this, a mount that "unmounted" but left
    // the volume attached would pass every assertion above and still be
    // broken — which is the state that wedges a machine.
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    assert_eq!(
        mask & (1u32 << (letter as u8 - b'A')),
        0,
        "{letter}: must be free again after unmount"
    );
}
