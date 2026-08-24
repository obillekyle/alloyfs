//! Open handles: the table, and what a handle knows.
//!
//! A handle is the client's, not the server's — `OpenState` outlives any one
//! server fh so a reconnect can re-open underneath it. The bookkeeping here is
//! what lets a read find its fh, a write find its size, and a rename find every
//! handle it has to move.

use super::*;

impl RemoteFs {
    pub fn open(&self, ino: u64, flags: OpenFlags) -> Result<(u64, Attr), FsError> {
        self.open_hinted(ino, flags, true)
    }

    /// `open` for a caller that knows nobody is about to read this file.
    ///
    /// Windows is why this exists. The FSD's cached paging I/O may issue a
    /// Read against a handle the application opened write-only — an unaligned
    /// write is a read-modify-write of a page — so the mount cannot simply
    /// clear `flags.read`: the server would refuse exactly those reads. What
    /// it can say is that no read is *intended*, which is the only thing the
    /// head prefetch was ever keyed off. Without it, opening a file to
    /// overwrite it pulled 128 KiB of the contents about to be replaced
    /// across the wire, to drop them on the floor.
    pub fn open_no_head(&self, ino: u64, flags: OpenFlags) -> Result<(u64, Attr), FsError> {
        self.open_hinted(ino, flags, false)
    }

    fn open_hinted(&self, ino: u64, flags: OpenFlags, may_read: bool) -> Result<(u64, Attr), FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            return self.overlay_ref().open(&path, flags);
        }
        // A path the batcher still owes the server has no remote truth to
        // open yet; the queue lands first, and its failures surface here
        // rather than as a mystery later.
        if self.batch.as_ref().is_some_and(|b| b.involves(&path)) {
            self.barrier_for(&path)?;
        }
        // The barrier alone is not enough, and this is why: an UNSEALED
        // pending file's bytes live on its own handle, not in the queue, so
        // `barrier_for` — which drains the queue and checks damage — leaves it
        // exactly where it was. The open then went to the wire, and the
        // agent's `open` has no create, so it answered NotFound for a file
        // `ls` had just listed.
        //
        // `rename` and `copy_range` already call this for the same reason.
        // Reachable through all three backends, and the same root cause made
        // `flock()` and `chmod +x` fail on a file you had just written and
        // still held open.
        self.materialize_open_pending(&path)?;
        // Read-only, and the cache already holds this file at the version the
        // last listing reported? Then the server has nothing to add. Skipping
        // the round trip here is what makes browsing a remote tree bearable:
        // `ls`, git and Explorer's property handlers all open files they never
        // read a byte of, and each of those opens was costing a full RTT.
        //
        // The freshness test is the SAME one the answer would have been fed
        // through — `fresh_for` compares size, mtime and version — applied to
        // the attribute the readdir already cached instead of to one fetched
        // again. That attribute is at most one attr-TTL old and the event stream
        // invalidates it sooner, which is the freshness contract every other
        // read on this mount already runs under.
        //
        // Writes, truncation, append and O_EXCL all still go to the server: a
        // cached blob says what the file WAS, which is no basis for changing
        // it.
        if flags.read && !flags.write && !flags.truncate && !flags.append && !flags.excl {
            if let Some(attr) = self.cached_attr_fresh(ino) {
                if self.cache.as_ref().is_some_and(|c| c.fresh_for(&path, &attr)) {
                    let fh = LAZY_FH_BIT | self.next_lazy_fh.fetch_add(1, Ordering::Relaxed);
                    self.track_open(
                        fh,
                        OpenState {
                            path,
                            flags,
                            server_fh: AtomicU64::new(NO_SERVER_FH),
                            cache_ok: AtomicBool::new(true),
                            warm_asked: AtomicBool::new(false),
                            warm_fill: std::sync::Mutex::new(None),
                            wrote: AtomicBool::new(false),
                            ra: ReadAhead::new(),
                            lock: std::sync::Mutex::new(Vec::new()),
                            poisoned: AtomicBool::new(false),
                            pending_new: None,
                            blob: std::sync::RwLock::new(None),
                            version: AtomicU64::new(attr.version),
                            size: AtomicU64::new(attr.size),
                            mtime_ns: AtomicU64::new(crate::autocache::mtime_ns(attr.mtime) as u64),
                        },
                    );
                    return Ok((fh, attr));
                }
            }
        }
        // v9+: a plain read-open carries the head of the file back with the
        // handle, folding the open+first-read pair — the residual per-file
        // cost after ReadMany — into one exchange. Only when the auto-cache
        // will not already answer the read locally (cache_ok below), and only
        // for opens that will read: head bytes for a write-only or truncating
        // open would be fetched to be thrown away. `len == 0` spells exactly
        // that on the wire.
        // `may_read` is the caller's own statement of intent, which on Windows
        // is strictly better information than `flags.read` — see
        // `open_no_head`.
        let wants_head = may_read && flags.read && !flags.truncate;
        let (fh, attr, head) = if self.conn().proto >= 9 {
            let len = if wants_head { DATA_CHUNK } else { 0 };
            let resp = self.call(Request::OpenRead {
                path: path.clone(),
                flags,
                len,
            })?;
            expect_resp!(resp, Response::OpenedData { fh, attr, data } => (fh, attr, Some(data)))
        } else {
            let resp = self.call(Request::Open {
                path: path.clone(),
                flags,
            })?;
            expect_resp!(resp, Response::Opened { fh, attr } => (fh, attr, None))
        };
        debug_assert!(fh & OVERLAY_FH_BIT == 0, "server fh collides with overlay bit");
        self.cache_attr(ino, attr);
        let cache_ok = self.cache.as_ref().is_some_and(|c| c.fresh_for(&path, &attr));
        let ra = ReadAhead::new();
        // Plant the head where the first read will look. Retained block 0 is
        // exactly what a post-open read at offset 0 consults, so a small file
        // is now open+read in one round trip and read locally after. An empty
        // head is NOT planted: an empty retained block would answer reads at
        // offset 0 with EOF for a file that merely declined to send bytes.
        if let Some(data) = head {
            if !data.is_empty() && !cache_ok {
                ra.retain(0, data);
            }
        }
        self.track_open(
            fh,
            OpenState {
                path,
                flags,
                server_fh: AtomicU64::new(fh),
                cache_ok: AtomicBool::new(cache_ok),
                warm_asked: AtomicBool::new(false),
                warm_fill: std::sync::Mutex::new(None),
                wrote: AtomicBool::new(false),
                ra,
                lock: std::sync::Mutex::new(Vec::new()),
                poisoned: AtomicBool::new(false),
                pending_new: None,
                blob: std::sync::RwLock::new(None),
                version: AtomicU64::new(attr.version),
                size: AtomicU64::new(attr.size),
                mtime_ns: AtomicU64::new(crate::autocache::mtime_ns(attr.mtime) as u64),
            },
        );
        Ok((fh, attr))
    }

    /// Fetch one whole DATA_CHUNK-aligned block on `conn` (async).
    pub(super) async fn fetch_block(conn: Arc<MuxConnection>, server_fh: u64, block: u64) -> Option<Bytes> {
        let offset = block * DATA_CHUNK as u64;
        match conn
            .request(Request::Read {
                fh: server_fh,
                offset,
                len: DATA_CHUNK,
            })
            .await
        {
            Ok(Ok(Response::Data(data))) => Some(data),
            _ => None,
        }
    }

    /// v11+: change the Windows attribute bits (Hidden/System) on `ino`.
    ///
    /// Masked intents, resolved server-side; the reply's attrs re-patch the
    /// caches so the next stat and the parent's listing tell the new truth.
    /// Overlay-routed paths apply locally through their own metadata (the
    /// overlay file IS the file). A pre-v11 server keeps the historical
    /// accepted-and-dropped behaviour — the request is never sent.
    pub fn set_win_attrs(&self, ino: u64, set: u32, clear: u32) -> Result<Attr, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            // An overlay file exists only here, so `SetWinAttrs` has nothing
            // to reach — but the bits still have to be APPLIED, to the local
            // file, by us. Returning `getattr` instead (which this did) hands
            // back the unchanged attributes and reads as success: Explorer's
            // Hidden tick appeared to work and changed nothing, on files and
            // folders alike.
            let attr = self.overlay_ref().set_win_attrs(&path, set, clear)?;
            self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
            self.cache_attr(ino, attr);
            return Ok(attr);
        }
        if self.conn().proto < 11 {
            // The old contract, kept exactly: acknowledged, unpersisted. A
            // pre-v11 agent has no `SetWinAttrs` verb, and refusing would
            // break mounts against one — so this stays a no-op on purpose.
            //
            // But it says so now. This is the same silent shape as the two
            // overlay bugs found today (accept, discard, report the old state
            // as success), and the only thing separating it from those is that
            // it is deliberate. A line in the log is what lets someone
            // diagnosing "Hidden does not stick" tell the two apart.
            tracing::debug!(
                %path,
                proto = self.conn().proto,
                "SetWinAttrs needs proto v11; acknowledged without persisting"
            );
            return self.getattr(ino);
        }
        if self.batch.as_ref().is_some_and(|b| b.involves(&path)) {
            self.barrier_for(&path)?;
        }
        let attr = expect_resp!(
            self.call(Request::SetWinAttrs {
                path: path.clone(),
                set,
                clear,
            })?,
            Response::Attr(attr) => attr
        );
        self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        Ok(attr)
    }

    /// Materialize the OPEN pending-new file at `path`, if one exists — the
    /// prologue for operations that need the server to KNOW the path while
    /// its handle is still accumulating locally (rename endpoints). A
    /// no-op for everything else.
    /// Register an open handle, keeping [`Self::open_by_path`] in step.
    ///
    /// The guard from each map is released before the other is touched, so
    /// the two are never held together and the pair cannot deadlock.
    pub(crate) fn track_open(&self, fh: u64, state: OpenState) {
        let path = state.path.clone();
        self.open_files.insert(fh, state);
        self.open_by_path.entry(path).or_default().push(fh);
    }

    /// Drop an open handle and its index entry, returning the state the way
    /// `DashMap::remove` would.
    pub(crate) fn untrack_open(&self, fh: u64) -> Option<OpenState> {
        let (_, state) = self.open_files.remove(&fh)?;
        let empty = match self.open_by_path.get_mut(&state.path) {
            Some(mut handles) => {
                handles.retain(|&h| h != fh);
                handles.is_empty()
            }
            None => false,
        };
        // Only after the guard above is gone: removing the entry while
        // holding a reference into it would wait on this same shard.
        if empty {
            self.open_by_path.remove(&state.path);
        }
        Some(state)
    }

    /// Does the path index still describe `open_files` exactly?
    ///
    /// A test and debug aid, not part of the working API. It is exposed
    /// because the index is an optimization that turns into a correctness bug
    /// the moment it drifts: a missing entry means an event about a file
    /// silently fails to invalidate the handle reading it, and stale bytes get
    /// served with no error anywhere. Anything that changes how handles are
    /// added or dropped should assert this afterwards. O(handles), so it is
    /// checked at the end of a test rather than inside the code under test.
    pub fn open_index_is_consistent(&self) -> bool {
        let indexed: usize = self.open_by_path.iter().map(|e| e.value().len()).sum();
        if indexed != self.open_files.len() {
            return false;
        }
        self.open_files
            .iter()
            .all(|e| self.handles_on(&e.value().path).contains(e.key()))
    }

    /// How many handles are open right now. Test and debug aid, alongside
    /// [`Self::open_index_is_consistent`]: a release path that forgets to
    /// remove its entry leaks silently, and this is what makes that visible.
    pub fn open_handle_count(&self) -> usize {
        self.open_files.len()
    }

    /// The handles currently open on `path`, as an owned list.
    ///
    /// Owned rather than a guard because every caller then reaches back into
    /// `open_files`, and copying a handful of `u64`s is a far cheaper way to
    /// keep the two maps' locks from ever overlapping than reasoning about
    /// which order they are taken in.
    pub(crate) fn handles_on(&self, path: &RelPath) -> Vec<u64> {
        self.open_by_path.get(path).map(|e| e.clone()).unwrap_or_default()
    }

    pub(super) fn materialize_open_pending(&self, path: &RelPath) -> Result<(), FsError> {
        let fh = self
            .handles_on(path)
            .into_iter()
            .find(|fh| self.open_files.get(fh).is_some_and(|e| e.pending_new.is_some()));
        match fh {
            Some(fh) => self.materialize_pending(fh),
            None => Ok(()),
        }
    }

    /// A pending NEW file outgrew the batcher — a non-sequential write, or
    /// size past the cap. Everything queued before it flushes first (order),
    /// then the file takes the classic path: a server create, the buffered
    /// bytes pushed through ordinary writes, and the handle rebound to the
    /// server's. Write-through resumes for this fh from here.
    pub(super) fn materialize_pending(&self, fh: u64) -> Result<(), FsError> {
        self.flush_batch();
        let (path, buf, mode, cancelled) = {
            let Some(mut e) = self.open_files.get_mut(&fh) else {
                return Err(ErrorCode::BadHandle.into());
            };
            let Some(pending) = e.pending_new.take() else {
                return Ok(());
            };
            let p = pending.into_inner().unwrap();
            (e.path.clone(), p.data, p.mode, p.cancelled)
        };
        if cancelled {
            // Unlinked while open, then written past the cap: the data has
            // nowhere durable to go — POSIX would keep it in the dead inode,
            // which this client cannot fabricate remotely. EIO over silence.
            return Err(ErrorCode::Io.into());
        }
        if let Some(batch) = &self.batch {
            batch.forget(&path);
        }
        let (sfh, _attr) = expect_resp!(
            self.call(Request::Create {
                path: path.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..OpenFlags::default()
                },
                mode,
            })?,
            Response::Opened { fh, attr } => (fh, attr)
        );
        let mut off = 0u64;
        for chunk in buf.chunks(DATA_CHUNK as usize) {
            expect_resp!(
                self.call(Request::Write {
                    fh: sfh,
                    offset: off,
                    data: Bytes::copy_from_slice(chunk),
                    expect_version: None,
                })?,
                Response::Written { .. } | Response::WrittenAttr { .. } => ()
            );
            off += chunk.len() as u64;
        }
        if let Some(e) = self.open_files.get(&fh) {
            e.server_fh.store(sfh, Ordering::Release);
        }
        Ok(())
    }

    /// Seal a pending NEW file into the batcher queue, leaving the handle
    /// alive. Returns the path when something was sealed. Shared by release
    /// (which then drops the handle) and fsync (which then barriers).
    pub fn seal_pending(&self, fh: u64) -> Option<RelPath> {
        let mut e = self.open_files.get_mut(&fh)?;
        let pending = e.pending_new.take()?;
        let path = e.path.clone();
        drop(e);
        let p = pending.into_inner().unwrap();
        if p.cancelled {
            if let Some(b) = &self.batch {
                b.forget(&path);
            }
            return None;
        }
        let batch = self.batch.as_ref()?;
        batch.push(PendingOp::Write {
            path: path.clone(),
            mode: p.mode,
            data: p.data.into(),
        });
        // Queued claim taken; the unsealed one retires.
        batch.forget(&path);
        if batch.wants_flush() {
            self.flush_batch();
        }
        Some(path)
    }
}
