//! Sequential readahead for one open remote handle.
//!
//! Without prefetch a big sequential read pays one RTT per kernel read even
//! with per-read chunk concurrency. This module watches the offset stream
//! per handle; after `MIN_STREAK` sequential-ish reads it keeps
//! `WINDOW_BLOCKS` × 128 KiB in flight ahead of the reader.
//!
//! Two mount realities shape the design (they were the 3 MB/s ceiling):
//!
//! - Kernel read streams are NOT strictly ordered. WinFsp dispatches on
//!   multiple threads and the Windows cache manager issues overlapped
//!   read-ahead paging I/O; FUSE readahead can reorder too. A "sequential"
//!   copy therefore arrives as slightly out-of-order, overlapping offsets —
//!   the window must tolerate that instead of clearing itself on every
//!   deviation (each clear aborted 2 MiB of useful in-flight data).
//! - Kernel reads can be smaller than DATA_CHUNK (64 KiB paging reads are
//!   common). A consumed block must stay available for the next sub-chunk
//!   read (`recent` retention) or every second read pays a synchronous RTT
//!   re-fetching the block its predecessor just threw away.
//!
//! Blocks are absolute DATA_CHUNK-aligned file ranges. Failed or abandoned
//! blocks die with the handle table entry.

use std::collections::HashMap;
use std::sync::Mutex;

use bytes::Bytes;
use ds_proto::DATA_CHUNK;
use tokio::task::JoinHandle;

/// How many blocks to keep in flight ahead of a sequential reader (32 ×
/// 128 KiB = 4 MiB per streaming handle; measured on a 60 ms link, 32 beats
/// 16 by ~8% once the window stops collapsing).
pub(crate) const WINDOW_BLOCKS: u64 = 32;
/// Consecutive sequential-ish reads before prefetch kicks in (random access
/// never pays the bandwidth tax).
const MIN_STREAK: u32 = 2;
/// Consumed blocks kept for sub-chunk re-reads (4 × 128 KiB per handle).
const RETAIN_BLOCKS: usize = 4;

pub(crate) struct ReadAhead {
    /// block index → in-flight fetch of that block's bytes (None = failed).
    window: Mutex<HashMap<u64, JoinHandle<Option<Bytes>>>>,
    /// Recently assembled blocks (insertion-ordered ring of RETAIN_BLOCKS).
    recent: Mutex<Vec<(u64, Bytes)>>,
    seq: Mutex<SeqState>,
    pub stats: Stats,
}

struct SeqState {
    expected_next: u64,
    streak: u32,
}

/// Read-path counters, dumped on release when `DS_READ_STATS=1`. Cheap
/// enough to maintain unconditionally (relaxed atomics).
#[derive(Default)]
pub(crate) struct Stats {
    pub window_hits: std::sync::atomic::AtomicU64,
    pub retained_hits: std::sync::atomic::AtomicU64,
    pub sync_fetches: std::sync::atomic::AtomicU64,
    pub clears: std::sync::atomic::AtomicU64,
    pub tolerated_ooo: std::sync::atomic::AtomicU64,
}

impl Stats {
    pub fn enabled() -> bool {
        std::env::var_os("DS_READ_STATS").is_some_and(|v| v == "1")
    }
}

impl ReadAhead {
    pub fn new() -> Self {
        Self {
            window: Mutex::new(HashMap::new()),
            recent: Mutex::new(Vec::with_capacity(RETAIN_BLOCKS)),
            seq: Mutex::new(SeqState {
                expected_next: 0,
                streak: 0,
            }),
            stats: Stats::default(),
        }
    }

    /// Record this read's position. Returns true when the access pattern is
    /// sequential enough that the caller should top up the prefetch window.
    ///
    /// Offsets that don't exactly continue the stream but land NEAR it
    /// (within a window span either side) are tolerated as the reordered /
    /// overlapped dispatch they almost always are — the window survives and
    /// the streak continues. Only a genuine far seek clears the window.
    pub fn observe(&self, offset: u64, size: u32) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let mut seq = self.seq.lock().unwrap();
        if offset == seq.expected_next {
            seq.streak = seq.streak.saturating_add(1);
            seq.expected_next = offset + size as u64;
            return seq.streak >= MIN_STREAK;
        }
        let block = Self::block_of(offset);
        let expected_block = Self::block_of(seq.expected_next);
        let near = block >= expected_block.saturating_sub(WINDOW_BLOCKS)
            && block <= expected_block.saturating_add(WINDOW_BLOCKS);
        if near {
            self.stats.tolerated_ooo.fetch_add(1, Relaxed);
            seq.streak = seq.streak.saturating_add(1);
            // Only ever advance: a backward overlapped read must not shrink
            // the frontier the window is prefetching ahead of.
            seq.expected_next = seq.expected_next.max(offset + size as u64);
            return seq.streak >= MIN_STREAK;
        }
        // Far seek: in-flight blocks for the old stream are useless.
        self.stats.clears.fetch_add(1, Relaxed);
        seq.streak = if offset == 0 { 1 } else { 0 };
        seq.expected_next = offset + size as u64;
        drop(seq);
        self.clear();
        false
    }

    /// Take the in-flight fetch for `block` if one exists (consumed once —
    /// callers hand the assembled bytes back via `retain`).
    pub fn take(&self, block: u64) -> Option<JoinHandle<Option<Bytes>>> {
        self.window.lock().unwrap().remove(&block)
    }

    /// A recently assembled block's bytes, if still retained (sub-chunk
    /// kernel reads hit the same block several times in a row).
    pub fn retained(&self, block: u64) -> Option<Bytes> {
        let recent = self.recent.lock().unwrap();
        recent.iter().find(|(b, _)| *b == block).map(|(_, d)| d.clone())
    }

    /// Remember an assembled block for imminent sub-chunk re-reads.
    pub fn retain(&self, block: u64, data: Bytes) {
        let mut recent = self.recent.lock().unwrap();
        if let Some(slot) = recent.iter_mut().find(|(b, _)| *b == block) {
            slot.1 = data;
            return;
        }
        if recent.len() == RETAIN_BLOCKS {
            recent.remove(0);
        }
        recent.push((block, data));
    }

    /// Which blocks in `[from_block, from_block + WINDOW_BLOCKS)` are not yet
    /// in flight? (Caller spawns the fetches and hands them back via `put`.)
    pub fn missing(&self, from_block: u64, max_block_exclusive: u64) -> Vec<u64> {
        let window = self.window.lock().unwrap();
        (from_block..from_block.saturating_add(WINDOW_BLOCKS).min(max_block_exclusive))
            .filter(|b| !window.contains_key(b))
            .collect()
    }

    pub fn put(&self, block: u64, task: JoinHandle<Option<Bytes>>) {
        self.window.lock().unwrap().insert(block, task);
    }

    /// Drop everything in flight AND retained (write happened, far seek,
    /// reconnect) — stale bytes must never answer a later read.
    pub fn clear(&self) {
        let mut window = self.window.lock().unwrap();
        for (_, task) in window.drain() {
            task.abort();
        }
        drop(window);
        self.recent.lock().unwrap().clear();
    }

    pub fn block_of(offset: u64) -> u64 {
        offset / DATA_CHUNK as u64
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        self.clear();
    }
}
