//! Sequential readahead for one open remote handle.
//!
//! Kernel reads arrive one at a time (read N+1 is issued only after N
//! returns), so without prefetch a big sequential read pays one RTT per
//! window even with per-read chunk concurrency. This module watches the
//! offset stream per handle; after `MIN_STREAK` back-to-back sequential
//! reads it keeps `WINDOW_BLOCKS` × 128 KiB in flight ahead of the reader —
//! at 60 ms RTT that turns ~3 MB/s into window/RTT ≈ 30+ MB/s.
//!
//! Blocks are absolute DATA_CHUNK-aligned file ranges. A prefetched block is
//! consumed at most once (taken out of the window when served); abandoned
//! blocks die with the handle table entry.

use std::collections::HashMap;
use std::sync::Mutex;

use bytes::Bytes;
use ds_proto::DATA_CHUNK;
use tokio::task::JoinHandle;

/// How many blocks to keep in flight ahead of a sequential reader (16 ×
/// 128 KiB = 2 MiB per streaming handle).
pub(crate) const WINDOW_BLOCKS: u64 = 16;
/// Consecutive sequential reads before prefetch kicks in (random access
/// never pays the bandwidth tax).
const MIN_STREAK: u32 = 2;

pub(crate) struct ReadAhead {
    /// block index → in-flight fetch of that block's bytes (None = failed).
    window: Mutex<HashMap<u64, JoinHandle<Option<Bytes>>>>,
    seq: Mutex<SeqState>,
}

struct SeqState {
    expected_next: u64,
    streak: u32,
}

impl ReadAhead {
    pub fn new() -> Self {
        Self {
            window: Mutex::new(HashMap::new()),
            seq: Mutex::new(SeqState {
                expected_next: 0,
                streak: 0,
            }),
        }
    }

    /// Record this read's position. Returns true when the access pattern is
    /// sequential enough that the caller should top up the prefetch window.
    pub fn observe(&self, offset: u64, size: u32) -> bool {
        let mut seq = self.seq.lock().unwrap();
        if offset == seq.expected_next {
            seq.streak = seq.streak.saturating_add(1);
        } else {
            seq.streak = if offset == 0 { 1 } else { 0 };
            // Pattern broke: in-flight blocks for the old stream are useless.
            drop(seq);
            self.clear();
            let mut seq = self.seq.lock().unwrap();
            seq.expected_next = offset + size as u64;
            return false;
        }
        seq.expected_next = offset + size as u64;
        seq.streak >= MIN_STREAK
    }

    /// Take the in-flight fetch for `block` if one exists (consumed once).
    pub fn take(&self, block: u64) -> Option<JoinHandle<Option<Bytes>>> {
        self.window.lock().unwrap().remove(&block)
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

    /// Drop everything in flight (write happened, pattern broke, reconnect).
    pub fn clear(&self) {
        let mut window = self.window.lock().unwrap();
        for (_, task) in window.drain() {
            task.abort();
        }
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
