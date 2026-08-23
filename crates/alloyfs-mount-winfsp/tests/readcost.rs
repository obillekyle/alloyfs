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
