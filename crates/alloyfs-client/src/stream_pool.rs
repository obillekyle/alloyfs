//! Extra data connections for cold sequential streams — multi-stream
//! transport.
//!
//! A cold stream's throughput over one TCP connection is bounded by that
//! connection's congestion window against the round-trip time; the
//! request-level readahead window keeps the PIPELINE full, but every byte
//! still rides one cwnd. N parallel connections multiply the window — the
//! same lever rclone ships as --multi-thread-streams.
//!
//! The pool is lazy, bounded, and disposable. It dials only when a stream
//! qualifies (a cold file with at least [`MIN_BLOCKS_AHEAD`] still ahead of
//! the reader), it prunes dead members on use, and it drops every extra
//! after an idle minute — an ssh:// mount spawns a remote process per
//! connection, so idle extras are not free to keep.
//!
//! Correctness never depends on the pool. Handles are session-scoped, so
//! each pool connection opens its own fh per file (once, then cached);
//! every striped block that fails for any reason — dial refused, connection
//! died, open failed — comes back `None`, and the read path's existing miss
//! handling refetches it on the primary connection. The pool only ever adds
//! lanes; it can never lose a byte.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use alloyfs_proto::{OpenFlags, RelPath, Request, Response, DATA_CHUNK};
use alloyfs_transport::MuxConnection;
use bytes::Bytes;
use dashmap::DashMap;

use crate::options::Dialer;

/// A stream must still have at least this much ahead of it (× 128 KiB =
/// 4 MiB) to engage the pool: dialing costs a round trip or an ssh spawn,
/// and a short file is done on the primary before an extra lane could help.
pub(crate) const MIN_BLOCKS_AHEAD: u64 = 32;

/// Idle this long → every extra connection is dropped (their agent sessions
/// end, ssh children exit). The next qualifying stream re-dials.
const IDLE_DROP: Duration = Duration::from_secs(60);

/// One extra connection: its own session, its own handles.
pub(crate) struct PoolEntry {
    conn: Arc<MuxConnection>,
    /// fh per path ON THIS SESSION — a striped Read must use a handle its
    /// own connection opened.
    fhs: DashMap<RelPath, u64>,
    /// Serializes open-if-absent so a burst of first blocks for one file
    /// opens once, not once per block.
    opening: tokio::sync::Mutex<()>,
}

impl PoolEntry {
    async fn fh_for(&self, path: &RelPath) -> Option<u64> {
        if let Some(fh) = self.fhs.get(path) {
            return Some(*fh);
        }
        let _g = self.opening.lock().await;
        if let Some(fh) = self.fhs.get(path) {
            return Some(*fh);
        }
        let resp = self
            .conn
            .request(Request::Open {
                path: path.clone(),
                flags: OpenFlags {
                    read: true,
                    ..OpenFlags::default()
                },
            })
            .await;
        match resp {
            Ok(Ok(Response::Opened { fh, .. })) => {
                self.fhs.insert(path.clone(), fh);
                Some(fh)
            }
            _ => None,
        }
    }

    /// One striped block; the same shape the primary's fetch has, so the
    /// readahead window holds both interchangeably. `None` on any failure —
    /// the read path's miss handling refetches on the primary.
    pub(crate) async fn fetch_block(self: Arc<Self>, path: RelPath, block: u64) -> Option<Bytes> {
        let fh = self.fh_for(&path).await?;
        let offset = block * DATA_CHUNK as u64;
        match self
            .conn
            .request(Request::Read {
                fh,
                offset,
                len: DATA_CHUNK,
            })
            .await
        {
            Ok(Ok(Response::Data(data))) => Some(data),
            _ => None,
        }
    }
}

pub(crate) struct StreamPool {
    dialer: Dialer,
    export: String,
    /// Extra connections aimed for (the primary makes it lanes = target+1).
    target: usize,
    conns: Mutex<Vec<Arc<PoolEntry>>>,
    /// One fill task at a time.
    dialing: AtomicBool,
    /// Connections successfully established over the pool's lifetime —
    /// what the loopback test pins engagement with.
    established: AtomicUsize,
    last_use: Mutex<Instant>,
}

impl StreamPool {
    pub(crate) fn new(
        dialer: Dialer,
        export: String,
        target: usize,
        rt: &tokio::runtime::Handle,
    ) -> Arc<Self> {
        let pool = Arc::new(Self {
            dialer,
            export,
            target,
            conns: Mutex::new(Vec::new()),
            dialing: AtomicBool::new(false),
            established: AtomicUsize::new(0),
            last_use: Mutex::new(Instant::now()),
        });
        // Weak, like the mux pinger: the reaper must never keep a dropped
        // mount's pool alive by itself.
        rt.spawn(Self::reap(Arc::downgrade(&pool)));
        pool
    }

    /// The ready lanes right now, topping the pool up in the background when
    /// it is short. Never blocks and never fails — an empty return means
    /// every block rides the primary this round, and a later round finds
    /// the dialed lanes ready.
    pub(crate) fn lanes(self: &Arc<Self>, rt: &tokio::runtime::Handle) -> Vec<Arc<PoolEntry>> {
        *self.last_use.lock().unwrap() = Instant::now();
        let mut conns = self.conns.lock().unwrap();
        conns.retain(|e| !e.conn.is_closed());
        if conns.len() < self.target && !self.dialing.swap(true, Ordering::AcqRel) {
            rt.spawn(self.clone().fill());
        }
        conns.clone()
    }

    /// Dial until the pool is full or a dial fails; failures leave the pool
    /// short and the next `lanes` call tries again.
    async fn fill(self: Arc<Self>) {
        loop {
            let have = self.conns.lock().unwrap().len();
            if have >= self.target {
                break;
            }
            let Ok(conn) = (self.dialer)().await else { break };
            let attached = conn
                .request(Request::Attach {
                    export: self.export.clone(),
                })
                .await;
            if !matches!(attached, Ok(Ok(Response::AttachOk { .. }))) {
                break;
            }
            self.established.fetch_add(1, Ordering::Relaxed);
            self.conns.lock().unwrap().push(Arc::new(PoolEntry {
                conn,
                fhs: DashMap::new(),
                opening: tokio::sync::Mutex::new(()),
            }));
        }
        self.dialing.store(false, Ordering::Release);
    }

    /// Release every pool handle for `path` — called when the mount closes
    /// the file, so long-lived pool sessions don't accumulate open handles
    /// (and, on a Windows server, share locks) for files nobody reads.
    pub(crate) fn forget_path(&self, rt: &tokio::runtime::Handle, path: &RelPath) {
        let conns = self.conns.lock().unwrap().clone();
        for e in conns {
            if let Some((_, fh)) = e.fhs.remove(path) {
                let conn = e.conn.clone();
                rt.spawn(async move {
                    let _ = conn.send_oneway(Request::Release { fh }).await;
                });
            }
        }
    }

    /// Connections established over the pool's lifetime (not the live count).
    pub(crate) fn established(&self) -> usize {
        self.established.load(Ordering::Relaxed)
    }

    async fn reap(weak: Weak<Self>) {
        let mut tick = tokio::time::interval(IDLE_DROP / 2);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(pool) = weak.upgrade() else { break };
            let idle = pool.last_use.lock().unwrap().elapsed();
            if idle >= IDLE_DROP {
                // Dropping the entries drops their connections; the agent
                // sessions end and release every handle they held.
                pool.conns.lock().unwrap().clear();
            }
        }
    }
}
