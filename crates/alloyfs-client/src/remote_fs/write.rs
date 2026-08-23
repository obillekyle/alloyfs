//! The write path, and the batcher that settles it.
//!
//! Writes are acknowledged locally and settled against the server in batches,
//! so the two questions here are what a handle believes and what the server has
//! confirmed — and `flush` is where those have to agree.

use super::*;

impl RemoteFs {
    /// Write, discarding the server's post-write attributes. `write_at` is
    /// the same call for backends that can use them.
    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, FsError> {
        self.write_at(fh, offset, data).map(|(n, _)| n)
    }

    /// Write, handing back the file's attributes as the server saw them
    /// immediately afterwards — when the negotiated protocol carried them.
    ///
    /// `None` means "they did not come with the reply": a v4-or-older server,
    /// or a write routed to the local overlay. Callers that fill a kernel
    /// stat structure right after a write want the `Some` case; that is the
    /// Getattr round-trip which used to follow every write.
    ///
    /// Either way the attribute cache is left CORRECT for this path — either
    /// refreshed from the reply or, with nothing to refresh it with, dropped.
    pub fn write_at(&self, fh: u64, offset: u64, data: &[u8]) -> Result<(u32, Option<Attr>), FsError> {
        if fh & OVERLAY_FH_BIT != 0 {
            return self.overlay_ref().write(fh, offset, data).map(|n| (n, None));
        }
        self.check_poisoned(fh)?;
        // A pending NEW file accumulates locally while the writes stay
        // sequential and small; the first write that breaks either rule
        // materializes it onto the classic path below.
        if let Some(state) = self.open_files.get(&fh) {
            if let Some(pending) = &state.pending_new {
                let appended = {
                    let mut p = pending.lock().unwrap();
                    if offset as usize == p.data.len() && p.data.len() + data.len() <= PENDING_FILE_MAX {
                        p.data.extend_from_slice(data);
                        Some(p.data.len() as u64)
                    } else {
                        None
                    }
                };
                let path = state.path.clone();
                drop(state);
                match appended {
                    Some(size) => {
                        let ino = self.ino.ino_of(&path);
                        let now = std::time::SystemTime::now();
                        let base = ino.and_then(|i| self.cached_attr_fresh(i));
                        let attr = Attr {
                            size,
                            mtime: now,
                            ..base.unwrap_or(Attr {
                                kind: alloyfs_proto::FileKind::File,
                                size,
                                mtime: now,
                                ctime: now,
                                mode: 0o666,
                                version: 0,
                            })
                        };
                        if let Some(ino) = ino {
                            self.cache_attr(ino, attr);
                        }
                        return Ok((data.len() as u32, Some(attr)));
                    }
                    None => self.materialize_pending(fh)?,
                }
            }
        }
        let server_fh = self.server_fh_for_io(fh)?;
        // The version this write is allowed to overwrite. `None` when the flag
        // is off (the server then never checks) or when we never learned one.
        //
        // It has to be threaded through the chunk loop rather than read once:
        // our OWN write bumps the server's version, so a large write sending
        // the same expectation for every chunk would conflict with itself
        // after the first one.
        let mut expect = if self.detect_conflicts {
            self.open_files
                .get(&fh)
                .map(|s| s.version.load(Ordering::Relaxed))
                .filter(|v| *v != 0)
        } else {
            None
        };
        // The freshest reply's attributes describe the file as it now
        // stands; whichever branch below runs leaves them here for the
        // shared cache maintenance after it.
        let mut fresh: Option<Attr> = None;
        if expect.is_none() && data.len() > DATA_CHUNK as usize {
            // No expectation to thread means the chunks are independent:
            // one fh, disjoint ranges. Sending them one blocking RTT at a
            // time priced a 1 MiB cache-manager write at 8 serial round
            // trips while the read side kept a 32-block window in flight —
            // this is the write side getting the same treatment, with the
            // cold-read fallback's join_all idiom. The failure shape moves
            // with it, deliberately: a connection dying mid-write can now
            // leave holes where the serial loop left a clean prefix. A
            // torn write-through was always unspecified; --detect-conflicts
            // keeps the serial loop below.
            let conn = self.conn();
            let replies = self.rt.block_on(async {
                futures::future::join_all(data.chunks(DATA_CHUNK as usize).enumerate().map(|(i, chunk)| {
                    conn.request(Request::Write {
                        fh: server_fh,
                        offset: offset + (i as u64) * u64::from(DATA_CHUNK),
                        data: Bytes::copy_from_slice(chunk),
                        expect_version: None,
                    })
                }))
                .await
            });
            // The server may process the chunks in any order, but versions
            // are monotonic per path — the highest version marks the reply
            // that observed the youngest state among ours. One guard on its
            // attrs: an earlier-processed chunk's size must never be served
            // as final, so size is raised to at least this write's own end
            // (concurrent growth by another writer can only agree or
            // exceed; nothing here truncates).
            let mut latest_version = 0u64;
            for (resp, chunk) in replies.into_iter().zip(data.chunks(DATA_CHUNK as usize)) {
                let (n, new_version, attr) = match resp?? {
                    Response::Written { n, new_version, .. } => (n, new_version, None),
                    Response::WrittenAttr { n, attr } => (n, attr.version, Some(attr)),
                    other => {
                        tracing::error!(?other, "unexpected response variant");
                        return Err(ErrorCode::Io.into());
                    }
                };
                if (n as usize) < chunk.len() {
                    // The agent writes whole chunks or errors; a short count
                    // would leave a mid-buffer hole no retry here can see.
                    tracing::error!(fh, n, want = chunk.len(), "server short-wrote a chunk");
                    return Err(ErrorCode::Io.into());
                }
                if new_version > latest_version {
                    latest_version = new_version;
                    fresh = attr;
                }
            }
            fresh = fresh.map(|mut a| {
                a.size = a.size.max(offset + data.len() as u64);
                a
            });
            if let Some(state) = self.open_files.get(&fh) {
                state.version.store(latest_version, Ordering::Relaxed);
            }
        } else {
            let mut pos = 0usize;
            while pos < data.len() {
                let chunk = &data[pos..(pos + DATA_CHUNK as usize).min(data.len())];
                let written = match self.call(Request::Write {
                    fh: server_fh,
                    offset: offset + pos as u64,
                    data: Bytes::copy_from_slice(chunk),
                    expect_version: expect,
                }) {
                    Ok(resp) => resp,
                    // A conflict is a refusal now, not a flag on a write that
                    // has already happened: nothing was written for this
                    // chunk. Earlier chunks of a large write may have landed,
                    // which is the same partial-write hazard any interrupted
                    // write-through has — worth logging the offset so it is
                    // diagnosable.
                    Err(FsError::Remote(ErrorCode::Conflict)) => {
                        tracing::warn!(
                            fh,
                            offset = offset + pos as u64,
                            bytes_already_written = pos,
                            "refused: the file changed on another machine (--detect-conflicts)"
                        );
                        return Err(ErrorCode::Conflict.into());
                    }
                    Err(e) => return Err(e),
                };
                // Both shapes are legal: v5+ servers answer with the
                // attributes, everything older with the byte count and
                // version alone. The version means the same thing in both —
                // `Attr::version` IS what `Written::new_version` carried.
                let (n, new_version) = match written {
                    Response::Written { n, new_version, .. } => {
                        fresh = None;
                        (n, new_version)
                    }
                    Response::WrittenAttr { n, attr } => {
                        let version = attr.version;
                        fresh = Some(attr);
                        (n, version)
                    }
                    other => {
                        tracing::error!(?other, "unexpected response variant");
                        return Err(ErrorCode::Io.into());
                    }
                };
                if let Some(state) = self.open_files.get(&fh) {
                    state.version.store(new_version, Ordering::Relaxed);
                }
                if expect.is_some() {
                    expect = Some(new_version);
                }
                pos += n as usize;
                if n == 0 {
                    return Err(ErrorCode::Io.into());
                }
            }
        }
        // Our own writes never come back as events (server strips
        // self-origin), so cache coherence is synchronous, right here.
        if let Some(state) = self.open_files.get(&fh) {
            state.wrote.store(true, Ordering::Relaxed);
            // Keep the readahead bound honest about what we just wrote:
            // the server's answer when it gave one, otherwise at least as
            // far as this write reached.
            let grown = fresh
                .map(|a| a.size)
                .unwrap_or_else(|| offset + data.len() as u64);
            state.size.fetch_max(grown, Ordering::Relaxed);
            self.mark_path_written(
                &state.path,
                fresh.zip(self.ino.ino_of(&state.path)).map(|(a, i)| (i, a)),
            );
            // Size, mtime and version all just changed. With the reply
            // carrying them the cached attr is REPLACED rather than dropped,
            // and the next stat of this file — which every mount does
            // immediately, to fill the write's own reply — is a memory hit
            // instead of a Getattr. Without them the entry has to go: serving
            // a pre-write size would be worse than paying for the round-trip.
            match (fresh, self.ino.ino_of(&state.path)) {
                (Some(attr), Some(ino)) => self.cache_attr(ino, attr),
                (None, Some(ino)) => {
                    self.invalidate_attr(ino);
                }
                (_, None) => {} // never stat'ed through this mount: nothing cached
            }
        }
        Ok((data.len() as u32, fresh))
    }

    /// Drain the write batcher to the server and apply every outcome:
    /// server attrs re-patch what the optimistic ack guessed, refusals
    /// restore the caches to the server's truth and land in the damage
    /// ledger. No-op without a batcher.
    ///
    /// This is the BARRIER primitive — fsync, flush, rename, unmount, a
    /// cold listing and the lock ops all promise, by returning, that
    /// everything queued before them is on the server. It therefore may
    /// NOT skip on an empty queue: a concurrent flush (the age flusher)
    /// empties the queue the instant it drains, a good while before those
    /// bytes reach the wire, so `is_empty()` reads true during precisely
    /// the window a barrier exists to cover. Taking the flush lock — which
    /// `flush_with` holds across its send — is what makes the promise
    /// true; an uncontended lock costs nothing next to the syscall or
    /// round trip every one of these callers is already making.
    pub(crate) fn flush_batch(&self) {
        let Some(batch) = &self.batch else { return };
        batch.flush_with(
            |req| match self.call(req) {
                Ok(resp) => Ok(resp),
                Err(FsError::Remote(code)) => Err(code),
                Err(FsError::Transport(_)) => Err(ErrorCode::Io),
            },
            |op, outcome, last| match (op, outcome) {
                // Patch only as the path's LAST claim: an older write's attrs
                // landing over a newer acknowledged remove re-inserted files
                // the application had deleted (measured — see flush_with).
                (PendingOp::Write { path, .. }, Ok(Some(attr))) if last => {
                    let ino = self.ino.get_or_alloc(path.clone());
                    self.patch_parent_dir(path, ListingPatch::Upsert(ino, *attr));
                    self.cache_attr(ino, *attr);
                }
                // The server's post-setattr attrs replace the merged local
                // echo — the true server mtime granularity, version, and
                // win bits land here.
                (PendingOp::Setattr { path, .. }, Ok(Some(attr))) if last => {
                    if let Some(ino) = self.ino.ino_of(path) {
                        self.patch_parent_dir(path, ListingPatch::Upsert(ino, *attr));
                        self.cache_attr(ino, *attr);
                    }
                }
                (PendingOp::Setattr { .. }, Ok(_)) => {} // superseded, or no attrs
                (PendingOp::Write { .. }, Ok(_)) => {}   // superseded by a newer op
                (PendingOp::Remove { .. }, Ok(_)) => {}  // patched at enqueue
                (PendingOp::Write { path, .. }, Err(_))
                | (PendingOp::Remove { path, .. }, Err(_))
                | (PendingOp::Setattr { path, .. }, Err(_)) => {
                    self.settle_failures.fetch_add(1, Ordering::Relaxed);
                    if last {
                        // The optimistic ack promised something the server
                        // refused: the caches stop vouching for this path.
                        // With a NEWER op still queued, its settle decides
                        // instead — this outcome is already history.
                        self.invalidate_parent_dir(path);
                        if let Some(ino) = self.ino.ino_of(path) {
                            self.invalidate_attr(ino);
                        }
                    }
                }
            },
        );
    }

    /// Flush the batcher and report what broke for `path` — the fsync
    /// promise: returning Ok means the server has everything this path was
    /// ever acknowledged for.
    pub(crate) fn barrier_for(&self, path: &RelPath) -> Result<(), FsError> {
        let Some(batch) = &self.batch else {
            return Ok(());
        };
        self.flush_batch();
        match batch.take_damage(path) {
            Some(code) => Err(code.into()),
            None => Ok(()),
        }
    }

    /// Does a COMPLETE cached listing of `dir` decide whether `name` exists?
    /// `None` when no listing can vouch — the caller must ask the server.
    pub(super) fn knows_child_exists(&self, parent: u64, dir: &RelPath, name: &str) -> Option<bool> {
        if let Some(hit) = self.dir_cache.get(&parent) {
            let (entries, when) = &*hit;
            if when.elapsed() < self.dir_ttl() {
                return Some(entries.binary_search_by(|(n, _, _)| n.as_str().cmp(name)).is_ok());
            }
        }
        self.warm
            .get(dir)
            .map(|w| w.binary_search_by(|(n, _)| n.as_str().cmp(name)).is_ok())
    }

    /// Does a COMPLETE cached listing prove `dir` is empty? Removing a
    /// directory optimistically is only honest when the server could not
    /// answer NotEmpty.
    pub(super) fn knows_dir_empty(&self, path: &RelPath) -> bool {
        if let Some(ino) = self.ino.ino_of(path) {
            if let Some(hit) = self.dir_cache.get(&ino) {
                let (entries, when) = &*hit;
                if when.elapsed() < self.dir_ttl() {
                    return entries.is_empty();
                }
            }
        }
        self.warm.get(path).map(|w| w.is_empty()).unwrap_or(false)
    }
}
