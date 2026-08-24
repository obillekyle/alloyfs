//! Namespace mutations: create, remove, rename, and the attribute setters.
//!
//! Grouped because they share one obligation — every one of them invalidates
//! something (a listing, a handle's path, a cached attr), and getting that
//! wrong is what leaves a client believing in a file that is gone.

use super::*;

impl RemoteFs {
    pub fn create(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        flags: OpenFlags,
    ) -> Result<(u64, u64, Attr), FsError> {
        let dir = self.path_of(parent)?;
        let path = dir.join(name);
        if self.is_overlay(&path) {
            let (fh, attr) = self.overlay_ref().create(&path, flags, mode)?;
            let ino = self.ino.get_or_alloc(path);
            return Ok((ino, fh, attr));
        }
        // The batched fast path: a NEW file acknowledged locally, its bytes
        // headed for one WriteMany entry at release. Engaged only when a
        // COMPLETE cached listing can answer the existence question the
        // server would have — deciding excl on a guess would invent files.
        if let Some(batch) = &self.batch {
            match self.knows_child_exists(parent, &dir, name) {
                Some(true) if flags.excl => return Err(ErrorCode::AlreadyExists.into()),
                Some(false) => {
                    let now = std::time::SystemTime::now();
                    let attr = Attr {
                        kind: alloyfs_proto::FileKind::File,
                        size: 0,
                        mtime: now,
                        ctime: now,
                        mode,
                        version: 0,
                    };
                    let ino = self.ino.get_or_alloc(path.clone());
                    // Claim before patch, for the same settle race remove()
                    // documents.
                    batch.pending_open(&path);
                    self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
                    self.cache_attr(ino, attr);
                    let fh = LAZY_FH_BIT | self.next_lazy_fh.fetch_add(1, Ordering::Relaxed);
                    self.track_open(
                        fh,
                        OpenState {
                            path,
                            flags,
                            server_fh: AtomicU64::new(NO_SERVER_FH),
                            cache_ok: AtomicBool::new(false),
                            warm_asked: AtomicBool::new(false),
                            warm_fill: std::sync::Mutex::new(None),
                            wrote: AtomicBool::new(true),
                            ra: ReadAhead::new(),
                            lock: std::sync::Mutex::new(Vec::new()),
                            poisoned: AtomicBool::new(false),
                            blob: std::sync::RwLock::new(None),
                            pending_new: Some(std::sync::Mutex::new(PendingNew {
                                data: Vec::new(),
                                mode,
                                cancelled: false,
                            })),
                            version: AtomicU64::new(0),
                            size: AtomicU64::new(0),
                            mtime_ns: AtomicU64::new(0),
                        },
                    );
                    return Ok((ino, fh, attr));
                }
                // Exists (without excl) or unknowable: the classic exchange
                // below answers both correctly.
                _ => {}
            }
        }
        let (fh, attr) = expect_resp!(
            self.call(Request::Create { path: path.clone(), flags, mode })?,
            Response::Opened { fh, attr } => (fh, attr)
        );
        let ino = self.ino.get_or_alloc(path.clone());
        // Self-origin events are stripped server-side, so the pump will never
        // tell us about our own create — the listing is corrected here, with
        // the reply's own attributes. Patched rather than busted: the next
        // create's existence probe stays local (see patch_parent_dir).
        self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        self.track_open(
            fh,
            OpenState {
                path,
                flags,
                server_fh: AtomicU64::new(fh),
                cache_ok: AtomicBool::new(false),
                warm_asked: AtomicBool::new(false),
                warm_fill: std::sync::Mutex::new(None),
                wrote: AtomicBool::new(true),
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
        Ok((ino, fh, attr))
    }

    pub fn mkdir(&self, parent: u64, name: &str, mode: u32) -> Result<(u64, Attr), FsError> {
        let dir = self.path_of(parent)?;
        let path = dir.join(name);
        if self.is_overlay(&path) {
            let attr = self.overlay_ref().mkdir(&path)?;
            let ino = self.ino.get_or_alloc(path);
            return Ok((ino, attr));
        }
        // A queued removal of this same path must land first, or the create
        // races it: `rm -rf d && mkdir d` inside the batcher's window gets
        // EEXIST from the server, and then the queued removal deletes the
        // directory that was just made. `rename` and `copy_range` already
        // barrier for exactly this; these three did not.
        if self.batch.as_ref().is_some_and(|b| b.involves(&path)) {
            self.barrier_for(&path)?;
        }
        let attr = expect_resp!(self.call(Request::Mkdir { path: path.clone(), mode })?, Response::Attr(attr) => attr);
        let ino = self.ino.get_or_alloc(path.clone());
        self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        // A just-created directory is COMPLETE and empty — seed that listing,
        // because completeness is what lets a create burst into the new
        // directory answer its own existence probes (and take the batched
        // path) without a wire readdir first.
        self.bound_dir_cache();
        self.dir_cache.insert(ino, (Vec::new(), Instant::now()));
        Ok((ino, attr))
    }

    pub fn unlink(&self, parent: u64, name: &str) -> Result<(), FsError> {
        self.remove(parent, name, false)
    }

    pub fn rmdir(&self, parent: u64, name: &str) -> Result<(), FsError> {
        self.remove(parent, name, true)
    }

    fn remove(&self, parent: u64, name: &str, dir: bool) -> Result<(), FsError> {
        let parent_path = self.path_of(parent)?;
        let path = parent_path.join(name);
        if self.is_overlay(&path) {
            return if dir {
                self.overlay_ref().rmdir(&path)
            } else {
                self.overlay_ref().unlink(&path)
            };
        }
        // The batched fast path: a removal a complete listing can vouch for
        // acknowledges locally and rides RemoveMany. Directories need their
        // own emptiness proven too, or the server's NotEmpty would arrive
        // after this ack already said gone.
        if let Some(batch) = &self.batch {
            if self.knows_child_exists(parent, &parent_path, name) == Some(true)
                && (!dir || self.knows_dir_empty(&path))
            {
                // A pending-new handle on this path dies with the name: its
                // release must enqueue nothing (the server never heard of
                // the file), and if nothing was ever queued for it, neither
                // is this removal.
                let mut never_reached_server = false;
                for fh in self.handles_on(&path) {
                    if let Some(e) = self.open_files.get(&fh) {
                        if let Some(p) = &e.pending_new {
                            p.lock().unwrap().cancelled = true;
                            never_reached_server = true;
                        }
                    }
                }
                if never_reached_server {
                    batch.forget(&path);
                }
                // Claim BEFORE patch: a concurrent flush settling this
                // path's older write computes "am I the last claim" — if the
                // local patch lands in the gap before the claim registers,
                // that settle answers yes and resurrects the entry this
                // removal just erased. Measured as sporadic AlreadyExists on
                // recreates under the age flusher. A file created and deleted
                // entirely inside the ack window enqueues nothing: the server
                // owes nothing and hears nothing.
                let enqueue = !(never_reached_server && batch.queued_count(&path) == 0);
                if enqueue {
                    batch.push(PendingOp::Remove {
                        path: path.clone(),
                        dir,
                    });
                }
                self.patch_parent_dir(&path, ListingPatch::Remove);
                self.bust_warm(&path);
                if let Some(ino) = self.ino.ino_of(&path) {
                    self.invalidate_attr(ino);
                }
                if let Some(cache) = &self.cache {
                    cache.remove(&path);
                }
                if enqueue && batch.wants_flush() {
                    self.flush_batch();
                }
                return Ok(());
            }
        }
        let req = if dir {
            Request::Rmdir { path: path.clone() }
        } else {
            Request::Unlink { path: path.clone() }
        };
        expect_resp!(self.call(req)?, Response::Ok => ());
        self.patch_parent_dir(&path, ListingPatch::Remove);
        // A removed DIRECTORY's own warm listing has to go too: the path can
        // be re-created, and get_or_alloc would hand the new directory its
        // predecessor's listing. (No-op for files — they never key the map.)
        self.bust_warm(&path);
        if let Some(ino) = self.ino.ino_of(&path) {
            self.invalidate_attr(ino);
        }
        if let Some(cache) = &self.cache {
            cache.remove(&path);
        }
        Ok(())
    }

    /// Copy a file within the export, server-side (wire v14).
    ///
    /// The whole point is that no bytes cross the link: a copy inside the
    /// mounted export otherwise reads every byte down and writes every
    /// byte back up, twice the file over the wire for data that never
    /// leaves the server's disk.
    ///
    /// Refused rather than emulated when it cannot be done server-side, and
    /// the refusal says WHICH reason. An old server is `VersionMismatch`,
    /// which is already EOPNOTSUPP to the kernel — precisely the "do it
    /// yourself" answer `copy_file_range` is specified to fall back from.
    /// An endpoint in the local overlay is `CrossDevice`, which is what it
    /// literally is: one side lives on this machine and the server has no
    /// copy of the data to duplicate. Emulating either here with a read
    /// and a write would work and would hide the fact that the fast path
    /// never happened.
    pub fn copy_range(
        &self,
        ino_in: u64,
        offset_in: u64,
        ino_out: u64,
        offset_out: u64,
        len: u64,
    ) -> Result<u32, FsError> {
        let from = self.path_of(ino_in)?;
        let to = self.path_of(ino_out)?;
        if self.is_overlay(&from) || self.is_overlay(&to) {
            return Err(ErrorCode::CrossDevice.into());
        }
        self.require_proto(14, "server-side copy")?;
        // Same ordering duty as rename: the source's queued (or still
        // open-pending) truth has to be on the server before it can copy
        // it, and the destination must not be shadowed by queued work
        // either. Without this a copy could read a file the server has
        // never seen and answer zero bytes.
        if self.batch.is_some() {
            self.materialize_open_pending(&from)?;
            self.materialize_open_pending(&to)?;
            self.barrier_for(&from)?;
            self.barrier_for(&to)?;
        }
        let (n, new_version) = expect_resp!(
            self.call(Request::Copy {
                from,
                from_offset: offset_in,
                to: to.clone(),
                to_offset: offset_out,
                len,
            })?,
            Response::Written { n, new_version, .. } => (n, new_version)
        );
        // The destination changed size and mtime and we do not know the
        // new values — the reply carries a count, not attributes. Drop
        // what is cached rather than guess: a stale size here would be
        // served to the next stat as truth.
        if let Some(ino) = self.ino.ino_of(&to) {
            self.invalidate_attr(ino);
            if let Some(state) = self.open_files.get(&ino_out) {
                state.version.store(new_version, Ordering::Relaxed);
            }
        }
        self.invalidate_parent_dir(&to);
        self.invalidate_open_reads(&to);
        Ok(n)
    }

    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        replace: bool,
    ) -> Result<(), FsError> {
        let from = self.path_of(parent)?.join(name);
        let to = self.path_of(newparent)?.join(newname);
        // A rename can touch names whose truth is still queued; ordering
        // requires the queue lands first, and any damage on either endpoint
        // belongs to this operation.
        //
        // Queued is not enough, though: an OPEN pending-new file exists only
        // on its handle — it seals into the queue at first close, so the
        // barrier below cannot land it. bun's atomic save is exactly that
        // shape: write .lock-*.tmp, rename it over the target WHILE THE
        // HANDLE IS STILL OPEN (the POSIX rename-while-open this mount
        // advertises support for), close after. The server had never heard
        // of the tmp, answered NotFound, and bun reported ENOENT for a file
        // it had just written — reproduced 100% with `bun init` on the
        // mount, stranded tmp and all. Materialize open-pending endpoints
        // first; the barrier then handles the queued world as before.
        if self.batch.is_some() {
            self.materialize_open_pending(&from)?;
            self.materialize_open_pending(&to)?;
            self.barrier_for(&from)?;
            self.barrier_for(&to)?;
        }
        match (self.is_overlay(&from), self.is_overlay(&to)) {
            (true, true) => {
                self.overlay_ref().rename(&from, &to, replace)?;
                self.ino.rename(&from, &to);
                Ok(())
            }
            (false, false) => {
                expect_resp!(
                    self.call(Request::Rename { from: from.clone(), to: to.clone(), replace })?,
                    Response::Ok => ()
                );
                // Both listings changed: one lost an entry, one gained it. The
                // renamed directory's OWN cached listing survives — its ino is
                // stable and the names inside it did not move. The warm tier
                // is path-keyed, so it gets no such stability: everything
                // under both old and new paths is forgotten.
                self.invalidate_parent_dir(&from);
                self.invalidate_parent_dir(&to);
                self.warm_forget_subtree(&from);
                self.warm_forget_subtree(&to);
                self.ino.rename(&from, &to);
                if let Some(cache) = &self.cache {
                    cache.rename(&from, &to);
                }
                Ok(())
            }
            // Across the boundary: EXDEV — tools fall back to copy+delete,
            // and each individual op then routes to the right side.
            _ => Err(ErrorCode::CrossDevice.into()),
        }
    }

    pub fn setattr(
        &self,
        ino: u64,
        size: Option<u64>,
        mtime: Option<std::time::SystemTime>,
        mode: Option<u32>,
    ) -> Result<Attr, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            return self.overlay_ref().setattr(&path, size, mtime, mode);
        }
        // v10 batched metadata-only setattr — the archive-extraction shape,
        // a timestamp restore per file, each of which was a full round trip.
        // Size stays write-through (truncation is data, not metadata), and
        // the ack needs a cached attr to merge into: with nothing cached
        // there is nothing honest to answer with, so that case (and an
        // unsealed pending-new file, whose path the server cannot know yet)
        // takes the classic path below.
        if size.is_none() && (mtime.is_some() || mode.is_some()) {
            if let Some(batch) = &self.batch {
                if self.conn().proto >= 10 && !batch.has_open_pending(&path) {
                    if let Some(hit) = self.attr_cache.get(&ino) {
                        let (mut attr, _) = *hit;
                        drop(hit);
                        if let Some(mt) = mtime {
                            attr.mtime = mt;
                        }
                        if let Some(md) = mode {
                            // The win bits ride the high mode bits (v11) and a
                            // kernel chmod never carries them: keep ours, take
                            // the caller's permission bits.
                            attr.mode = (attr.mode & alloyfs_proto::MODE_WIN_MASK)
                                | (md & !alloyfs_proto::MODE_WIN_MASK);
                        }
                        batch.push_setattr(&path, mtime, mode);
                        if batch.wants_flush() {
                            self.flush_batch();
                        }
                        self.patch_parent_dir(&path, ListingPatch::Upsert(ino, attr));
                        self.cache_attr(ino, attr);
                        return Ok(attr);
                    }
                }
            }
        }
        let attr = expect_resp!(
            self.call(Request::Setattr { path: path.clone(), size, mtime, mode })?,
            Response::Attr(attr) => attr
        );
        self.finish_setattr(ino, &path, size, attr)
    }

    /// `setattr` with the readonly bit resolved SERVER-side (v9+).
    ///
    /// Callers that want "make it readonly / writable" without dictating a
    /// full mode — WinFsp's attribute mapping — used to fetch the current
    /// mode and send a computed one: a round trip, plus a race against any
    /// chmod landing between the two. Against a pre-v9 agent this does
    /// exactly that old dance as the fallback, so the caller no longer has to.
    pub fn setattr_readonly(
        &self,
        ino: u64,
        size: Option<u64>,
        mtime: Option<std::time::SystemTime>,
        readonly: bool,
    ) -> Result<Attr, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            let cur = self.overlay_ref().getattr(&path)?;
            let mode = if readonly {
                cur.mode & !0o222
            } else {
                cur.mode | 0o200
            };
            return self.overlay_ref().setattr(&path, size, mtime, Some(mode));
        }
        if self.conn().proto >= 9 {
            let attr = expect_resp!(
                self.call(Request::Setattr2 {
                    path: path.clone(),
                    size,
                    mtime,
                    mode: None,
                    readonly: Some(readonly),
                })?,
                Response::Attr(attr) => attr
            );
            return self.finish_setattr(ino, &path, size, attr);
        }
        // Pre-v9: the read-modify-write this method exists to retire, kept as
        // the compatibility path — claims exactly what the caller always got.
        let cur = self.getattr(ino)?;
        let mode = if readonly {
            cur.mode & !0o222
        } else {
            cur.mode | 0o200
        };
        self.setattr(ino, size, mtime, Some(mode))
    }

    /// The shared tail of every setattr flavour: cache upkeep in one place so
    /// the two wire paths cannot drift.
    fn finish_setattr(
        &self,
        ino: u64,
        path: &RelPath,
        size: Option<u64>,
        attr: Attr,
    ) -> Result<Attr, FsError> {
        // The parent's cached listing carries this entry's attributes; an
        // explicit metadata change is patched into it from the reply.
        self.patch_parent_dir(path, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        if size.is_some() {
            // The patch above already corrected the listings; passing the
            // attrs again keeps this from busting the warm tier it fixed.
            self.mark_path_written(path, Some((ino, attr)));
        }
        Ok(attr)
    }

    pub fn link(&self, target_ino: u64, newparent: u64, newname: &str) -> Result<(u64, Attr), FsError> {
        let target = self.path_of(target_ino)?;
        let dir = self.path_of(newparent)?;
        let link = dir.join(newname);
        // Same race as mkdir and symlink: a queued removal of this name has to
        // land before the link is created. The TARGET matters too — linking to
        // a file the queue has not written yet would ask the server for a
        // source that is not there.
        if let Some(b) = self.batch.as_ref() {
            if b.involves(&link) {
                self.barrier_for(&link)?;
            }
            if b.involves(&target) {
                self.barrier_for(&target)?;
            }
        }
        match (self.is_overlay(&target), self.is_overlay(&link)) {
            (true, true) => {
                let attr = self.overlay_ref().link(&target, &link)?;
                let ino = self.ino.get_or_alloc(link);
                Ok((ino, attr))
            }
            (false, false) => {
                let attr = expect_resp!(self.call(Request::Link { target, link: link.clone() })?, Response::Attr(attr) => attr);
                let ino = self.ino.get_or_alloc(link.clone());
                self.patch_parent_dir(&link, ListingPatch::Upsert(ino, attr));
                self.cache_attr(ino, attr);
                Ok((ino, attr))
            }
            _ => Err(ErrorCode::CrossDevice.into()),
        }
    }

    /// Rewrite a symlink target that points back into this mount so the
    /// server can store it, leaving anything else untouched.
    ///
    /// Tooling writes absolute targets by habit, and the absolute form is
    /// always local: PowerShell's `New-Item -Target "real.txt"` resolves to
    /// `Y:\dir\real.txt` before the syscall is made, and `ln -s "$(pwd)/x"`
    /// produces `/mnt/alloy/dir/x`. Neither means anything to the server,
    /// which would refuse both as paths outside the export — a correct check
    /// with a useless outcome, since the user meant a path inside it.
    ///
    /// So a target under this mount's own root becomes one relative to the
    /// link's directory, which is what was meant and what stays correct if
    /// the export is later mounted somewhere else. A target absolute
    /// SOMEWHERE ELSE (`C:\Windows`, `/etc`, a UNC share) is passed through
    /// and the server refuses it — those genuinely do leave the export.
    ///
    /// This lives here rather than in a backend because every backend has the
    /// problem; it was found on Windows only because that is where symlinks
    /// were tested first.
    fn localize_target(&self, target: &str, link_dir: &RelPath) -> String {
        match &self.mount_root {
            Some(root) => localize_symlink_target(root, target, link_dir),
            None => target.to_string(),
        }
    }
    /// Create a symlink at `parent/name` pointing at `target`.
    ///
    /// Unlike `link`, only the LINK's location decides where this goes: the
    /// target is opaque text that may not resolve to anything yet, so there is
    /// no second path to route on. A link created in the overlay stays local;
    /// one created on the server is sent there and validated against the
    /// export boundary by the agent.
    pub fn symlink(&self, parent: u64, name: &str, target: &str) -> Result<(u64, Attr), FsError> {
        let dir = self.path_of(parent)?;
        let link = dir.join(name);
        // Before anything else: a target pointing back into this mount is
        // written as an absolute local path by most tooling, and means
        // nothing to the server.
        let target = self.localize_target(target, &dir);
        if self.is_overlay(&link) {
            let attr = self.overlay_ref().symlink(&target, &link)?;
            let ino = self.ino.get_or_alloc(link);
            return Ok((ino, attr));
        }
        // Same race as mkdir: a queued removal of this name must land before
        // the link is created, or the create gets EEXIST and the removal then
        // deletes what it made.
        if self.batch.as_ref().is_some_and(|b| b.involves(&link)) {
            self.barrier_for(&link)?;
        }
        self.require_proto(4, "symlink")?;
        let attr = expect_resp!(
            self.call(Request::Symlink {
                target,
                link: link.clone(),
            })?,
            Response::Attr(attr) => attr
        );
        let ino = self.ino.get_or_alloc(link.clone());
        self.patch_parent_dir(&link, ListingPatch::Upsert(ino, attr));
        self.cache_attr(ino, attr);
        Ok((ino, attr))
    }

    /// A symlink's target, verbatim as stored.
    pub fn readlink(&self, ino: u64) -> Result<String, FsError> {
        let path = self.path_of(ino)?;
        if self.is_overlay(&path) {
            return self.overlay_ref().readlink(&path);
        }
        self.require_proto(4, "readlink")?;
        Ok(expect_resp!(
            self.call(Request::ReadLink { path })?,
            Response::Target(t) => t
        ))
    }
}
