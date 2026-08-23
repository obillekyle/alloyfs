//! Attributes, lookup and directory listings.
//!
//! The read-only half of the namespace: what a path IS, and what a directory
//! contains. Everything here can be answered from a cache or an index when one
//! is live, which is why the overlay merge lives here too — a listing has to
//! show local-only children alongside the server's.

use super::*;

impl RemoteFs {
    pub fn getattr(&self, ino: u64) -> Result<Attr, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            // Local stat is ~µs; bypass the TTL cache entirely.
            return self.overlay_ref().getattr(&path);
        }
        if let Some(hit) = self.attr_cache.get(&ino) {
            let (attr, when) = *hit;
            if when.elapsed() < self.attr_ttl() {
                return Ok(attr);
            }
        }
        // The WinFsp backend resolves names through getattr rather than
        // lookup, so the negative half of the listing cache has to be
        // consulted here too — a missing name allocates a fresh ino, lands
        // here with no attr cached, and used to pay a round trip to be told
        // "no" every single time.
        if let Some((parent, name)) = path.split() {
            if let Some(pino) = self.ino.ino_of(&parent) {
                if let Some(hit) = self.dir_cache.get(&pino) {
                    let (entries, when) = &*hit;
                    if when.elapsed() < self.dir_ttl()
                        && entries
                            .binary_search_by(|(n, _, _)| n.as_str().cmp(name))
                            .is_err()
                    {
                        tracing::debug!(path = %path, "getattr: NEGATIVE from live listing");
                        return Err(ErrorCode::NotFound.into());
                    }
                }
            }
            // The warm tier answers BOTH halves where the live listing above
            // answers only the negative. The positive matters here because
            // warm listings have no TTL: an attr-cache expiry must not push a
            // stat of a token-proven entry back onto the wire, or a remount
            // would go cold again five seconds after it went warm.
            if let Some(w) = self.warm.get(&parent) {
                let found = w
                    .binary_search_by(|(n, _)| n.as_str().cmp(name))
                    .ok()
                    .map(|i| w[i].1);
                drop(w);
                return match found {
                    Some(attr) => {
                        self.cache_attr(ino, attr);
                        Ok(attr)
                    }
                    None => {
                        tracing::debug!(path = %path, "getattr: NEGATIVE from warm tier");
                        Err(ErrorCode::NotFound.into())
                    }
                };
            }
        }
        let attr = expect_resp!(self.call(Request::Getattr { path })?, Response::Attr(attr) => attr);
        self.cache_attr(ino, attr);
        Ok(attr)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Result<(u64, Attr), FsError> {
        let dir = self.path_of(parent)?;
        let path = dir.join(name);
        if self.is_overlay(&path) {
            let attr = self.overlay_ref().getattr(&path)?;
            let ino = self.ino.get_or_alloc(path);
            return Ok((ino, attr));
        }
        // A live listing of the parent answers this in BOTH directions. The
        // positive case is what saves the FUSE/kernel backends from one round
        // trip per entry after every readdir; the negative case is the bigger
        // one — the listing is complete, so a name it lacks does not exist,
        // and Windows and every resolver probe missing names relentlessly.
        // Overlay names never reach here (routed above), so absence from the
        // remote listing is the whole answer.
        if let Some(hit) = self.dir_cache.get(&parent) {
            let (entries, when) = &*hit;
            if when.elapsed() < self.dir_ttl() {
                return match entries.binary_search_by(|(n, _, _)| n.as_str().cmp(name)) {
                    Ok(i) => {
                        let (_, ino, attr) = &entries[i];
                        Ok((*ino, *attr))
                    }
                    Err(_) => Err(ErrorCode::NotFound.into()),
                };
            }
        }
        // The warm tier gives the same two answers when no live listing
        // does. Same completeness claim, different proof: the tree token the
        // walker verified at mount, kept honest by events instead of a TTL.
        // Feeding the attr cache on the way out is what lets the open that
        // follows this lookup take the lazy no-server path.
        if let Some(w) = self.warm.get(&dir) {
            let found = w
                .binary_search_by(|(n, _)| n.as_str().cmp(name))
                .ok()
                .map(|i| w[i].1);
            drop(w);
            return match found {
                Some(attr) => {
                    let ino = self.ino.get_or_alloc(path);
                    self.cache_attr(ino, attr);
                    Ok((ino, attr))
                }
                None => Err(ErrorCode::NotFound.into()),
            };
        }
        let attr =
            expect_resp!(self.call(Request::Getattr { path: path.clone() })?, Response::Attr(attr) => attr);
        let ino = self.ino.get_or_alloc(path);
        self.cache_attr(ino, attr);
        Ok((ino, attr))
    }

    /// Full listing. With an overlay: remote entries minus shadowed names,
    /// plus local overlay children of this directory.
    /// How many entries a listing of `ino` would produce, if that is already
    /// known — for sizing a buffer, never for answering a question.
    ///
    /// Deliberately approximate: it reads the cached or warm listing's length
    /// and ignores the overlay merge and shadow filter entirely, so the number
    /// can be a little low or a little high. `None` means nothing is cached
    /// and the caller should not pay a round trip to find out.
    pub fn dir_len_hint(&self, ino: u64) -> Option<usize> {
        if let Some(hit) = self.dir_cache.get(&ino) {
            let (entries, when) = &*hit;
            if when.elapsed() < self.dir_ttl() {
                return Some(entries.len());
            }
        }
        let dir = self.path_of(ino).ok()?;
        self.warm.get(&dir).map(|w| w.len())
    }

    /// Is this directory empty — asked without materializing it.
    ///
    /// The mount layer asks exactly this before allowing a directory to be
    /// deleted, and asking it through `readdir` meant deep-cloning the cached
    /// listing (a `String` per child, plus an overlay merge) to look at
    /// `.is_empty()` and drop it. Deleting a directory of ten thousand files
    /// therefore allocated ten thousand names to learn a single bit.
    ///
    /// This reads a length instead, and leans on the one structural fact that
    /// makes that safe: `merge_overlay_children` only ever PUSHES. The merged
    /// listing is empty exactly when the base half is empty and no overlay
    /// child lives here, so a non-empty base half settles the question by
    /// itself. Anything the fast paths can't answer falls through to the real
    /// listing, so the answer is never weaker than `readdir`'s.
    pub fn dir_is_empty(&self, ino: u64) -> Result<bool, FsError> {
        let dir = self.path_of(ino)?;
        if self.is_overlay(&dir) {
            return Ok(self.overlay_ref().readdir_children(&dir).is_empty());
        }
        let overlay_empty = || {
            self.overlay.as_ref().is_none_or(|ov| {
                !ov.readdir_children(&dir)
                    .into_iter()
                    .any(|(name, _)| self.lives_in_overlay(&dir.join(&name)))
            })
        };
        if let Some(hit) = self.dir_cache.get(&ino) {
            let (entries, when) = &*hit;
            if when.elapsed() < self.dir_ttl() {
                let base_empty = entries.is_empty();
                drop(hit);
                return Ok(base_empty && overlay_empty());
            }
        }
        if let Some(w) = self.warm.get(&dir) {
            // Only an outright empty warm listing is cheap to trust. A
            // non-empty one still owes `readdir`'s per-entry shadow filter,
            // and every entry could be shadowed by an overlay whiteout — in
            // which case the directory really is empty. Re-deriving that here
            // would duplicate the filter for no gain, since the warm tier
            // serves cold mounts rather than the hot path.
            let empty = w.is_empty();
            drop(w);
            if empty {
                return Ok(overlay_empty());
            }
        }
        Ok(self.readdir(ino)?.is_empty())
    }

    pub fn readdir(&self, ino: u64) -> Result<Vec<(String, u64, Attr)>, FsError> {
        let dir = self.path_of(ino)?;
        if self.is_overlay(&dir) {
            let mut out = Vec::new();
            for (name, attr) in self.overlay_ref().readdir_children(&dir) {
                let child_ino = self.ino.get_or_alloc(dir.join(&name));
                out.push((name, child_ino, attr));
            }
            return Ok(out);
        }
        // Serve the remote half from cache while it is live. The overlay half
        // is merged fresh on every call — it is a local read, so caching it
        // would save microseconds and buy an invalidation problem.
        if let Some(hit) = self.dir_cache.get(&ino) {
            let (entries, when) = &*hit;
            if when.elapsed() < self.dir_ttl() {
                let mut out = entries.clone();
                drop(hit);
                self.merge_overlay_children(&dir, &mut out);
                return Ok(out);
            }
        }
        // The warm tier: a complete listing restored from the metadata
        // snapshot, token-proven at mount and event-busted from then on.
        // Filtered and merged exactly like a wire listing, because the
        // snapshot stores the SERVER's view raw — exclude patterns are client
        // configuration and may differ between the mount that wrote it and
        // this one, so routing belongs to serve time, not save time.
        if let Some(w) = self.warm.get(&dir) {
            let listing = w.clone();
            drop(w);
            let mut out = Vec::with_capacity(listing.len());
            for (name, attr) in listing {
                let child = dir.join(&name);
                if self.shadowed_by_overlay(&child) {
                    continue;
                }
                let child_ino = self.ino.get_or_alloc(child);
                self.cache_attr(child_ino, attr);
                out.push((name, child_ino, attr));
            }
            self.merge_overlay_children(&dir, &mut out);
            return Ok(out);
        }
        // A WIRE listing must not describe a state older than what this
        // client already acknowledged. The write batcher never trips this —
        // it only acks against a live complete listing, which the branches
        // above would have served — but a batched SETATTR acks against a
        // cached attr alone, so a cold listing fetched here could still show
        // the pre-chmod mode. Whatever is pending goes out first; it was due
        // within FLUSH_AGE anyway, and cold listings are already round-trip
        // priced.
        if self.batch.as_ref().is_some_and(|b| !b.is_empty()) {
            self.flush_batch();
        }
        // Captured BEFORE the first page goes out. Paging is several round
        // trips, and anything that invalidates a listing in that window makes
        // what comes back a description of the past.
        let epoch_at_start = self.dir_epoch.load(Ordering::Acquire);
        let mut remote = Vec::new();
        let mut cursor = 0u64;
        loop {
            let (entries, next_cursor) = expect_resp!(
                self.call(Request::Readdir { path: dir.clone(), cursor })?,
                Response::Dir { entries, next_cursor } => (entries, next_cursor)
            );
            for e in entries {
                let child = dir.join(&e.name);
                if self.shadowed_by_overlay(&child) {
                    continue;
                }
                let child_ino = self.ino.get_or_alloc(child);
                self.cache_attr(child_ino, e.attr);
                remote.push((e.name, child_ino, e.attr));
            }
            match next_cursor {
                Some(c) => cursor = c,
                None => break,
            }
        }
        // Cached listings are kept name-sorted (byte order): every lookup
        // against them binary-searches, and patch_parent_dir splices with
        // partition_point on the same assumption. Modern agents serve pages
        // sorted already, so this is a near-free merge pass — but the
        // invariant is enforced HERE, not trusted to the peer.
        remote.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        // Only cache a listing that still describes the present. A mutation
        // during the fetch means this result may already be wrong, and a wrong
        // COMPLETE listing does not merely go stale — it answers NotFound for
        // a file that exists. Skipping the insert costs one uncached readdir.
        if self.dir_epoch.load(Ordering::Acquire) == epoch_at_start {
            self.bound_dir_cache();
            self.dir_cache.insert(ino, (remote.clone(), Instant::now()));
        }
        let mut out = remote;
        self.merge_overlay_children(&dir, &mut out);
        Ok(out)
    }

    /// Append this directory's overlay children — always read live, never
    /// cached; see the field comment on `dir_cache`.
    fn merge_overlay_children(&self, dir: &RelPath, out: &mut DirListing) {
        if let Some(ov) = &self.overlay {
            for (name, attr) in ov.readdir_children(dir) {
                let child = dir.join(&name);
                if self.lives_in_overlay(&child) {
                    let child_ino = self.ino.get_or_alloc(child);
                    out.push((name, child_ino, attr));
                }
            }
        }
    }
}
