//! Auto-download cache: full local copies of small (or pinned) remote files,
//! kept fresh by the event stream plus synchronous hooks on our own writes
//! (the server strips self-origin events, so we can't rely on echoes).
//!
//! Freshness rule for serving a blob: the server Attr in hand must match the
//! manifest entry on size AND mtime, and on version unless either side is 0 —
//! server versions live in memory and reset on agent restart, so size+mtime
//! are co-primary.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use alloyfs_proto::{Attr, RelPath};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use alloyfs_common::ExcludeSet;
use alloyfs_common::OrCode;

pub(crate) struct AutoCacheConfig {
    /// `cache.auto-size`: per file. The walker auto-downloads anything up to
    /// this and nothing larger. A read is not bound by it — see `wants`.
    pub max_file_size: u64,
    /// `cache.auto-max`: total pool for files the WALKER pulled down.
    pub budget: u64,
    /// `cache.warm-max`: total pool for files a READ pulled down. Separate
    /// from `budget` on purpose — speculation gets a small allowance, and a
    /// file someone actually opened gets a large one, so a big read cannot
    /// evict the prefetched working set and the prefetcher cannot evict what
    /// is being read.
    pub warm_budget: u64,
    pub pins: Vec<String>,
    pub root: PathBuf,     // data_dir/cache/<mount_key>
    pub manifest: PathBuf, // data_dir/cache/<mount_key>.manifest.json
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct CacheEntry {
    pub version: u64,
    pub size: u64,
    pub mtime_ns: u128,
    pub pinned: bool,
    /// Which pool this entry is charged against: `true` for a blob a READ
    /// demanded, `false` for one the walker chose. Defaults false so a
    /// manifest written before the split loads as auto-downloaded, which is
    /// what every entry in it was.
    #[serde(default)]
    pub warm: bool,
    pub last_used: u64,
    /// Not serialized: false after ResyncRequired until an open re-validates.
    #[serde(skip, default = "default_true")]
    pub verified: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    format: u32,
    entries: BTreeMap<String, CacheEntry>,
    /// The event sequence this cache was last known to be current at.
    ///
    /// This is what lets a cache survive a restart instead of merely a
    /// reconnect. The blobs record WHAT was cached; without a cursor there is
    /// no way to say WHEN, so every entry had to be re-proved one file at a
    /// time. With it, the next mount resubscribes from here and the server
    /// either replays what changed — leaving everything unmentioned provably
    /// current — or answers `TooOld`, which forces a full resync.
    ///
    /// `default` so a format-1 manifest still loads: seq 0 means "no idea",
    /// which subscribes live and re-verifies, exactly as before.
    #[serde(default)]
    seq: u64,
    /// The export's tree token when this cache was last known complete.
    ///
    /// `seq` cannot answer the question this does. It is a per-SESSION event
    /// counter, and an `ssh://` agent is spawned fresh for every mount, so its
    /// numbering restarts and a cursor from a previous mount means nothing.
    /// The tree token is derived from the export's CONTENT, so it is the same
    /// value across agent restarts — which makes it the one thing that can
    /// say "nothing has changed since this cache was written" in a single
    /// exchange, without re-proving a single file.
    ///
    /// 0 means unknown: no token recorded, an unindexed export, or a
    /// format < 3 manifest. All of them mean "walk and find out".
    #[serde(default)]
    tree_token: u64,
}

pub(crate) struct CacheState {
    pub entries: BTreeMap<RelPath, CacheEntry>,
    /// Every byte held, both pools. What `stats` reports and what the
    /// manifest records.
    pub total_bytes: u64,
    /// The read-warmed share of `total_bytes`. The auto share is the
    /// difference, so the two pools are enforced without a second map.
    pub warm_bytes: u64,
    tick: u64,
    /// The manifest no longer describes what is cached: an entry was added,
    /// replaced, evicted or renamed. Must be persisted.
    dirty: bool,
    /// Only RECENCY moved — a hit bumped `last_used`. Worth writing at
    /// shutdown, never worth a periodic rewrite of the whole manifest: a
    /// read-only workload dirtied the manifest on every cache HIT, so the
    /// 30 s flusher rewrote the entire file forever for a workload that
    /// changed nothing. Losing recency costs slightly worse-informed first
    /// evictions after a restart, and nothing else.
    lru_dirty: bool,
}

impl CacheState {
    /// Remove an entry and charge both counters correctly.
    ///
    /// Every removal goes through here. The two pools are tracked as one
    /// total plus the warm share, so a removal that forgot to adjust
    /// `warm_bytes` would leave the warm pool permanently over-counted and
    /// eventually refuse to cache anything. Making that impossible to get
    /// wrong is worth a helper.
    fn take(&mut self, path: &RelPath) -> Option<CacheEntry> {
        let e = self.entries.remove(path)?;
        self.total_bytes -= e.size;
        if e.warm {
            self.warm_bytes -= e.size;
        }
        Some(e)
    }

    /// Insert an entry and charge both counters correctly.
    fn put(&mut self, path: RelPath, e: CacheEntry) {
        self.total_bytes += e.size;
        if e.warm {
            self.warm_bytes += e.size;
        }
        self.entries.insert(path, e);
    }

    /// Bytes held in the pool this entry class is charged against.
    fn pool_bytes(&self, warm: bool) -> u64 {
        if warm {
            self.warm_bytes
        } else {
            self.total_bytes - self.warm_bytes
        }
    }
}

pub(crate) struct AutoCache {
    pub cfg: AutoCacheConfig,
    pins: ExcludeSet, // reused matcher type: "pin globs" share exclude semantics
    state: Mutex<CacheState>,
    fetch_tx: mpsc::UnboundedSender<RelPath>,
    /// Paths a READ asked for, which the auto-download size gate must not
    /// refuse. `auto_cache_max` bounds what the walker pulls down
    /// speculatively; a read is not speculation, so a file the user actually
    /// opened is cached whatever its size - bounded by the budget and its LRU,
    /// like everything else here. Entries are removed once the fetch settles,
    /// so this holds only what is in flight.
    demanded: Mutex<std::collections::HashSet<RelPath>>,
    /// Event sequence the cache is current at. Loaded from the manifest and
    /// written back on every flush, so the cursor outlives the process.
    seq: std::sync::atomic::AtomicU64,
    /// Tree token the cache was last known COMPLETE at; see `Manifest`.
    tree_token: std::sync::atomic::AtomicU64,
    /// Set when this mount skipped discovery because the token still matched.
    /// "Did this mount do any discovery at all" is otherwise invisible from
    /// outside, which makes both the log line and the test unfalsifiable.
    walk_skipped: std::sync::atomic::AtomicBool,
    /// Held across a manifest write, so a caller that wants the manifest
    /// DURABLE waits for one already in progress. `dirty` alone cannot say
    /// that: it is cleared when the snapshot is taken, and the serialize +
    /// write + rename that follow happen outside the state lock. See
    /// `flush_manifest`.
    manifest_lock: Mutex<()>,
}

pub(crate) fn mtime_ns(t: SystemTime) -> u128 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

impl AutoCache {
    /// Load (and verify) the manifest. Returns the cache plus the receiver
    /// end of the re-fetch queue (drained by the fetcher task).
    pub fn load(cfg: AutoCacheConfig) -> anyhow::Result<(Self, mpsc::UnboundedReceiver<RelPath>)> {
        std::fs::create_dir_all(&cfg.root)?;
        let pins = ExcludeSet::compile(&cfg.pins, cfg!(windows))?;
        let mut entries = BTreeMap::new();
        let mut total = 0u64;
        let mut warm_total = 0u64;
        let mut loaded_seq = 0u64;
        let mut loaded_token = 0u64;
        if let Ok(text) = std::fs::read_to_string(&cfg.manifest) {
            if let Ok(m) = serde_json::from_str::<Manifest>(&text) {
                loaded_seq = m.seq;
                loaded_token = m.tree_token;
                for (path, entry) in m.entries {
                    let rel = RelPath(path);
                    // Crash tolerance: blob must exist with the recorded size.
                    let blob = blob_path(&cfg.root, &rel);
                    match std::fs::metadata(&blob) {
                        Ok(md) if md.len() == entry.size => {
                            total += entry.size;
                            if entry.warm {
                                warm_total += entry.size;
                            }
                            entries.insert(rel, entry);
                        }
                        _ => {
                            let _ = std::fs::remove_file(&blob);
                            // The cache is no longer what the token says it
                            // is. The token describes the EXPORT and is still
                            // perfectly true; it just no longer speaks for
                            // this cache, and leaving it set would let the
                            // next mount skip the walk that would refill what
                            // was just discarded.
                            loaded_token = 0;
                        }
                    }
                }
            }
        }
        // Enforce the budget at load too: a remount with a smaller budget
        // must shrink the cache immediately, not on the next refetch.
        if total > cfg.budget {
            let mut victims: Vec<(RelPath, u64, u64, bool)> = entries
                .iter()
                .map(|(p, e)| (p.clone(), e.last_used, e.size, e.pinned))
                .collect();
            victims.sort_by_key(|(_, used, _, _)| *used);
            let mut evicted = 0usize;
            for (p, _, size, pinned) in victims {
                if total <= cfg.budget {
                    break;
                }
                if pinned {
                    continue;
                }
                entries.remove(&p);
                total -= size;
                let _ = std::fs::remove_file(blob_path(&cfg.root, &p));
                evicted += 1;
            }
            if evicted > 0 {
                // Same reasoning as a dropped blob above: evicting for budget
                // leaves a cache the token can no longer vouch for.
                loaded_token = 0;
                tracing::info!(evicted, bytes = total, "auto-cache shrank to fit budget at load");
            }
            if total > cfg.budget {
                tracing::warn!(
                    bytes = total,
                    budget = cfg.budget,
                    "pinned files alone exceed the cache budget"
                );
            }
        }
        let loaded = entries.len();
        let tick = entries.values().map(|e| e.last_used).max().unwrap_or(0) + 1;
        let (fetch_tx, fetch_rx) = mpsc::unbounded_channel();
        tracing::info!(
            entries = loaded,
            bytes = total,
            seq = loaded_seq,
            "auto-cache manifest loaded"
        );
        Ok((
            Self {
                cfg,
                pins,
                state: Mutex::new(CacheState {
                    entries,
                    total_bytes: total,
                    warm_bytes: warm_total,
                    tick,
                    dirty: false,
                    lru_dirty: false,
                }),
                fetch_tx,
                demanded: Mutex::new(std::collections::HashSet::new()),
                seq: std::sync::atomic::AtomicU64::new(loaded_seq),
                tree_token: std::sync::atomic::AtomicU64::new(loaded_token),
                walk_skipped: std::sync::atomic::AtomicBool::new(false),
                manifest_lock: Mutex::new(()),
            },
            fetch_rx,
        ))
    }

    /// The event sequence this cache was last current at, 0 when unknown.
    pub fn saved_seq(&self) -> u64 {
        self.seq.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The tree token this cache was last known COMPLETE at, 0 when unknown.
    pub fn saved_tree_token(&self) -> u64 {
        self.tree_token.load(std::sync::atomic::Ordering::Acquire)
    }

    /// True when this mount skipped discovery because the token matched.
    pub fn walk_skipped(&self) -> bool {
        self.walk_skipped.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Note that discovery was skipped for this mount.
    pub fn note_walk_skipped(&self) {
        self.walk_skipped
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Record the token the cache is now complete at.
    ///
    /// "Complete" is the whole contract, and it is why this is only ever
    /// called after a walk that finished having fetched everything it wanted.
    /// Recording a token beside a half-populated cache would let the next
    /// mount skip a walk it needed — the token would say "nothing changed",
    /// which is true, while the cache was never finished in the first place.
    pub fn record_tree_token(&self, token: u64) {
        self.tree_token.store(token, std::sync::atomic::Ordering::Release);
        self.st().dirty = true;
    }

    /// Advance the recorded cursor. Monotonic — a late writer must never move
    /// it backwards, or the next mount would ask to replay from a point it has
    /// already passed and re-verify work it did not need to.
    pub fn record_seq(&self, seq: u64) {
        let prev = self.seq.fetch_max(seq, std::sync::atomic::Ordering::AcqRel);
        if seq > prev {
            self.st().dirty = true;
        }
    }

    /// The state lock, one honest panic point instead of eighteen.
    fn st(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state.lock().unwrap()
    }

    pub fn pin_match(&self, path: &RelPath) -> bool {
        self.pins.is_excluded(path)
    }

    /// Should the walker/fetcher cache this file at all?
    ///
    /// Three ways in, and they answer different questions. A `pin` is a
    /// standing instruction, so it ignores size. A path a READ asked for is
    /// demand rather than speculation, so it ignores size too — see
    /// `demanded`. Everything else is the walker guessing what will be
    /// wanted, and `max_file_size` is the bound on that guess.
    pub fn wants(&self, path: &RelPath, size: u64) -> bool {
        self.pin_match(path)
            || self.is_demanded(path)
            || (self.cfg.max_file_size > 0 && size <= self.cfg.max_file_size)
    }

    fn is_demanded(&self, path: &RelPath) -> bool {
        self.demanded.lock().unwrap().contains(path)
    }

    /// A read went to the network for `path`; pull the whole file down so the
    /// next read does not have to.
    ///
    /// Idempotent by the `demanded` set: a second call while one is in flight
    /// adds nothing to the queue. The caller still guards per handle so this
    /// is not reached on every read of a large file.
    pub fn enqueue_demand(&self, path: RelPath) {
        if self.cfg.max_file_size == 0 && self.pins.is_empty() {
            return; // caching is off entirely; nothing to warm
        }
        if !self.demanded.lock().unwrap().insert(path.clone()) {
            return; // already in flight
        }
        let _ = self.fetch_tx.send(path);
    }

    /// Drop a demand marker once its fetch has settled, successfully or not.
    /// A later read re-demands, which is what should happen after a failure.
    pub fn clear_demand(&self, path: &RelPath) {
        self.demanded.lock().unwrap().remove(path);
    }

    /// May the cached blob serve reads for `path`, given a current server
    /// Attr? Also bumps LRU + re-verifies after a resync.
    pub fn fresh_for(&self, path: &RelPath, attr: &Attr) -> bool {
        let mut st = self.st();
        st.tick += 1;
        let tick = st.tick;
        let Some(entry) = st.entries.get_mut(path) else {
            return false;
        };
        let fresh = attr.size == entry.size
            && mtime_ns(attr.mtime) == entry.mtime_ns
            && (attr.version == entry.version || attr.version == 0 || entry.version == 0);
        if fresh {
            entry.last_used = tick;
            entry.verified = true;
            // Recency only — nothing about WHAT is cached changed, so this
            // must not drag the whole manifest to disk every 30 s for a
            // workload that is purely reading.
            st.lru_dirty = true;
        }
        fresh
    }

    /// The blob mapped for serving — the only read entry point, because
    /// every caller reads more than once. The mount retains one per open fh
    /// (opening per read priced every cached 64 K with a path build and a
    /// file open/close), and the retained thing is now a MAPPING rather
    /// than a file handle: a warm read is one memcpy out of the page cache
    /// instead of a positional-read syscall plus a zeroed scratch buffer.
    ///
    /// `None` on any open/map failure — the caller falls through to the
    /// network, which is how eviction races resolve themselves.
    pub fn map_blob(&self, path: &RelPath) -> Option<Blob> {
        let f = std::fs::File::open(blob_path(&self.cfg.root, path)).ok()?;
        Blob::of(&f)
    }

    /// Record a fully fetched blob (already staged at its final path by the
    /// fetcher). Evicts LRU non-pinned entries to fit the pool it belongs to.
    ///
    /// Which pool that is comes from whether a READ asked for this path. The
    /// demand marker is still set here — the fetcher clears it only once the
    /// fetch has settled — so no caller has to thread the distinction down.
    pub fn commit(&self, path: &RelPath, attr: &Attr, pinned: bool) {
        let warm = self.is_demanded(path);
        let budget = if warm {
            self.cfg.warm_budget
        } else {
            self.cfg.budget
        };
        let mut st = self.st();
        st.tick += 1;
        let tick = st.tick;
        st.take(path);
        // Budget: evict least-recently-used non-pinned entries FROM THE SAME
        // POOL. Evicting across pools would defeat the split — a large read
        // could clear the prefetched working set, and a prefetch could clear
        // the file being read.
        //
        // Down to a LOW-WATER mark rather than to exactly-fits. Building and
        // sorting the victim list is O(n log n) with a String clone per
        // entry, under the lock every open contends on — and evicting just
        // enough meant paying that on EVERY commit once the cache sat at
        // budget, which is precisely the tail of a big walk. Freeing a
        // tenth of the budget instead amortizes the sort over the many
        // commits that follow. A cache evicting slightly more than it must
        // is a cache; a walk that goes quadratic at the finish line is a
        // bug. (Tiny budgets keep the old behaviour: the tenth rounds to
        // zero and the mark collapses back onto the budget.)
        let low_water = budget - budget / 10;
        if st.pool_bytes(warm) + attr.size > budget {
            let mut victims: Vec<(RelPath, u64, u64)> = st
                .entries
                .iter()
                .filter(|(_, e)| !e.pinned && e.warm == warm)
                .map(|(p, e)| (p.clone(), e.last_used, e.size))
                .collect();
            victims.sort_by_key(|(_, used, _)| *used);
            for (vp, _, _) in victims {
                if st.pool_bytes(warm) + attr.size <= low_water {
                    break;
                }
                st.take(&vp);
                let _ = std::fs::remove_file(blob_path(&self.cfg.root, &vp));
                tracing::debug!(path = %vp, warm, "auto-cache evicted (budget)");
            }
            if pinned && st.pool_bytes(warm) + attr.size > budget {
                tracing::warn!(path = %path, "pinned files exceed the cache budget; caching anyway");
            } else if st.pool_bytes(warm) + attr.size > budget && attr.size > budget {
                // Single blob larger than its whole pool and not pinned:
                // don't cache it at all.
                let _ = std::fs::remove_file(blob_path(&self.cfg.root, path));
                return;
            }
        }
        st.put(
            path.clone(),
            CacheEntry {
                version: attr.version,
                size: attr.size,
                mtime_ns: mtime_ns(attr.mtime),
                pinned,
                warm,
                last_used: tick,
                verified: true,
            },
        );
        st.dirty = true;
    }

    /// Is a (re-)fetch worthwhile: not already fresh for this attr?
    pub fn needs_fetch(&self, path: &RelPath, attr: &Attr) -> bool {
        let st = self.st();
        match st.entries.get(path) {
            Some(e) => {
                !(attr.size == e.size
                    && mtime_ns(attr.mtime) == e.mtime_ns
                    && (attr.version == e.version || attr.version == 0 || e.version == 0))
            }
            None => true,
        }
    }

    pub fn known(&self, path: &RelPath) -> bool {
        self.st().entries.contains_key(path)
    }

    pub fn invalidate(&self, path: &RelPath) {
        let mut st = self.st();
        if st.take(path).is_some() {
            st.dirty = true;
            let _ = std::fs::remove_file(blob_path(&self.cfg.root, path));
        }
    }

    pub fn remove(&self, path: &RelPath) {
        self.invalidate(path);
    }

    /// Rename bookkeeping incl. directory prefix moves (mirrors InodeTable).
    pub fn rename(&self, from: &RelPath, to: &RelPath) {
        let mut st = self.st();
        let prefix = format!("{}/", from.0);
        let affected: Vec<RelPath> = st
            .entries
            .keys()
            .filter(|p| **p == *from || p.0.starts_with(&prefix))
            .cloned()
            .collect();
        for old in affected {
            let new = if old == *from {
                to.clone()
            } else {
                RelPath(format!("{}{}", to.0, &old.0[from.0.len()..]))
            };
            if let Some(entry) = st.take(&old) {
                let old_blob = blob_path(&self.cfg.root, &old);
                let new_blob = blob_path(&self.cfg.root, &new);
                if let Some(parent) = new_blob.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::rename(&old_blob, &new_blob).is_ok() {
                    st.put(new, entry);
                } else {
                    // `take` already discharged it from both counters.
                    let _ = std::fs::remove_file(&old_blob);
                }
                st.dirty = true;
            }
        }
    }

    /// After ResyncRequired: keep blobs but force re-validation at next open.
    pub fn mark_all_unverified(&self) {
        let mut st = self.st();
        for e in st.entries.values_mut() {
            e.verified = false;
        }
    }

    pub fn enqueue_refetch(&self, path: RelPath) {
        let _ = self.fetch_tx.send(path);
    }

    pub fn stats(&self) -> (usize, u64) {
        let st = self.st();
        (st.entries.len(), st.total_bytes)
    }

    /// Persist the manifest if its CONTENT changed. The flusher task's call.
    pub fn flush_manifest(&self) {
        self.write_manifest(false);
    }

    /// The process's last write: also persists recency-only changes.
    ///
    /// Shutdown is the one moment when writing `last_used` is free. The
    /// alternative is not persisting recency at all — a periodic flush that
    /// honoured it would rewrite the entire manifest every 30 s for a
    /// workload that is purely reading, which is what this used to do.
    pub fn flush_manifest_final(&self) {
        self.write_manifest(true);
    }

    /// Write the manifest, if anything worth writing changed.
    ///
    /// The lock is what makes this a BARRIER as well as a write. The dirty
    /// flags are cleared the moment the snapshot is taken, while the
    /// serialize, the temp write and the rename all happen after the state
    /// lock is released — so a caller arriving during that window sees
    /// them clear and would conclude the manifest is safely on disk.
    /// Shutdown is exactly that caller, and it is followed by the process
    /// exiting, which is what turns the wrong conclusion into lost cache
    /// entries. Same lesson as the write batcher's barrier: a cleared flag
    /// is not a completed write.
    fn write_manifest(&self, include_recency: bool) {
        let _one_at_a_time = self.manifest_lock.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = {
            let mut st = self.st();
            if !(st.dirty || (include_recency && st.lru_dirty)) {
                return;
            }
            // Both clear: the snapshot carries whatever `last_used` values
            // are current, so writing it persists recency either way.
            st.dirty = false;
            st.lru_dirty = false;
            Manifest {
                // 2 added `seq`, 3 adds `tree_token`. Neither is a breaking
                // bump: both default to 0 on read, and an older file loads as
                // a cache of unknown age with no known token — which is what
                // it is, and which means "walk and find out".
                format: 3,
                seq: self.seq.load(std::sync::atomic::Ordering::Acquire),
                tree_token: self.tree_token.load(std::sync::atomic::Ordering::Acquire),
                entries: st.entries.iter().map(|(p, e)| (p.0.clone(), e.clone())).collect(),
            }
        };
        match serde_json::to_string(&snapshot) {
            Ok(json) => {
                let tmp = self.cfg.manifest.with_extension("json.part");
                if std::fs::write(&tmp, json).is_ok() {
                    let _ = std::fs::rename(&tmp, &self.cfg.manifest);
                }
            }
            Err(e) => tracing::warn!(error = %e, "manifest serialize failed"),
        }
    }

    pub fn blob_stage_path(&self, path: &RelPath) -> PathBuf {
        let mut p = blob_path(&self.cfg.root, path);
        let name = format!(
            "{}.part",
            p.file_name().and_then(|n| n.to_str()).unwrap_or("blob")
        );
        p.set_file_name(name);
        p
    }

    pub fn blob_final_path(&self, path: &RelPath) -> PathBuf {
        blob_path(&self.cfg.root, path)
    }
}

pub(crate) fn blob_path(root: &std::path::Path, rel: &RelPath) -> PathBuf {
    let mut full = root.to_path_buf();
    for comp in rel.0.split('/').filter(|c| !c.is_empty()) {
        full.push(comp);
    }
    full
}

/// A committed blob, memory-mapped for serving.
///
/// Reads are a bounds-checked slice copy — the page cache serves them
/// directly, with no syscall after fault-in and no zeroed scratch buffer.
/// A read past the mapping's end returns the short/empty tail, which is the
/// same EOF answer the positional-read path gave: a committed blob is the
/// COMPLETE file, so its end is the file's end.
///
/// Mapping instead of holding a file handle keeps the exact lifecycle the
/// handle had: on unix a blob replaced (stage + rename) or evicted
/// (remove_file) under the map leaves the old inode alive until the map
/// drops, and on Windows the map blocks deletion just as the open handle
/// did — eviction already treats that as a race it loses gracefully.
pub struct Blob {
    /// `None` = the blob is an empty file (zero-length mappings are an
    /// error on Windows); every read of it is the empty EOF answer.
    map: Option<memmap2::Mmap>,
}

impl Blob {
    /// Map `f`. `None` when mapping fails (the caller falls through to the
    /// network, exactly as a failed open always did).
    pub(crate) fn of(f: &std::fs::File) -> Option<Self> {
        if f.metadata().ok()?.len() == 0 {
            return Some(Self { map: None });
        }
        // SAFETY: the mapping is only sound while no one truncates the file
        // in place, and blob files are never touched in place — the fetcher
        // stages `.part` and RENAMES over the final path, eviction removes
        // the file. Under both, the mapped inode's length is immutable for
        // the mapping's lifetime, which is the same discipline the retained
        // read handle already leaned on.
        let map = unsafe { memmap2::Mmap::map(f) }.ok()?;
        Some(Self { map: Some(map) })
    }

    pub fn read(&self, offset: u64, size: u32) -> Vec<u8> {
        let Some(map) = &self.map else {
            return Vec::new();
        };
        let len = map.len() as u64;
        let start = offset.min(len) as usize;
        let end = offset.saturating_add(size as u64).min(len) as usize;
        map[start..end].to_vec()
    }

    /// [`Self::read`] straight into the caller's buffer — the mount hands
    /// us the kernel's buffer, so the warm path is one memcpy total instead
    /// of map→Vec→kernel. Returns bytes written; short at the mapping's
    /// end, exactly like `read`.
    pub fn read_into(&self, offset: u64, buf: &mut [u8]) -> usize {
        let Some(map) = &self.map else {
            return 0;
        };
        let len = map.len() as u64;
        let start = offset.min(len) as usize;
        let end = offset.saturating_add(buf.len() as u64).min(len) as usize;
        let n = end - start;
        buf[..n].copy_from_slice(&map[start..end]);
        n
    }
}

/// Convenience wrapper so callers get FsError-flavored IO errors.
pub(crate) fn stage_write(path: &std::path::Path, data: &[u8]) -> Result<(), alloyfs_proto::ErrorCode> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).or_code()?;
    }
    std::fs::write(path, data).or_code()
}

#[cfg(test)]
mod tests {
    /// Reads exactly as the mount does: a retained mapping, sliced.
    fn read_via_handle(c: &AutoCache, p: &RelPath, offset: u64, len: usize) -> Option<Vec<u8>> {
        Some(c.map_blob(p)?.read(offset, len as u32))
    }

    use super::*;
    use alloyfs_proto::FileKind;

    pub(super) fn attr(size: u64, mtime_s: u64, version: u64) -> Attr {
        Attr {
            kind: FileKind::File,
            size,
            mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(mtime_s),
            ctime: SystemTime::UNIX_EPOCH,
            mode: 0o644,
            version,
        }
    }

    fn cache(dir: &std::path::Path, max: u64, budget: u64) -> (AutoCache, mpsc::UnboundedReceiver<RelPath>) {
        AutoCache::load(AutoCacheConfig {
            max_file_size: max,
            budget,
            warm_budget: budget,
            pins: vec![],
            root: dir.join("blobs"),
            manifest: dir.join("m.manifest.json"),
        })
        .unwrap()
    }

    #[test]
    fn freshness_and_manifest_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ds-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (c, _rx) = cache(&dir, 1024, 10_000);
        let p = RelPath("a/b.txt".into());
        let a = attr(5, 100, 7);
        stage_write(&c.blob_final_path(&p), b"hello").unwrap();
        c.commit(&p, &a, false);
        assert!(c.fresh_for(&p, &a));
        assert!(!c.fresh_for(&p, &attr(6, 100, 7)), "size mismatch");
        assert!(!c.fresh_for(&p, &attr(5, 101, 7)), "mtime mismatch");
        assert!(!c.fresh_for(&p, &attr(5, 100, 8)), "version mismatch");
        assert!(c.fresh_for(&p, &attr(5, 100, 0)), "version 0 escape hatch");
        assert_eq!(read_via_handle(&c, &p, 0, 5).as_deref(), Some(&b"hello"[..]));
        c.flush_manifest();

        // Reload: entry survives; corrupt the blob size → dropped.
        let (c2, _rx2) = cache(&dir, 1024, 10_000);
        assert!(c2.fresh_for(&p, &a));
        std::fs::write(c2.blob_final_path(&p), b"xx").unwrap();
        let (c3, _rx3) = cache(&dir, 1024, 10_000);
        assert!(!c3.fresh_for(&p, &a), "size-mismatched blob dropped at load");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn budget_evicts_lru_not_pins() {
        let dir = std::env::temp_dir().join(format!("ds-cache-evict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (c, _rx) = cache(&dir, 1024, 10);
        let pin = RelPath("keep.bin".into());
        let old = RelPath("old.bin".into());
        let new = RelPath("new.bin".into());
        for (p, pinned) in [(&pin, true), (&old, false)] {
            stage_write(&c.blob_final_path(p), &[0u8; 4]).unwrap();
            c.commit(p, &attr(4, 1, 1), pinned);
        }
        let _ = c.fresh_for(&pin, &attr(4, 1, 1)); // bump pin's LRU anyway
        stage_write(&c.blob_final_path(&new), &[0u8; 4]).unwrap();
        c.commit(&new, &attr(4, 1, 1), false); // 12 > 10 → evict `old`
        assert!(c.known(&pin), "pinned survives");
        assert!(!c.known(&old), "LRU non-pinned evicted");
        assert!(c.known(&new));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_moves_prefix() {
        let dir = std::env::temp_dir().join(format!("ds-cache-ren-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (c, _rx) = cache(&dir, 1024, 10_000);
        let a = RelPath("d/x.txt".into());
        stage_write(&c.blob_final_path(&a), b"1").unwrap();
        c.commit(&a, &attr(1, 1, 1), false);
        c.rename(&RelPath("d".into()), &RelPath("e".into()));
        assert!(!c.known(&a));
        assert!(c.known(&RelPath("e/x.txt".into())));
        assert_eq!(
            read_via_handle(&c, &RelPath("e/x.txt".into()), 0, 1).as_deref(),
            Some(&b"1"[..])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reading does not rewrite the manifest; shutdown still saves recency.
    ///
    /// Every cache HIT used to mark the manifest dirty, purely to record
    /// that `last_used` had moved — so a mount doing nothing but reading
    /// rewrote its entire manifest every 30 s, forever. Content changes
    /// still must persist promptly; recency is worth exactly one write, at
    /// the end.
    #[test]
    fn reading_does_not_dirty_the_manifest_but_shutdown_saves_recency() {
        let dir = std::env::temp_dir().join(format!("ds-cache-lru-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (c, _rx) = cache(&dir, 1024, 10_000);
        let p = RelPath("a.txt".into());
        let a = attr(5, 100, 7);
        stage_write(&c.blob_final_path(&p), b"hello").unwrap();
        c.commit(&p, &a, false);
        c.flush_manifest();
        assert!(c.cfg.manifest.exists(), "a content change persists");

        // Removing the file makes the next write observable: whether one
        // happens at all is the whole question.
        std::fs::remove_file(&c.cfg.manifest).unwrap();
        for _ in 0..50 {
            assert!(c.fresh_for(&p, &a), "a hit, which moves recency only");
        }
        c.flush_manifest();
        assert!(
            !c.cfg.manifest.exists(),
            "reads alone must not rewrite the manifest — that is a whole-file \
             write every 30 s for a workload that changed nothing"
        );

        c.flush_manifest_final();
        assert!(
            c.cfg.manifest.exists(),
            "shutdown still persists recency, so the next mount evicts informed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manifest flush is a BARRIER as well as a write.
    ///
    /// `dirty` is cleared when the snapshot is taken, while the serialize,
    /// the temp write and the rename all happen after the state lock is
    /// released — so a caller arriving mid-write saw `dirty == false` and
    /// concluded the manifest was on disk. Shutdown is that caller, and
    /// the process exit right behind it is what turned the wrong
    /// conclusion into lost cache entries. Same family as the write
    /// batcher's barrier bug, one layer up.
    ///
    /// Made reproducible the only way a unit test can make a write slow:
    /// enough entries that serializing them takes real time, flushed from
    /// another thread, with the shutdown-shaped flush racing it.
    #[test]
    fn a_manifest_flush_waits_for_one_already_writing() {
        let dir = std::env::temp_dir().join(format!("ds-manifest-barrier-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (c, _rx) = cache(&dir, 1024, 100_000_000);
        // Entries only — no blobs. The manifest write is what has to take
        // measurable time here, and staging thousands of one-byte files
        // would cost seconds of unrelated disk work.
        for i in 0..30_000 {
            c.commit(&RelPath(format!("dir/f{i:06}.bin")), &attr(1, 100, 1), false);
        }
        let manifest = c.cfg.manifest.clone();
        assert!(!manifest.exists(), "nothing written yet");

        let cache_ref = std::sync::Arc::new(c);
        let writer = {
            let cache_ref = cache_ref.clone();
            std::thread::spawn(move || cache_ref.flush_manifest())
        };
        // Let the writer take the snapshot and clear `dirty` — that is the
        // state the barrier has to survive, so waiting for it is the point
        // rather than a way of hiding a race.
        std::thread::sleep(std::time::Duration::from_millis(5));
        // The shutdown-shaped call: pre-fix it saw the cleared `dirty` and
        // returned while the writer was still serializing.
        cache_ref.flush_manifest();
        let landed = manifest.exists()
            && serde_json::from_str::<Manifest>(&std::fs::read_to_string(&manifest).unwrap()).is_ok();
        writer.join().unwrap();
        assert!(
            landed,
            "flush_manifest returned before the manifest was on disk — a \
             cleared dirty flag is not a completed write"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod warm_pool_tests {
    use super::tests::attr;
    use super::*;

    fn cache_with(dir: &std::path::Path, auto_size: u64, auto_max: u64, warm_max: u64) -> AutoCache {
        AutoCache::load(AutoCacheConfig {
            max_file_size: auto_size,
            budget: auto_max,
            warm_budget: warm_max,
            pins: vec![],
            root: dir.join("blobs"),
            manifest: dir.join("m.manifest.json"),
        })
        .unwrap()
        .0
    }

    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ds-warm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The point of the whole change: a file too big for the walker's
    /// `auto-size` is cached anyway once a READ asks for it.
    ///
    /// Without the demand path `wants` refuses it on size and the fetcher
    /// never commits, which is exactly why reading a 100 MB file crossed the
    /// link on every pass.
    #[test]
    fn a_read_caches_a_file_the_walker_would_refuse() {
        let dir = fresh_dir("demand");
        // auto-size 10 bytes: a 5000-byte file is far past what the walker takes.
        let c = cache_with(&dir, 10, 1_000_000, 1_000_000);
        let p = RelPath("big.bin".into());
        let a = attr(5000, 100, 1);

        assert!(!c.wants(&p, a.size), "the walker must refuse it on size");
        c.enqueue_demand(p.clone());
        assert!(
            c.wants(&p, a.size),
            "a read demanded it, so size must stop mattering"
        );

        stage_write(&c.blob_final_path(&p), &vec![7u8; 5000]).unwrap();
        c.commit(&p, &a, false);
        assert!(c.fresh_for(&p, &a), "and it is now served locally");
    }

    /// The two pools are charged separately, so a big read cannot evict the
    /// prefetched working set.
    #[test]
    fn warming_a_large_file_does_not_evict_the_prefetched_set() {
        let dir = fresh_dir("pools");
        // Tiny auto pool, roomy warm pool.
        let c = cache_with(&dir, 1000, 3000, 100_000);
        // warm pool 100_000: two 60 KB reads cannot both fit, so the second
        // must evict — from its OWN pool.

        // Three walker-chosen files that exactly fill the auto pool.
        for i in 0..3 {
            let p = RelPath(format!("small{i}.bin"));
            let a = attr(1000, 100, 1);
            stage_write(&c.blob_final_path(&p), &vec![1u8; 1000]).unwrap();
            c.commit(&p, &a, false);
        }
        for i in 0..3 {
            let p = RelPath(format!("small{i}.bin"));
            assert!(c.known(&p), "small{i} should be cached before the big read");
        }

        // Two reads that between them OVERFILL the warm pool, so the warm
        // commit genuinely has to evict. Without that the eviction branch
        // never runs and the test cannot tell the pools apart at all.
        for (name, size) in [("huge1.bin", 60_000u64), ("huge2.bin", 60_000)] {
            let bp = RelPath(name.into());
            let ba = attr(size, 100, 1);
            c.enqueue_demand(bp.clone());
            stage_write(&c.blob_final_path(&bp), &vec![9u8; size as usize]).unwrap();
            c.commit(&bp, &ba, false);
            c.clear_demand(&bp);
        }
        let big = RelPath("huge2.bin".into());

        assert!(c.known(&big), "the demanded file is cached");
        for i in 0..3 {
            let p = RelPath(format!("small{i}.bin"));
            assert!(
                c.known(&p),
                "small{i} was evicted by a warm commit — the pools are not separate"
            );
        }
    }

    /// And the reverse: prefetching cannot evict what a read pulled in.
    #[test]
    fn prefetch_does_not_evict_the_warm_set() {
        let dir = fresh_dir("reverse");
        let c = cache_with(&dir, 1000, 2000, 100_000);

        let warm = RelPath("opened.bin".into());
        let wa = attr(1500, 100, 1);
        c.enqueue_demand(warm.clone());
        stage_write(&c.blob_final_path(&warm), &vec![3u8; 1500]).unwrap();
        c.commit(&warm, &wa, false);
        assert!(c.known(&warm));

        // Fill the auto pool several times over.
        for i in 0..6 {
            let p = RelPath(format!("pf{i}.bin"));
            let a = attr(900, 100, 1);
            stage_write(&c.blob_final_path(&p), &vec![2u8; 900]).unwrap();
            c.commit(&p, &a, false);
        }
        assert!(
            c.known(&warm),
            "the file a read pulled in was evicted by prefetching"
        );
    }

    /// A settled fetch drops its marker, or `wants` would keep saying yes for
    /// a path nothing is fetching and a later read could never re-demand.
    #[test]
    fn a_settled_demand_stops_overriding_the_size_gate() {
        let dir = fresh_dir("clear");
        let c = cache_with(&dir, 10, 1_000_000, 1_000_000);
        let p = RelPath("x.bin".into());
        c.enqueue_demand(p.clone());
        assert!(c.wants(&p, 5000));
        c.clear_demand(&p);
        assert!(!c.wants(&p, 5000), "the marker must not outlive its fetch");
    }
}
