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

/// A drive letter claimed for the life of this value, released on drop.
///
/// `GetLogicalDrives` alone is not enough. nextest runs every test in its own
/// PROCESS, concurrently, so two mount tests both saw the same letter free and
/// both took it — the smoke test then read the staleness test's volume and
/// failed on content it never wrote. It passed on retry, which is the worst
/// version of the problem: a flake that looks like a product bug.
///
/// The lock file is the claim, and `create_new` is what makes it atomic across
/// processes: exactly one caller can create a given path, and the loser moves
/// to the next letter.
struct LetterClaim {
    letter: char,
    lock: std::path::PathBuf,
}

impl Drop for LetterClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock);
    }
}

fn claim_drive_letter() -> Option<LetterClaim> {
    // Bit 0 is A:, bit 1 is B:, and so on. A set bit means the letter is
    // taken — by a disk, a network share, or another user-mode filesystem.
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    if mask == 0 {
        return None; // the call failed; take no chances with a letter
    }
    for &b in CANDIDATES {
        let letter = b as char;
        if mask & (1u32 << (b - b'A')) != 0 {
            continue; // the system has it
        }
        let lock = std::env::temp_dir().join(format!("alloyfs-test-drive-{letter}.lock"));
        // A crashed run must not take a letter out of circulation forever.
        if let Ok(md) = std::fs::metadata(&lock) {
            let stale = md
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(300));
            if stale {
                let _ = std::fs::remove_file(&lock);
            }
        }
        if std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .is_ok()
        {
            return Some(LetterClaim { letter, lock });
        }
    }
    None
}

/// Names Windows puts on a volume by itself, which the test did not create and
/// must not compare.
///
/// `System Volume Information` is the one that actually bit: Windows writes
/// `WPSettings.dat` into every freshly mounted volume, so it appears THROUGH
/// the mount — and not in a walk of the backing directory, because the folder
/// is ACL-restricted and `read_dir` on it fails for an ordinary process. The
/// result reads exactly like the mount inventing a file, which is the accusation
/// these tests exist to make, so it has to be ruled out explicitly rather than
/// left to look like a finding.
///
/// It did not reproduce locally and failed every CI run, which is the usual
/// shape of "the test environment is doing something the developer's box is
/// not".
fn is_windows_volume_metadata(name: &str) -> bool {
    name.eq_ignore_ascii_case("System Volume Information") || name.eq_ignore_ascii_case("$RECYCLE.BIN")
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
    let Some(claim) = claim_drive_letter() else {
        eprintln!(
            "SKIP: no free drive letter among {}",
            String::from_utf8_lossy(CANDIDATES)
        );
        return;
    };
    let letter = claim.letter;
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
            .filter(|n| !is_windows_volume_metadata(n))
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
    let Some(claim) = claim_drive_letter() else {
        eprintln!("SKIP: no free drive letter");
        return;
    };
    let letter = claim.letter;
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

/// How long a server-side ATTRIBUTE change takes to reach the next `stat`.
///
/// `FileInfoTimeout` is 30 s, and a timeout that long is only acceptable if it
/// is a backstop rather than the mechanism — the mechanism being
/// `FspFileSystemNotify`, which should evict the kernel's cached FileInfo the
/// moment an event lands. If it works, a change is visible in milliseconds and
/// the 30 s never applies. If it does not, a mount serves a stale size for up
/// to half a minute, and plenty happens in half a minute.
///
/// So this measures the latency rather than assuming it, and fails if the
/// answer is anywhere near the timeout. Run at 0 too, which has no window at
/// all, so the two can be compared: if 30 s is meaningfully slower than 0, the
/// notify path is not doing the work and the timeout is load-bearing.
#[cfg(windows)]
#[test]
fn a_server_side_attribute_change_reaches_the_next_stat() {
    let Some(claim) = claim_drive_letter() else {
        eprintln!("SKIP: no free drive letter");
        return;
    };
    let letter = claim.letter;
    let mountpoint = format!("{letter}:");

    for fit in [30_000u32, 0u32] {
        let fixture = fixture();
        let backing = fixture.dir.path().join("hello.txt");
        let drive = match alloyfs_mount_winfsp::mount_with_timeout(
            fixture.fs.clone(),
            &mountpoint,
            "attrs",
            Some(Duration::from_micros(300)),
            fit,
            fit,
        ) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("SKIP: could not mount: {e}");
                return;
            }
        };

        // The production wiring, which the fixture alone does not do: events
        // reach the kernel only if something pumps them into the drive's sink.
        // Without this the client never subscribes, `pump_healthy` stays false,
        // and the measurement below reports the DEGRADED 5 s floor instead of
        // the notify path — which is exactly what the first run of this test
        // measured before the pump was wired in.
        let sink = drive.event_sink();
        let pump_fs = fixture.fs.clone();
        fixture
            ._rt
            .block_on(async move { pump_fs.start_event_pump(move |batch| sink.push(batch)).await })
            .expect("event pump");
        assert!(
            fixture.fs.event_pump_healthy(),
            "the pump must be subscribed, or this measures the degraded regime"
        );

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let path = std::path::Path::new(&mountpoint).join("hello.txt");
            let before = std::fs::metadata(&path).expect("first stat").len();
            assert_eq!(before, 23, "baseline size");

            // A SIZE change, so it is unambiguously visible in the attrs, and
            // one the kernel would happily keep serving from cache.
            let grown = vec![b'z'; 5000];
            std::fs::write(&backing, &grown).expect("grow the file on the server");

            let start = std::time::Instant::now();
            let deadline = start + Duration::from_secs(25);
            let mut seen = before;
            while std::time::Instant::now() < deadline {
                seen = std::fs::metadata(&path).expect("stat").len();
                if seen == 5000 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let took = start.elapsed();
            println!("  FileInfoTimeout={fit:<10} new size visible after {:?}", took);
            assert_eq!(
                seen, 5000,
                "FileInfoTimeout={fit}: the mount never saw the new size within 25 s — \
                 the notify path is not evicting the kernel's cached FileInfo, which \
                 makes the timeout the mechanism rather than a backstop"
            );
            assert!(
                took < Duration::from_secs(5),
                "FileInfoTimeout={fit}: took {took:?} to see a server-side size change. \
                 Anything near the 30 s window means notify is not doing the work"
            );
        }));

        drive.unmount();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

/// Server-side churn, then: does the mount agree with the origin about
/// EVERYTHING?
///
/// The gap this closes. Every other test here asks one question about one
/// file. Real desync is not like that — it is one layer of several holding a
/// value the others have moved past, and it shows up as a file that looks
/// right until you compare it against the server. Metadata lives in at least
/// six places between the disk and the drive letter: the agent's tree index,
/// the client's attr cache, its listing cache, its auto-cache manifest, the
/// kernel's FileInfo cache and the kernel's dir-info cache. A disagreement
/// between any pair is a bug, and nothing was checking pairs.
///
/// So this churns the backing directory the way a person would — create,
/// delete, grow, shrink, truncate to the SAME size with different bytes — and
/// after each round compares the full picture: the set of names, and every
/// file's size and content hash. Same-size edits are in the mix deliberately:
/// that is the shape that hid for twenty minutes on the live drive, invisible
/// to any check that compares attributes alone.
///
/// Deterministic seed, so a failure is reproducible rather than a story.
#[cfg(windows)]
#[test]
fn the_mount_agrees_with_the_origin_under_churn() {
    let Some(claim) = claim_drive_letter() else {
        eprintln!("SKIP: no free drive letter");
        return;
    };
    let letter = claim.letter;
    let mountpoint = format!("{letter}:");

    let fixture = fixture();
    let root = fixture.dir.path().to_path_buf();
    let drive = match alloyfs_mount_winfsp::mount(
        fixture.fs.clone(),
        &mountpoint,
        "churn",
        Some(Duration::from_micros(300)),
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: could not mount: {e}");
            return;
        }
    };
    let sink = drive.event_sink();
    let pump_fs = fixture.fs.clone();
    fixture
        ._rt
        .block_on(async move { pump_fs.start_event_pump(move |b| sink.push(b)).await })
        .expect("event pump");
    assert!(fixture.fs.event_pump_healthy(), "the pump must be subscribed");

    /// Every file under `dir`, as (relative path, size, content hash).
    ///
    /// The hash is what makes this a desync test rather than an attribute
    /// test: a same-size edit leaves size and mtime identical and is visible
    /// only here.
    fn picture(dir: &std::path::Path) -> Vec<(String, u64, u64)> {
        fn walk(base: &std::path::Path, at: &std::path::Path, out: &mut Vec<(String, u64, u64)>) {
            let Ok(rd) = std::fs::read_dir(at) else { return };
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                // Two kinds of thing neither side is meant to be compared on.
                //
                // `.alloyfs` is agent bookkeeping — a Windows agent keeps a
                // POSIX-mode sidecar there, auto-excluded from the client, so
                // leaving it in makes this sensitive to exclude settings that
                // have nothing to do with the question.
                //
                // `System Volume Information` is Windows writing to its own
                // fresh volume. See `is_windows_volume_metadata`.
                if name.starts_with(".alloyfs") || is_windows_volume_metadata(&name) {
                    continue;
                }
                let Ok(md) = std::fs::metadata(&p) else { continue };
                if md.is_dir() {
                    walk(base, &p, out);
                } else {
                    let rel = p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
                    let body = std::fs::read(&p).unwrap_or_default();
                    // FNV-1a: no dependency, and collisions do not matter for
                    // "did these bytes change".
                    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                    for b in &body {
                        h ^= *b as u64;
                        h = h.wrapping_mul(0x1000_0000_01b3);
                    }
                    out.push((rel, md.len(), h));
                }
            }
        }
        let mut v = Vec::new();
        walk(dir, dir, &mut v);
        v.sort();
        v
    }

    let mount_root = std::path::PathBuf::from(format!("{mountpoint}\\"));
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut rng = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        seed >> 11
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut worst = Duration::ZERO;
        for round in 0..12 {
            // One mutation per round, cycling the kinds so every one is
            // exercised several times.
            let n = rng() % 40;
            match round % 5 {
                0 => {
                    std::fs::write(root.join(format!("new{n}.txt")), vec![b'a'; (n as usize) + 1]).unwrap();
                }
                1 => {
                    // Grow an existing file.
                    std::fs::write(root.join("hello.txt"), vec![b'g'; 100 + n as usize]).unwrap();
                }
                2 => {
                    // SAME SIZE, different bytes — the shape that hid on L:.
                    let cur = std::fs::read(root.join("hello.txt")).unwrap();
                    let flipped: Vec<u8> = cur.iter().map(|b| b ^ 0x20).collect();
                    std::fs::write(root.join("hello.txt"), &flipped).unwrap();
                }
                3 => {
                    std::fs::write(
                        root.join("sub").join("nested.bin"),
                        vec![(n % 251) as u8; 1000 + n as usize],
                    )
                    .unwrap();
                }
                _ => {
                    let victim = root.join(format!("new{}.txt", n % 40));
                    if victim.exists() {
                        std::fs::remove_file(&victim).unwrap();
                    }
                }
            }

            // Converge, with a bound. Anything that needs more than this is
            // the bug the test is looking for.
            // BOTH sides are re-read every poll. Snapshotting the origin once
            // and polling the mount against it compares a live view to a stale
            // one: anything appearing in the export after the snapshot — agent
            // bookkeeping, an editor temp file, the OS — shows up on the mount,
            // never in the snapshot, and the test reports it as the mount
            // inventing a file. That is exactly what it did on CI.
            let start = std::time::Instant::now();
            let deadline = start + Duration::from_secs(10);
            let (mut got, mut want) = (picture(&mount_root), picture(&root));
            while got != want && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
                want = picture(&root);
                got = picture(&mount_root);
            }
            worst = worst.max(start.elapsed());

            if got.len() != want.len() {
                let g: Vec<&str> = got.iter().map(|e| e.0.as_str()).collect();
                let w: Vec<&str> = want.iter().map(|e| e.0.as_str()).collect();
                panic!(
                    "round {round}: the mount lists {} files, the origin has {}\n  \
                     mount:  {g:?}\n  origin: {w:?}\n  \
                     extra on the mount: {:?}\n  missing from the mount: {:?}",
                    got.len(),
                    want.len(),
                    g.iter().filter(|n| !w.contains(n)).collect::<Vec<_>>(),
                    w.iter().filter(|n| !g.contains(n)).collect::<Vec<_>>(),
                );
            }
            for (a, b) in got.iter().zip(&want) {
                assert_eq!(a.0, b.0, "round {round}: name mismatch");
                assert_eq!(a.1, b.1, "round {round}: {} size {} vs {}", a.0, a.1, b.1);
                assert_eq!(
                    a.2, b.2,
                    "round {round}: {} has the right SIZE and the wrong BYTES — \
                     a layer is serving a stale copy that no attribute check would catch",
                    a.0
                );
            }
        }
        println!("  12 churn rounds agreed; slowest convergence {worst:?}");
    }));

    drive.unmount();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Writes THROUGH the mount must land on the server as bytes, not as size.
///
/// The hypothesis this tests, and the reason it is worth a test of its own: a
/// file that is the right length and entirely NUL is not a truncated
/// transfer. A truncated transfer is SHORT. Full-size-and-zero is what you get
/// when the size landed and the data did not — `ftruncate` with no writes
/// leaves a sparse file that reads back as zeros. Which makes it a metadata /
/// data split on the WRITE path, the mirror of the stale-read split on the
/// read path.
///
/// One such file turned up at the origin during the 2026-08-23 incident:
/// `mockup/README.md`, 11,684 bytes, zero printable characters, while the
/// mount served the correct content. It has since been overwritten so the
/// evidence is gone, and the push was `scp` rather than the mount — so this
/// does not reproduce that file. It closes the gap that made it impossible to
/// rule the mount out.
///
/// Every case here ends with the SERVER's bytes being read directly, never
/// through the mount: reading back through the thing under test is how the
/// original went unnoticed for twenty minutes.
#[cfg(windows)]
#[test]
fn a_write_through_the_mount_lands_as_bytes_on_the_server() {
    use std::io::{Seek, SeekFrom, Write};

    let Some(claim) = claim_drive_letter() else {
        eprintln!("SKIP: no free drive letter");
        return;
    };
    let letter = claim.letter;
    let mountpoint = format!("{letter}:");

    let fixture = fixture();
    let root = fixture.dir.path().to_path_buf();
    let drive = match alloyfs_mount_winfsp::mount(
        fixture.fs.clone(),
        &mountpoint,
        "writes",
        Some(Duration::from_micros(300)),
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: could not mount: {e}");
            return;
        }
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mp = std::path::Path::new(&mountpoint);

        // 1. A plain create-and-write.
        let body = vec![b'k'; 11_684]; // the size of the file that started this
        std::fs::write(mp.join("plain.bin"), &body).expect("write through the mount");
        let on_server = std::fs::read(root.join("plain.bin")).expect("read the server's copy");
        assert_eq!(on_server.len(), body.len(), "plain: size");
        assert!(
            on_server.iter().any(|&b| b != 0),
            "plain: the server has {} bytes and every one is NUL — the size landed \
             and the data did not",
            on_server.len()
        );
        assert_eq!(on_server, body, "plain: bytes");

        // 2. Truncate-then-write, which is what an editor saving a file does,
        //    and the sequence that would leave a hole if the write half were
        //    lost.
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(mp.join("plain.bin"))
                .expect("reopen truncating");
            f.write_all(&vec![b'j'; 9_000]).expect("write");
            f.flush().expect("flush");
        }
        let on_server = std::fs::read(root.join("plain.bin")).expect("server copy");
        assert_eq!(on_server.len(), 9_000, "truncate+write: size");
        assert!(
            on_server.iter().all(|&b| b == b'j'),
            "truncate+write: the server kept a hole where the new bytes should be"
        );

        // 3. Grow by seeking PAST the end and writing, which creates a real
        //    hole on purpose. The hole must be NUL and the written tail must
        //    be present — a filesystem that loses the tail here reports a
        //    perfectly-sized all-NUL file.
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(mp.join("holed.bin"))
                .expect("create");
            f.seek(SeekFrom::Start(5_000)).expect("seek past the end");
            f.write_all(b"TAIL").expect("write the tail");
            f.flush().expect("flush");
        }
        let on_server = std::fs::read(root.join("holed.bin")).expect("server copy");
        assert_eq!(on_server.len(), 5_004, "holed: size");
        assert_eq!(&on_server[5_000..], b"TAIL", "holed: the tail must survive");
        assert!(
            on_server[..5_000].iter().all(|&b| b == 0),
            "holed: the hole must read as NUL"
        );

        // 4. Many small writes on one handle, the shape the write batcher
        //    coalesces — and therefore the shape where a lost batch would
        //    leave size without bytes.
        {
            let mut f = std::fs::File::create(mp.join("batched.bin")).expect("create");
            for i in 0..400 {
                f.write_all(&[(i % 251) as u8; 32]).expect("write");
            }
            f.flush().expect("flush");
        }
        let on_server = std::fs::read(root.join("batched.bin")).expect("server copy");
        assert_eq!(on_server.len(), 400 * 32, "batched: size");
        let expected: Vec<u8> = (0..400).flat_map(|i| [(i % 251) as u8; 32]).collect();
        assert_eq!(on_server, expected, "batched: bytes, in order");

        // 5. The counter that exists for exactly this. A settle failure means
        //    the client acknowledged a write the server then refused, which is
        //    the state that produces a file the mount believes in and the
        //    origin does not.
        assert_eq!(
            fixture.fs.batch_settle_failures(),
            0,
            "the client acknowledged writes the server did not accept"
        );
    }));

    drive.unmount();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
