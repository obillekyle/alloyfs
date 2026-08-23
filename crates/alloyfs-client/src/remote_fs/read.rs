//! The read path.
//!
//! Blocks in, caller's buffer out. This is the hot path of the whole client:
//! everything is arranged to avoid a copy, to keep the readahead window full,
//! and to let the auto-cache answer without a round trip when it can.

use super::*;

impl RemoteFs {
    /// `read` that fills the caller's buffer directly on the WARM paths —
    /// a pending file's local bytes and the mapped blob copy straight into
    /// the buffer the kernel handed the mount, skipping the Vec every warm
    /// read otherwise allocates and copies through. Everything else falls
    /// back to [`Self::read`]: the cold paths are round-trip-dominated and
    /// one extra copy is invisible there. Returns bytes written; 0 at EOF.
    pub fn read_into(&self, fh: u64, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        if fh & OVERLAY_FH_BIT == 0 {
            self.check_poisoned(fh)?;
            if let Some(state) = self.open_files.get(&fh) {
                if let Some(pending) = &state.pending_new {
                    let p = pending.lock().unwrap();
                    let start = (offset as usize).min(p.data.len());
                    let end = (offset.saturating_add(buf.len() as u64) as usize).min(p.data.len());
                    buf[..end - start].copy_from_slice(&p.data[start..end]);
                    return Ok(end - start);
                }
                if self.cache.is_some() && state.cache_ok.load(Ordering::Relaxed) {
                    {
                        let held = state.blob.read().unwrap();
                        if let Some(b) = held.as_ref() {
                            return Ok(b.read_into(offset, buf));
                        }
                    }
                    // No retained mapping yet: map now, serve, retain — the
                    // same lazy path `read` takes. A vanished blob falls
                    // through to the network exactly as it does there.
                    if let Some(cache) = &self.cache {
                        if let Some(b) = cache.map_blob(&state.path) {
                            let n = b.read_into(offset, buf);
                            *state.blob.write().unwrap() = Some(b);
                            return Ok(n);
                        }
                    }
                    state.cache_ok.store(false, Ordering::Relaxed);
                    *state.blob.write().unwrap() = None;
                }
            }
            // The shard guard is out of scope here — the network path
            // re-acquires and may take handles out (documented as unsafe
            // under a held guard).
            //
            // Straight to the block assembly rather than back through `read`:
            // everything `read` would do before reaching it — the poisoned
            // check, the pending-file buffer, the auto-cache — has just been
            // done above, and going through `read` would mean assembling into
            // a fresh `Vec` only to copy it in here.
            return self.read_blocks_into(fh, offset, buf);
        }
        // Overlay handles keep the old route: their bytes come off the local
        // filesystem, where one more copy is not what the read costs.
        let data = self.read(fh, offset, buf.len() as u32)?;
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return self.overlay_ref().read(fh, offset, size);
        }
        self.check_poisoned(fh)?;
        // A pending NEW file's truth is its local buffer.
        if let Some(state) = self.open_files.get(&fh) {
            if let Some(pending) = &state.pending_new {
                let p = pending.lock().unwrap();
                let start = (offset as usize).min(p.data.len());
                let end = (offset.saturating_add(size as u64) as usize).min(p.data.len());
                return Ok(p.data[start..end].to_vec());
            }
        }
        // Auto-cache fast path: serve from the local blob when fresh, through
        // a mapping retained on the fh — see the `blob` field for why.
        if let (Some(cache), Some(state)) = (&self.cache, self.open_files.get(&fh)) {
            if state.cache_ok.load(Ordering::Relaxed) {
                let served = {
                    let held = state.blob.read().unwrap();
                    held.as_ref().map(|b| b.read(offset, size))
                };
                if let Some(data) = served {
                    return Ok(data);
                }
                if let Some(b) = cache.map_blob(&state.path) {
                    let data = b.read(offset, size);
                    *state.blob.write().unwrap() = Some(b);
                    return Ok(data);
                }
                // Blob vanished (eviction race): fall through to the network.
                state.cache_ok.store(false, Ordering::Relaxed);
                *state.blob.write().unwrap() = None;
            }
        }
        // Past the cache, so this read genuinely needs the server.
        let mut out = vec![0u8; size as usize];
        let n = self.read_blocks_into(fh, offset, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    /// The network half of [`Self::read`], writing straight into `buf`.
    ///
    /// Split out so that a caller who already owns a buffer — which is every
    /// mount backend, since the kernel hands one down — can have the blocks
    /// land in it directly. Assembling a read used to copy three times: each
    /// block into a scratch `Vec` sized for the WHOLE block range, then the
    /// requested window out of that into a second `Vec`, then that into the
    /// caller's buffer. A 1 MiB read moved 3 MiB. Now each block's
    /// contribution is copied once, into its final place.
    ///
    /// Returns bytes written, which is short at EOF and 0 past it.
    ///
    /// Callers must already have cleared the overlay, pending-file and
    /// auto-cache paths — `read` and `read_into` both do, in that order,
    /// before reaching here.
    /// How many bytes block `b` holds in a file of `size` — a full chunk, or
    /// the short tail.
    ///
    /// A held block must be read at its EXACT length. The blob is sparse and
    /// full-length, so asking for a whole chunk at the tail would read the
    /// zeros past EOF and hand them back as data.
    fn block_len(b: u64, size: u64) -> usize {
        let start = b * DATA_CHUNK as u64;
        if start >= size {
            return 0;
        }
        ((size - start).min(DATA_CHUNK as u64)) as usize
    }

    fn read_blocks_into(&self, fh: u64, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let size = buf.len() as u32;
        // If the open never took a handle out, take one now — this is the
        // eviction-race and partial-blob path, not the common one, and it must
        // not be reached while holding a shard guard.
        if self.server_fh(fh) == NO_SERVER_FH {
            self.server_fh_for_io(fh)?;
        }
        let Some(state) = self.open_files.get(&fh) else {
            // Untracked fh (walker): no readahead state to assemble from.
            let data = self.read_blocks_direct(fh, offset, size)?;
            buf[..data.len()].copy_from_slice(&data);
            return Ok(data.len());
        };
        let server_fh = state.server_fh.load(Ordering::Acquire);
        // This read is going to the server, so start (or continue) collecting
        // its blocks into a cache blob. The bytes are already being paid for;
        // writing them down as they pass is what makes a second read local
        // without a second transfer. See `WarmFill`.
        //
        // Skipped for handles that WROTE — the file is in flux, and release
        // re-fetches it anyway — and for pending new files, which have no
        // server copy yet.
        if self.cache.is_some()
            && !state.cache_ok.load(Ordering::Relaxed)
            && !state.wrote.load(Ordering::Relaxed)
            && state.pending_new.is_none()
            && !state.warm_asked.swap(true, Ordering::Relaxed)
        {
            if let Some(cache) = &self.cache {
                let size = state.size.load(Ordering::Relaxed);
                // Only worth collecting what the cache would actually keep.
                if size > 0 && size <= cache.warm_limit() {
                    let attr = alloyfs_proto::Attr {
                        kind: alloyfs_proto::FileKind::File,
                        size,
                        mtime: std::time::UNIX_EPOCH
                            + std::time::Duration::from_nanos(state.mtime_ns.load(Ordering::Relaxed)),
                        ctime: std::time::UNIX_EPOCH,
                        mode: 0o644,
                        version: state.version.load(Ordering::Relaxed),
                    };
                    // Resume a partial blob when one is on disk and still
                    // describes this version of the file; otherwise start a
                    // fresh staging file.
                    let fill = match cache.blocks_for(&state.path, &attr) {
                        Some(map) if !map.is_complete() => crate::autocache::WarmFill::resume(
                            cache.blob_final_path(&state.path),
                            size,
                            attr.version,
                            map,
                        ),
                        _ => None,
                    };
                    *state.warm_fill.lock().unwrap() = fill.or_else(|| {
                        crate::autocache::WarmFill::begin(
                            cache.warm_stage_path(&state.path, fh),
                            size,
                            attr.version,
                        )
                    });
                }
            }
        }
        let prefetch = state.ra.observe(offset, size);
        tracing::trace!(fh, offset, size, prefetch, "mount read");

        // Serve [offset, offset+size) from DATA_CHUNK-aligned blocks:
        // retained copies first (sub-chunk re-reads), then prefetched ones,
        // then concurrent fetches for whatever is left.
        let chunk = DATA_CHUNK as u64;
        let first_block = ReadAhead::block_of(offset);
        // saturating: an absurd offset from a direct-IO caller would otherwise
        // wrap, leaving last_block < first_block and an empty block range that
        // reads as a successful empty result rather than a refusal.
        let last_block = ReadAhead::block_of(offset.saturating_add(size.max(1) as u64 - 1));

        // Top up the window BEFORE waiting on this read's own blocks: the
        // prefetches ride the same connection while we block, so the pipe
        // stays full instead of draining once per kernel read.
        //
        // Bounded at EOF. `missing`'s second argument exists for exactly this
        // and was being handed u64::MAX, which makes its `.min()` a no-op — so
        // a sequential read near the tail of a file fired a full window of
        // requests PAST the end. They cost round trips, and their empty
        // results were then retained, seeding precisely the short blocks that
        // go on to answer a later read as a false EOF.
        //
        // A cold attr cache falls back to unbounded rather than guessing. This
        // bound is a prefetch hint: too low only forgoes readahead, and too
        // high is what the code did before.
        // From the handle, which has known the size since it was opened.
        // The attr cache is consulted only if the handle never learned one
        // (0 is the resting value for a file created empty, and re-reading
        // it there is free).
        let known_size = match state.size.load(Ordering::Relaxed) {
            0 => self
                .ino
                .ino_of(&state.path)
                .and_then(|ino| self.attr_cache.get(&ino).map(|hit| hit.0.size)),
            size => Some(size),
        };
        let eof_block_exclusive = known_size
            .filter(|size| *size > 0)
            .map(|size| ReadAhead::block_of(size - 1) + 1)
            .unwrap_or(u64::MAX);
        if prefetch {
            // Multi-stream: a long cold stream stripes its window across the
            // pool's extra connections — same window depth, more congestion
            // windows under it (see stream_pool.rs). Engaging is async: the
            // first rounds ride the primary while the pool dials, and an
            // empty `lanes` costs nothing. Unknown size (u64::MAX) never
            // engages — no point dialing for what may be two blocks.
            let lanes = match &self.stream_pool {
                Some(pool)
                    if eof_block_exclusive != u64::MAX
                        && eof_block_exclusive.saturating_sub(first_block)
                            >= crate::stream_pool::MIN_BLOCKS_AHEAD =>
                {
                    pool.lanes(&self.rt)
                }
                _ => Vec::new(),
            };
            let total_lanes = lanes.len() as u64 + 1;
            for b in state.ra.missing(last_block + 1, eof_block_exclusive) {
                let lane = (b % total_lanes) as usize;
                let task = if lane == 0 {
                    self.rt.spawn(Self::fetch_block(self.conn(), server_fh, b))
                } else {
                    let entry = lanes[lane - 1].clone();
                    self.rt.spawn(entry.fetch_block(state.path.clone(), b))
                };
                state.ra.put(b, task);
            }
        }

        use std::sync::atomic::Ordering::Relaxed;
        let mut ready: HashMap<u64, Bytes> = HashMap::new();
        let mut need: Vec<u64> = Vec::new();
        for b in first_block..=last_block {
            if let Some(data) = state.ra.retained(b) {
                state.ra.stats.retained_hits.fetch_add(1, Relaxed);
                ready.insert(b, data);
                continue;
            }
            match state.ra.take(b) {
                Some(task) => match self.rt.block_on(task) {
                    Ok(Some(data)) => {
                        state.ra.stats.window_hits.fetch_add(1, Relaxed);
                        ready.insert(b, data);
                    }
                    _ => need.push(b), // prefetch failed (old conn?) — refetch
                },
                None => need.push(b),
            }
        }
        if !need.is_empty() {
            // Blocks this file already has on disk are read locally instead
            // of over the wire — the whole point of keeping a partial blob.
            // Done before the network batch so the request only asks for what
            // is genuinely missing.
            if let Ok(guard) = state.warm_fill.try_lock() {
                if let Some(fill) = guard.as_ref() {
                    need.retain(|&b| {
                        let want = Self::block_len(b, state.size.load(Relaxed));
                        match fill.get(b, want) {
                            Some(data) => {
                                ready.insert(b, Bytes::from(data));
                                false // served locally; drop it from the ask
                            }
                            None => true,
                        }
                    });
                }
            }
        }
        if !need.is_empty() {
            state.ra.stats.sync_fetches.fetch_add(need.len() as u64, Relaxed);
            let conn = self.conn();
            let fetched = self.rt.block_on(async {
                futures::future::join_all(
                    need.iter()
                        .map(|&b| Self::fetch_block(conn.clone(), server_fh, b)),
                )
                .await
            });
            for (b, data) in need.iter().zip(fetched) {
                match data {
                    Some(d) => {
                        ready.insert(*b, d);
                    }
                    None => return Err(ErrorCode::Io.into()),
                }
            }
        }
        // Every block this read pulled is in hand. Write them into the warm
        // fill before they are copied out — the same bytes, no second fetch.
        // Completion commits the blob; a fill that never completes is dropped
        // with its staging file when the handle closes.
        if let (Some(cache), Ok(mut guard)) = (&self.cache, state.warm_fill.try_lock()) {
            if let Some(fill) = guard.as_mut() {
                let mut complete = false;
                for (b, data) in ready.iter() {
                    complete |= fill.put(*b, data);
                }
                if complete {
                    let size = fill.size();
                    let version = fill.version();
                    if fill.finish(&cache.blob_final_path(&state.path)) {
                        // Attrs as the file was when the fill began. A change
                        // mid-read moves the version, and `fresh_for` then
                        // refuses this blob at the next open rather than
                        // serving a mixture.
                        let attr = alloyfs_proto::Attr {
                            kind: alloyfs_proto::FileKind::File,
                            size,
                            mtime: std::time::UNIX_EPOCH
                                + std::time::Duration::from_nanos(state.mtime_ns.load(Ordering::Relaxed)),
                            ctime: std::time::UNIX_EPOCH,
                            mode: 0o644,
                            version,
                        };
                        cache.commit_warm(&state.path, &attr, cache.pin_match(&state.path));
                        tracing::debug!(path = %state.path, size, "warm fill committed from reads");
                    }
                }
                if complete {
                    *guard = None;
                }
            }
        }
        // Copy each block's contribution straight into the caller's buffer,
        // walking the absolute file offset forward as we go. A short block is
        // EOF and ends the read.
        let mut written = 0usize;
        let mut pos = offset;
        for b in first_block..=last_block {
            // Every block in the range was fetched above or the read already
            // failed — but this runs on a mount dispatcher thread, where a
            // broken invariant must become EIO, never a panic.
            let Some(data) = ready.get(&b) else {
                tracing::error!(block = b, "readahead invariant broken: block missing");
                return Err(ErrorCode::Io.into());
            };
            // Where this read starts inside this block: the requested offset
            // for the first block, and zero for every one after it.
            let within = (pos - b * chunk) as usize;
            if within < data.len() {
                let take = (data.len() - within).min(buf.len() - written);
                buf[written..written + take].copy_from_slice(&data[within..within + take]);
                written += take;
                pos += take as u64;
            }
            // A short block is the end of the file; a full buffer is the end
            // of what was asked for.
            if data.len() < DATA_CHUNK as usize || written == buf.len() {
                break;
            }
        }

        // Retain this read's blocks: the next sub-chunk kernel read of the
        // same 128 KiB block must not pay a fresh RTT for bytes we had.
        for (b, data) in ready {
            state.ra.retain(b, data);
        }
        Ok(written)
    }

    /// The pre-readahead read path, kept for fhs we don't track (the cache
    /// walker opens raw server fhs that never enter `open_files`).
    fn read_blocks_direct(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        let end = offset + size as u64;
        let chunks: Vec<(u64, u32)> = {
            let mut v = Vec::new();
            let mut pos = offset;
            while pos < end {
                let want = ((end - pos) as u32).min(DATA_CHUNK);
                v.push((pos, want));
                pos += want as u64;
            }
            v
        };
        let conn = self.conn();
        let responses = self.rt.block_on(async {
            futures::future::join_all(chunks.iter().map(|&(pos, want)| {
                conn.request(Request::Read {
                    fh,
                    offset: pos,
                    len: want,
                })
            }))
            .await
        });
        let mut out = Vec::with_capacity(size as usize);
        for (resp, &(_, want)) in responses.into_iter().zip(&chunks) {
            let chunk = expect_resp!(resp??, Response::Data(chunk) => chunk);
            let got = chunk.len() as u32;
            out.extend_from_slice(&chunk);
            if got < want {
                break; // EOF inside this chunk
            }
        }
        Ok(out)
    }

    // --------------------------------------------------------------- writes

}
