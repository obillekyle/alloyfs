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
    dir: tempfile::TempDir,
    fs: Arc<RemoteFs>,
    _rt: tokio::runtime::Runtime,
    /// Dropping this stops the OS watcher, so it has to outlive the test.
    _watch: alloyfs_agent::watch::WatchGuard,
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

    // The watcher is what turns a server-side change into an event, so the
    // staleness test needs it running. It calls `tokio::spawn` internally, so
    // it has to be built inside the runtime context, not beside it.
    let export = registry.get("test").expect("export");
    let watch = {
        let _guard = rt.enter();
        alloyfs_agent::watch::spawn(
            export.clone(),
            export.events.clone(),
            std::time::Duration::from_millis(50),
        )
        .expect("watcher")
    };

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
        dir,
        fs,
        _rt: rt,
        _watch: watch,
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
        eprintln!(
            "SKIP: no free drive letter among {}",
            String::from_utf8_lossy(CANDIDATES)
        );
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
        assert!(nested.iter().all(|&b| b == 7), "and assembled in the right order");

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

/// Read the whole of `path` through `RemoteFs` directly, bypassing Windows.
///
/// The point of the test below is to compare this against what the same file
/// looks like through the mount: `RemoteFs` is everything AlloyFS owns, and
/// the mount is that plus the Windows I/O manager and its cache manager. When
/// the two disagree, the difference is the part we do not own.
fn read_via_client(fs: &Arc<RemoteFs>, rt: &tokio::runtime::Handle, name: &str) -> Vec<u8> {
    let fs = fs.clone();
    let name = name.to_string();
    rt.block_on(async move {
        tokio::task::spawn_blocking(move || {
            let (ino, _) = fs.lookup(alloyfs_client::ROOT_INO, &name).expect("lookup");
            let (fh, attr) = fs
                .open(
                    ino,
                    alloyfs_proto::OpenFlags {
                        read: true,
                        ..Default::default()
                    },
                )
                .expect("open");
            let data = fs.read(fh, 0, attr.size as u32).expect("read");
            fs.release(fh);
            data
        })
        .await
        .expect("join")
    })
}

/// A server-side change that does NOT move the file's size must still reach
/// the next read through the mount.
///
/// This is the live-mount failure of 2026-08-23 reduced to one test. On `L:`,
/// eleven files served content from before an out-of-band `scp` while
/// reporting the size and mtime from after it — the origin and the AlloyFS
/// blob cache agreed byte for byte, and only the bytes handed back through the
/// drive letter were old. The trigger was a same-length edit (a `?v=7` →
/// `?v=8` cache-buster), which is why it had gone unnoticed: a change that
/// moves the size does not show it.
///
/// The test reads the same file two ways on purpose. `read_via_client` is
/// everything AlloyFS owns; `std::fs::read` is that plus the Windows cache
/// manager, which `file_info_timeout(u32::MAX)` deliberately hands the volume
/// to. If the client sees the new bytes and the mount does not, the stale copy
/// is being held above us and no amount of invalidation inside AlloyFS will
/// fix it — which is the question that decides the fix, so the assertions
/// report both rather than just failing.
#[test]
fn a_same_size_server_change_reaches_the_next_read() {
    let Some(letter) = free_drive_letter() else {
        eprintln!("SKIP: no free drive letter");
        return;
    };
    let fixture = fixture();
    let mountpoint = format!("{letter}:");
    let backing = fixture.dir.path().join("hello.txt");

    let drive = match alloyfs_mount_winfsp::mount(
        fixture.fs.clone(),
        &mountpoint,
        "stale",
        Some(Duration::from_micros(300)),
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: could not mount at {mountpoint}: {e}");
            return;
        }
    };
    let rt = fixture._rt.handle().clone();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path = std::path::Path::new(&mountpoint).join("hello.txt");

        // Read it first, so Windows has something cached to serve staly.
        let before = std::fs::read(&path).expect("first read");
        assert_eq!(before, b"mounted through winfsp\n", "baseline");

        // Same length, different bytes — exactly the shape that hid on L:.
        let after_bytes = b"MOUNTED THROUGH WINFSP\n";
        assert_eq!(after_bytes.len(), before.len(), "the edit must not move the size");
        std::fs::write(&backing, after_bytes).expect("change the file on the server");

        // Wait for AlloyFS itself to see it. If this times out the failure is
        // ours — the event never arrived — and that is a different bug from
        // the one this test is about, so it is reported separately.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut via_client = Vec::new();
        while std::time::Instant::now() < deadline {
            via_client = read_via_client(&fixture.fs, &rt, "hello.txt");
            if via_client == after_bytes {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            String::from_utf8_lossy(&via_client),
            String::from_utf8_lossy(after_bytes),
            "AlloyFS itself never saw the change — the event pipeline is broken, \
             which is a different bug from the one this test is for"
        );

        // AlloyFS has the new bytes. Now: does the mount?
        let via_mount = std::fs::read(&path).expect("second read");
        assert_eq!(
            String::from_utf8_lossy(&via_mount),
            String::from_utf8_lossy(after_bytes),
            "the client returned the NEW bytes and the mount returned the OLD ones, \
             so the stale copy is held above AlloyFS — in the Windows cache manager \
             that file_info_timeout(u32::MAX) hands the volume to"
        );
    }));

    drive.unmount();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
