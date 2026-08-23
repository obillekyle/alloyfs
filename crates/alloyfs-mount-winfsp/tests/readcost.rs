//! What a cached read costs through the mount, at each `FileInfoTimeout`.
//!
//! The auto-cache is ON here, deliberately. `ClientOptions::default()` has
//! `auto_cache_max_fallback: 0` — the library default is cache OFF — so a run
//! without it measures a full protocol round trip on every read and answers a
//! different question entirely. A production mount takes its cache settings
//! from the agent, and every read below is served from a local blob.
//!
//! The question this answers: if the kernel cache cannot be used safely (see
//! `smoke.rs` and the comment at `file_info_timeout`), how much is left on the
//! table by serving every read ourselves?
//!
//! `#[ignore]`d — it is a measurement, not an assertion. Run it with
//! `cargo test -p alloyfs-mount-winfsp --test readcost -- --ignored --nocapture`.
//!
//! Discipline, because this box's latency drifts ~50% intraday: the three
//! settings are measured ADJACENTLY in one process, the round order rotates so
//! ordering bias cancels, and every round re-measures a local-disk control so
//! drift is visible rather than assumed. Numbers from different runs of this
//! file are not comparable; numbers within one run are.

#![cfg(windows)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloyfs_agent::{AgentConfig, AgentSession, ExportConfig, ExportRegistry};
use alloyfs_client::{ClientOptions, RemoteFs};
use alloyfs_transport::{serve_connection, MuxConnection, RequestHandler};

const READS: usize = 500;
const BLOCK: usize = 64 * 1024;
const FILE_BYTES: usize = 4 * 1024 * 1024;

/// See `smoke.rs` for why this is a lock file and not just `GetLogicalDrives`:
/// nextest runs each test in its own process, and two mount tests racing for
/// the same letter read each other's volumes.
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
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    if mask == 0 {
        return None;
    }
    for &b in b"YXWVUTSRQP" {
        let letter = b as char;
        if mask & (1u32 << (b - b'A')) != 0 {
            continue;
        }
        let lock = std::env::temp_dir().join(format!("alloyfs-test-drive-{letter}.lock"));
        if let Ok(md) = std::fs::metadata(&lock) {
            if md
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(300))
            {
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

fn pct(mut v: Vec<u128>, p: f64) -> f64 {
    v.sort_unstable();
    v[((v.len() as f64 - 1.0) * p) as usize] as f64
}

/// `READS` random 64 KiB reads at random offsets on ONE open handle.
///
/// The handle is opened outside the timed region on purpose. Opening per read
/// costs ~5 ms through a user-mode filesystem and swamps everything: measured
/// that way all three settings sit within 2x of each other and the read cost
/// is invisible. The question here is what a cached READ costs, so the open is
/// not part of it.
fn measure(path: &std::path::Path) -> (f64, f64) {
    use std::io::{Read, Seek, SeekFrom};
    let mut buf = vec![0u8; BLOCK];
    let mut us = Vec::with_capacity(READS);
    let mut f = std::fs::File::open(path).expect("open");
    // A fixed LCG rather than rand: the offsets must be identical across the
    // three settings, or the comparison measures the offsets.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..READS {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let off = (seed >> 20) as usize % (FILE_BYTES - BLOCK);
        let t = Instant::now();
        f.seek(SeekFrom::Start(off as u64)).expect("seek");
        f.read_exact(&mut buf).expect("read");
        us.push(t.elapsed().as_micros());
    }
    (pct(us.clone(), 0.50), pct(us, 0.95))
}

#[test]
#[ignore]
fn what_a_cached_read_costs_at_each_timeout() {
    let Some(claim) = claim_drive_letter() else {
        eprintln!("SKIP: no free drive letter");
        return;
    };
    let letter = claim.letter;

    let dir = tempfile::TempDir::new().expect("tempdir");
    let payload: Vec<u8> = (0..FILE_BYTES).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.path().join("big.bin"), &payload).unwrap();
    // The control: the same reads against local disk, so drift is measurable.
    let control_dir = tempfile::TempDir::new().expect("control tempdir");
    let control = control_dir.path().join("big.bin");
    std::fs::write(&control, &payload).unwrap();

    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            ..Default::default()
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("registry"));
    let cache_dir = tempfile::TempDir::new().expect("cache dir");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    let settings: [(&str, u32); 3] = [
        ("u32::MAX (kernel serves)", u32::MAX),
        ("30 s (we serve)", 30_000),
        ("0 (we serve, no meta cache)", 0),
    ];

    println!(
        "\n  {READS} random {}KiB reads, fresh handle each, one process\n",
        BLOCK / 1024
    );
    println!(
        "  {:<30} {:>10} {:>10} {:>12}",
        "FileInfoTimeout", "p50 us", "p95 us", "control p50"
    );

    for round in 0..3 {
        println!("  --- round {} ---", round + 1);
        for i in 0..settings.len() {
            // Rotate the order each round so a warming trend cannot favour
            // whichever setting happens to go first.
            let (label, fit) = settings[(i + round) % settings.len()];
            let fs = rt.block_on(async {
                let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
                let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
                tokio::spawn(async move {
                    let _ = serve_connection(server_io, "bench-agent", handler).await;
                });
                let conn = MuxConnection::establish(client_io, "bench-client")
                    .await
                    .expect("handshake");
                let opts = ClientOptions {
                    cache_dir: cache_dir.path().to_path_buf(),
                    data_dir: cache_dir.path().to_path_buf(),
                    mount_key: format!("bench{fit}"),
                    // Comfortably larger than the file, so it is cached whole
                    // and every timed read is a cache hit.
                    auto_cache_max_fallback: 64 * 1024 * 1024,
                    no_server_defaults: true,
                    ..Default::default()
                };
                RemoteFs::attach_with(conn, "test", opts).await.expect("attach")
            });
            let mountpoint = format!("{letter}:");
            let drive = match alloyfs_mount_winfsp::mount_with_timeout(
                fs,
                &mountpoint,
                "bench",
                Some(Duration::from_micros(300)),
                fit,
                // Listings ride the same setting here: this test is about the
                // file window, and letting dir info differ would confound it.
                fit,
            ) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("SKIP: mount failed: {e}");
                    return;
                }
            };
            let path = std::path::Path::new(&mountpoint).join("big.bin");
            // Warm it: the interesting number is a CACHED read, not a fetch.
            let _ = std::fs::read(&path).expect("warm");
            let (p50, p95) = measure(&path);
            drive.unmount();
            let (c50, _) = measure(&control);
            println!("  {label:<30} {p50:>10.1} {p95:>10.1} {c50:>12.1}");
        }
    }
    println!();
}

/// What opening a folder costs, at each `DirInfoTimeout`.
///
/// `DirInfoTimeout` overrides `FileInfoTimeout` for directory listings only,
/// and the two are not the same risk. What made `u32::MAX` unusable for file
/// info was cached DATA — a listing has none. It is names and attrs, and attr
/// invalidation is the half that demonstrably works: the FSD purges dir-info
/// per entry and the parent's listing on notify, which is why size and mtime
/// were correct throughout the stale-data incident.
///
/// So this asks whether a long dir cache is worth having, with
/// `FileInfoTimeout` held at the safe bounded value throughout.
#[test]
#[ignore]
fn what_opening_a_folder_costs() {
    let Some(claim) = claim_drive_letter() else {
        eprintln!("SKIP: no free drive letter");
        return;
    };
    let letter = claim.letter;

    let dir = tempfile::TempDir::new().expect("tempdir");
    // A directory big enough that enumerating it is measurable.
    let listing = dir.path().join("many");
    std::fs::create_dir(&listing).unwrap();
    for i in 0..500 {
        std::fs::write(listing.join(format!("f{i:04}.txt")), b"x").unwrap();
    }

    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            ..Default::default()
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("registry"));
    let cache_dir = tempfile::TempDir::new().expect("cache dir");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    let settings: [(&str, u32); 4] = [
        ("dir 0 (no kernel cache)", 0),
        ("dir 30 s (today)", 30_000),
        ("dir 300 s", 300_000),
        ("dir u32::MAX", u32::MAX),
    ];

    println!("\n  100 enumerations of a 500-entry directory, FileInfoTimeout held at 30 s\n");
    println!(
        "  {:<22} {:>10} {:>10} {:>12}",
        "DirInfoTimeout", "p50 us", "p95 us", "control p50"
    );

    for round in 0..3 {
        println!("  --- round {} ---", round + 1);
        for i in 0..settings.len() {
            let (label, dit) = settings[(i + round) % settings.len()];
            let fs = rt.block_on(async {
                let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
                let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
                tokio::spawn(async move {
                    let _ = serve_connection(server_io, "bench-agent", handler).await;
                });
                let conn = MuxConnection::establish(client_io, "bench-client")
                    .await
                    .expect("handshake");
                let opts = ClientOptions {
                    cache_dir: cache_dir.path().to_path_buf(),
                    data_dir: cache_dir.path().to_path_buf(),
                    mount_key: format!("dirbench{dit}"),
                    auto_cache_max_fallback: 64 * 1024 * 1024,
                    no_server_defaults: true,
                    ..Default::default()
                };
                RemoteFs::attach_with(conn, "test", opts).await.expect("attach")
            });
            let mountpoint = format!("{letter}:");
            let drive = match alloyfs_mount_winfsp::mount_with_timeout(
                fs,
                &mountpoint,
                "dirbench",
                Some(Duration::from_micros(300)),
                30_000,
                dit,
            ) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("SKIP: mount failed: {e}");
                    return;
                }
            };
            let path = std::path::Path::new(&mountpoint).join("many");
            let _ = std::fs::read_dir(&path).unwrap().count(); // warm
            let mut us = Vec::with_capacity(100);
            for _ in 0..100 {
                let t = Instant::now();
                let n = std::fs::read_dir(&path).unwrap().count();
                us.push(t.elapsed().as_micros());
                assert_eq!(n, 500, "the listing must be complete every time");
            }
            drive.unmount();
            let mut c = Vec::with_capacity(20);
            for _ in 0..20 {
                let t = Instant::now();
                let _ = std::fs::read_dir(&listing).unwrap().count();
                c.push(t.elapsed().as_micros());
            }
            println!(
                "  {label:<22} {:>10.1} {:>10.1} {:>12.1}",
                pct(us.clone(), 0.50),
                pct(us, 0.95),
                pct(c, 0.50)
            );
        }
    }
    println!();
}

/// What a repeated `stat` costs, at each `FileInfoTimeout`.
///
/// The gap the other two measurements leave. `what_a_cached_read_costs_*`
/// holds one handle open and times reads; `what_opening_a_folder_costs` times
/// enumerations. Neither touches the operation `FileInfoTimeout` is actually
/// named for: asking for one file's attributes, repeatedly.
///
/// This is the measurement that decides whether the kernel's metadata cache
/// earns its place at all. If a bounded window is much faster than 0, the
/// kernel is absorbing calls that would otherwise cross into this process. If
/// it is not, the window is buying nothing and only costs a staleness risk.
#[test]
#[ignore]
fn what_a_repeated_stat_costs_at_each_timeout() {
    let Some(claim) = claim_drive_letter() else {
        eprintln!("SKIP: no free drive letter");
        return;
    };
    let letter = claim.letter;

    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("one.txt"), b"x").unwrap();
    let control_dir = tempfile::TempDir::new().expect("control tempdir");
    let control = control_dir.path().join("one.txt");
    std::fs::write(&control, b"x").unwrap();

    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            ..Default::default()
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("registry"));
    let cache_dir = tempfile::TempDir::new().expect("cache dir");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    let settings: [(&str, u32); 3] = [
        ("0 (we answer every stat)", 0),
        ("30 s (today)", 30_000),
        ("u32::MAX", u32::MAX),
    ];

    let stats = 1000usize;
    println!("\n  {stats} stats of ONE file, one process\n");
    println!(
        "  {:<28} {:>10} {:>10} {:>12}",
        "FileInfoTimeout", "p50 us", "p95 us", "control p50"
    );

    for round in 0..3 {
        println!("  --- round {} ---", round + 1);
        for i in 0..settings.len() {
            let (label, fit) = settings[(i + round) % settings.len()];
            let fs = rt.block_on(async {
                let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
                let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
                tokio::spawn(async move {
                    let _ = serve_connection(server_io, "stat-agent", handler).await;
                });
                let conn = MuxConnection::establish(client_io, "stat-client")
                    .await
                    .expect("handshake");
                let opts = ClientOptions {
                    cache_dir: cache_dir.path().to_path_buf(),
                    data_dir: cache_dir.path().to_path_buf(),
                    mount_key: format!("stat{fit}"),
                    auto_cache_max_fallback: 64 * 1024 * 1024,
                    no_server_defaults: true,
                    ..Default::default()
                };
                RemoteFs::attach_with(conn, "test", opts).await.expect("attach")
            });
            let mountpoint = format!("{letter}:");
            let drive = match alloyfs_mount_winfsp::mount_with_timeout(
                fs,
                &mountpoint,
                "stat",
                Some(Duration::from_micros(300)),
                fit,
                fit,
            ) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("SKIP: mount failed: {e}");
                    return;
                }
            };
            let path = std::path::Path::new(&mountpoint).join("one.txt");
            let _ = std::fs::metadata(&path).expect("warm");
            let mut us = Vec::with_capacity(stats);
            for _ in 0..stats {
                let t = Instant::now();
                let _ = std::fs::metadata(&path).expect("stat");
                us.push(t.elapsed().as_micros());
            }
            drive.unmount();
            let mut c = Vec::with_capacity(200);
            for _ in 0..200 {
                let t = Instant::now();
                let _ = std::fs::metadata(&control).expect("control stat");
                c.push(t.elapsed().as_micros());
            }
            println!(
                "  {label:<28} {:>10.1} {:>10.1} {:>12.1}",
                pct(us.clone(), 0.50),
                pct(us, 0.95),
                pct(c, 0.50)
            );
        }
    }
    println!();
}

/// Where the 490 us of a `FileInfoTimeout(0)` stat actually goes.
///
/// "Serve metadata ourselves from RAM and set the timeout to 0" is only a good
/// trade if OUR stat is cheap. It measures 490 us through the mount against
/// 273 us with the kernel cache on, and 490 us is absurd for a hash lookup —
/// so the number has to be split before it can be argued about.
///
/// Three points, same process, same file:
///
///   - `RemoteFs::getattr` called directly: our cache lookup, nothing else.
///   - `RemoteFs::lookup` by name: what the mount actually calls, since WinFsp
///     hands us a PATH and not an inode.
///   - `std::fs::metadata` through the mount at timeout 0: all of the above
///     plus the FSD round trip and our dispatch.
///
/// The gap between the last two is the part no amount of caching can remove,
/// and the gap between the first two is ours.
#[test]
#[ignore]
fn where_a_stat_actually_goes() {
    let Some(claim) = claim_drive_letter() else {
        eprintln!("SKIP: no free drive letter");
        return;
    };
    let letter = claim.letter;

    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("one.txt"), b"x").unwrap();

    let mut cfg = AgentConfig::default();
    cfg.exports.insert(
        "test".into(),
        ExportConfig {
            path: dir.path().to_path_buf(),
            ..Default::default()
        },
    );
    let registry = Arc::new(ExportRegistry::from_config(&cfg).expect("registry"));
    let cache_dir = tempfile::TempDir::new().expect("cache dir");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    let fs = rt.block_on(async {
        let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
        let handler: Arc<dyn RequestHandler> = Arc::new(AgentSession::new(registry.clone()));
        tokio::spawn(async move {
            let _ = serve_connection(server_io, "split-agent", handler).await;
        });
        let conn = MuxConnection::establish(client_io, "split-client")
            .await
            .expect("handshake");
        let opts = ClientOptions {
            cache_dir: cache_dir.path().to_path_buf(),
            data_dir: cache_dir.path().to_path_buf(),
            mount_key: "split".into(),
            auto_cache_max_fallback: 64 * 1024 * 1024,
            no_server_defaults: true,
            ..Default::default()
        };
        RemoteFs::attach_with(conn, "test", opts).await.expect("attach")
    });

    let n = 2000usize;

    // 1. getattr on a known inode: the cache lookup alone.
    let ino = {
        let fs = fs.clone();
        rt.block_on(async move {
            tokio::task::spawn_blocking(move || {
                fs.lookup(alloyfs_client::ROOT_INO, "one.txt").expect("lookup").0
            })
            .await
            .expect("join")
        })
    };
    let getattr_us = {
        let fs = fs.clone();
        rt.block_on(async move {
            tokio::task::spawn_blocking(move || {
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    let t = Instant::now();
                    let _ = fs.getattr(ino).expect("getattr");
                    v.push(t.elapsed().as_nanos());
                }
                v
            })
            .await
            .expect("join")
        })
    };

    // 2. lookup by name: what the mount path actually needs.
    let lookup_us = {
        let fs = fs.clone();
        rt.block_on(async move {
            tokio::task::spawn_blocking(move || {
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    let t = Instant::now();
                    let _ = fs.lookup(alloyfs_client::ROOT_INO, "one.txt").expect("lookup");
                    v.push(t.elapsed().as_nanos());
                }
                v
            })
            .await
            .expect("join")
        })
    };

    // 3. the same thing through Windows, with the kernel cache off.
    let mountpoint = format!("{letter}:");
    let drive = match alloyfs_mount_winfsp::mount_with_timeout(
        fs.clone(),
        &mountpoint,
        "split",
        Some(Duration::from_micros(300)),
        0,
        0,
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: mount failed: {e}");
            return;
        }
    };
    let path = std::path::Path::new(&mountpoint).join("one.txt");
    let _ = std::fs::metadata(&path).expect("warm");
    let mut through = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let _ = std::fs::metadata(&path).expect("stat");
        through.push(t.elapsed().as_nanos());
    }
    drive.unmount();

    let p50 = |v: Vec<u128>| {
        let mut v = v;
        v.sort_unstable();
        v[v.len() / 2] as f64 / 1000.0
    };
    let g = p50(getattr_us);
    let l = p50(lookup_us);
    let t = p50(through);
    println!("\n  {n} stats of one file, same process\n");
    println!("  RemoteFs::getattr  (our cache alone)   {g:>9.2} us");
    println!("  RemoteFs::lookup   (by name, as mount) {l:>9.2} us");
    println!("  std::fs::metadata  (through the mount) {t:>9.2} us");
    println!("  ------------------------------------------------");
    println!(
        "  our share                              {l:>9.2} us  ({:.1}%)",
        l / t * 100.0
    );
    println!(
        "  FSD round trip + dispatch              {:>9.2} us  ({:.1}%)\n",
        t - l,
        (t - l) / t * 100.0
    );
}
