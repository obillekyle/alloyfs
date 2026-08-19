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
use alloyfs_common::{read_fully, OrCode};

pub(crate) struct AutoCacheConfig {
    pub max_file_size: u64,
    pub budget: u64,
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
}

pub(crate) struct CacheState {
    pub entries: BTreeMap<RelPath, CacheEntry>,
    pub total_bytes: u64,
    tick: u64,
    dirty: bool,
}

pub(crate) struct AutoCache {
    pub cfg: AutoCacheConfig,
    pins: ExcludeSet, // reused matcher type: "pin globs" share exclude semantics
    state: Mutex<CacheState>,
    fetch_tx: mpsc::UnboundedSender<RelPath>,
    /// Event sequence the cache is current at. Loaded from the manifest and
    /// written back on every flush, so the cursor outlives the process.
    seq: std::sync::atomic::AtomicU64,
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
        let mut loaded_seq = 0u64;
        if let Ok(text) = std::fs::read_to_string(&cfg.manifest) {
            if let Ok(m) = serde_json::from_str::<Manifest>(&text) {
                loaded_seq = m.seq;
                for (path, entry) in m.entries {
                    let rel = RelPath(path);
                    // Crash tolerance: blob must exist with the recorded size.
                    let blob = blob_path(&cfg.root, &rel);
                    match std::fs::metadata(&blob) {
                        Ok(md) if md.len() == entry.size => {
                            total += entry.size;
                            entries.insert(rel, entry);
                        }
                        _ => {
                            let _ = std::fs::remove_file(&blob);
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
                    tick,
                    dirty: false,
                }),
                fetch_tx,
                seq: std::sync::atomic::AtomicU64::new(loaded_seq),
            },
            fetch_rx,
        ))
    }

    /// The event sequence this cache was last current at, 0 when unknown.
    pub fn saved_seq(&self) -> u64 {
        self.seq.load(std::sync::atomic::Ordering::Acquire)
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
    pub fn wants(&self, path: &RelPath, size: u64) -> bool {
        self.pin_match(path) || (self.cfg.max_file_size > 0 && size <= self.cfg.max_file_size)
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
            st.dirty = true;
        }
        fresh
    }

    /// Read from the blob; None on any miss/short-read (caller falls through
    /// to the network — eviction races resolve themselves this way).
    pub fn read(&self, path: &RelPath, offset: u64, len: u32) -> Option<Vec<u8>> {
        let file = std::fs::File::open(blob_path(&self.cfg.root, path)).ok()?;
        let mut buf = vec![0u8; len as usize];
        let n = read_fully(&file, &mut buf, offset).ok()?;
        buf.truncate(n);
        Some(buf)
    }

    /// Record a fully fetched blob (already staged at its final path by the
    /// fetcher). Evicts LRU non-pinned entries to fit the budget.
    pub fn commit(&self, path: &RelPath, attr: &Attr, pinned: bool) {
        let mut st = self.st();
        st.tick += 1;
        let tick = st.tick;
        if let Some(old) = st.entries.remove(path) {
            st.total_bytes -= old.size;
        }
        // Budget: evict least-recently-used non-pinned entries.
        if st.total_bytes + attr.size > self.cfg.budget {
            let mut victims: Vec<(RelPath, u64, u64)> = st
                .entries
                .iter()
                .filter(|(_, e)| !e.pinned)
                .map(|(p, e)| (p.clone(), e.last_used, e.size))
                .collect();
            victims.sort_by_key(|(_, used, _)| *used);
            for (vp, _, vsize) in victims {
                if st.total_bytes + attr.size <= self.cfg.budget {
                    break;
                }
                st.entries.remove(&vp);
                st.total_bytes -= vsize;
                let _ = std::fs::remove_file(blob_path(&self.cfg.root, &vp));
                tracing::debug!(path = %vp, "auto-cache evicted (budget)");
            }
            if pinned && st.total_bytes + attr.size > self.cfg.budget {
                tracing::warn!(path = %path, "pinned files exceed the cache budget; caching anyway");
            } else if st.total_bytes + attr.size > self.cfg.budget && attr.size > self.cfg.budget {
                // Single blob larger than the whole budget and not pinned:
                // don't cache it at all.
                let _ = std::fs::remove_file(blob_path(&self.cfg.root, path));
                return;
            }
        }
        st.total_bytes += attr.size;
        st.entries.insert(
            path.clone(),
            CacheEntry {
                version: attr.version,
                size: attr.size,
                mtime_ns: mtime_ns(attr.mtime),
                pinned,
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
        if let Some(e) = st.entries.remove(path) {
            st.total_bytes -= e.size;
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
            if let Some(entry) = st.entries.remove(&old) {
                let old_blob = blob_path(&self.cfg.root, &old);
                let new_blob = blob_path(&self.cfg.root, &new);
                if let Some(parent) = new_blob.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::rename(&old_blob, &new_blob).is_ok() {
                    st.entries.insert(new, entry);
                } else {
                    st.total_bytes -= entry.size;
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

    /// Persist the manifest if dirty. Called by the flusher task and shutdown.
    pub fn flush_manifest(&self) {
        let snapshot = {
            let mut st = self.st();
            if !st.dirty {
                return;
            }
            st.dirty = false;
            Manifest {
                // 2 adds `seq`. Not a breaking bump: `seq` defaults to 0 on
                // read, and a format-1 file loads as a cache of unknown age,
                // which is what it is.
                format: 2,
                seq: self.seq.load(std::sync::atomic::Ordering::Acquire),
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

/// Convenience wrapper so callers get FsError-flavored IO errors.
pub(crate) fn stage_write(path: &std::path::Path, data: &[u8]) -> Result<(), alloyfs_proto::ErrorCode> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).or_code()?;
    }
    std::fs::write(path, data).or_code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloyfs_proto::FileKind;

    fn attr(size: u64, mtime_s: u64, version: u64) -> Attr {
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
        assert_eq!(c.read(&p, 0, 5).as_deref(), Some(&b"hello"[..]));
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
            c.read(&RelPath("e/x.txt".into()), 0, 1).as_deref(),
            Some(&b"1"[..])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
