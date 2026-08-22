//! Request serving: one kernel request in, one response frame out.
//!
//! Deliberately free of any I/O and of any Linux-only API, so the whole
//! opcode surface is exercised by unit tests on every platform and the
//! Linux half of this crate is reduced to "read bytes, call `handle`, write
//! bytes".
//!
//! Two mappings do the real work:
//!
//!   * **nodeid ↔ path.** The kernel addresses everything by nodeid; the wire
//!     protocol speaks paths. `RemoteFs::ino` is exactly that table, and its
//!     `ROOT_INO` is 1 — the same number the ABI reserves for the root — so
//!     the kernel's inode numbers *are* the client's inode numbers.
//!   * **nodeid → open handle.** The ABI has no OPEN/RELEASE: a READ names a
//!     file, not a handle. So reads and writes borrow a handle from a small
//!     LRU cache, opening one on demand and releasing the eviction victim.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloyfs_client::{FsError, RemoteFs, ROOT_INO};
use alloyfs_proto::{Attr, ErrorCode, EventKind, FileKind, FsEvent, OpenFlags, RelPath};

use crate::abi::{self, InHeader, KernelAttr, Notification};

/// How many server handles the daemon keeps open on behalf of the kernel.
/// Small on purpose: a handle is server-side state (and possibly a lease), so
/// the cache is a working set, not a registry.
const HANDLE_CACHE_CAP: usize = 64;

/// A directory listing is fetched whole and paged out across READDIR calls;
/// this bounds how long one listing may serve continuation calls.
const DIR_CACHE_TTL: Duration = Duration::from_secs(5);

/// The kernel emits "." and ".." itself before calling us, so its cursor
/// starts at 2 and child *i* lives at position *i* + 2.
const DOTS: u64 = 2;

/// Shared with the FUSE backend — see `alloyfs_client::posix_errno`. Both are
/// front-ends onto Linux, so one table serves them; the copies here and in
/// FUSE had already drifted apart on `NoSuchExport`.
use alloyfs_client::posix_errno as errno_of;

fn nanos_since_epoch(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// A name straight off the wire from the kernel. Everything about it is
/// checked here because the rest of this crate treats it as a path component.
fn name_of(bytes: &[u8]) -> Result<&str, i32> {
    if bytes.is_empty() {
        return Err(abi::EINVAL);
    }
    if bytes.len() > abi::MAX_NAME {
        return Err(abi::ENAMETOOLONG);
    }
    let name = std::str::from_utf8(bytes).map_err(|_| abi::EINVAL)?;
    // A name is one component: no separators, no dots, no interior NUL.
    if name.contains('/') || name.contains('\0') || name == "." || name == ".." {
        return Err(abi::EINVAL);
    }
    Ok(name)
}

#[derive(Clone, Copy)]
struct Handle {
    fh: u64,
    write: bool,
    used: u64,
    /// An advisory lock is held on this handle. The cache must not evict it:
    /// releasing the handle releases the lock, and the process that thinks it
    /// holds it gets no say in the matter.
    locked: bool,
}

#[derive(Default)]
struct HandleCache {
    map: HashMap<u64, Handle>,
    tick: u64,
}

/// One directory's children as the kernel wants them: name, nodeid, isdir.
///
/// Shared rather than owned because a directory is read one page at a time
/// and every page asks for the whole listing again. Owned, that meant a deep
/// clone — a `String` per child — per page: reading a ten-thousand-entry
/// directory in 4 KiB pages cloned ten thousand names about a hundred times
/// over. Nothing mutates a cached listing (a change replaces it wholesale),
/// so the pages can share one.
type DirEntries = Arc<Vec<(String, u64, bool)>>;

struct DirListing {
    at: Instant,
    entries: DirEntries,
}

pub struct Server {
    fs: Arc<RemoteFs>,
    handles: Mutex<HandleCache>,
    dirs: Mutex<HashMap<u64, DirListing>>,
    /// Is this nodeid a directory? Recorded from every attr we hand out, so a
    /// notification about a *deleted* name can still set FS_ISDIR correctly —
    /// by then there is nothing left to stat.
    kinds: Mutex<HashMap<u64, bool>>,
    /// Which lock owners hold something on each nodeid. This is what decides
    /// when a handle's `locked` pin can come off: byte ranges mean one unlock
    /// no longer implies "nothing left", so the pin drops only once every
    /// owner has sent the whole-file release the VFS issues on close.
    lock_owners: Mutex<HashMap<u64, HashSet<u64>>>,
}

impl Server {
    pub fn new(fs: Arc<RemoteFs>) -> Self {
        Self {
            fs,
            handles: Mutex::new(HandleCache::default()),
            dirs: Mutex::new(HashMap::new()),
            kinds: Mutex::new(HashMap::new()),
            lock_owners: Mutex::new(HashMap::new()),
        }
    }

    /// Serve one request buffer as read from the device. `None` means the
    /// bytes were not a well-formed request (nothing to answer, and no
    /// `unique` to answer it with).
    pub fn handle(&self, buf: &[u8]) -> Option<Vec<u8>> {
        let h = InHeader::parse(buf)?;
        let payload = &buf[abi::IN_HEADER_LEN..h.len as usize];
        // READ is framed in place. Every other op hands back a payload that
        // `abi::response` copies into a frame, which is the right trade when
        // a payload is an attr or a dirent page — but a READ payload is up to
        // 128 KiB, so that convention meant allocating and filling one buffer
        // and then allocating and filling a second, larger one. Here the
        // frame is allocated once and the file's bytes land directly behind
        // its header.
        if h.opcode == abi::OP_READ {
            return Some(match self.op_read_framed(&h) {
                Ok(frame) => frame,
                Err(errno) => {
                    tracing::debug!(op = h.opcode, nodeid = h.nodeid, errno, "request failed");
                    abi::response(h.unique, -errno, &[])
                }
            });
        }
        Some(match self.dispatch(&h, payload) {
            Ok(out) => abi::response(h.unique, 0, &out),
            Err(errno) => {
                tracing::debug!(op = h.opcode, nodeid = h.nodeid, errno, "request failed");
                abi::response(h.unique, -errno, &[])
            }
        })
    }

    fn dispatch(&self, header: &InHeader, payload: &[u8]) -> Result<Vec<u8>, i32> {
        match header.opcode {
            abi::OP_LOOKUP => self.op_lookup(header.nodeid, payload),
            abi::OP_GETATTR => self.op_getattr(header.nodeid),
            abi::OP_READDIR => self.op_readdir(header.nodeid, header.offset, header.size as usize),
            // OP_READ is deliberately absent: `handle` routes it to
            // `op_read_framed` before reaching here, so that it can build its
            // reply frame in one allocation instead of returning a payload to
            // be copied into a second. Adding an arm for it here would create
            // a second, slower implementation that nothing calls.
            abi::OP_CREATE => self.op_create(header.nodeid, payload, false),
            abi::OP_MKDIR => self.op_create(header.nodeid, payload, true),
            abi::OP_UNLINK => self.op_remove(header.nodeid, payload, false),
            abi::OP_RMDIR => self.op_remove(header.nodeid, payload, true),
            abi::OP_RENAME => self.op_rename(header.nodeid, payload),
            abi::OP_WRITE => self.op_write(header.nodeid, header.offset, payload),
            abi::OP_SETATTR => self.op_setattr(header.nodeid, payload),
            abi::OP_LOCK => self.op_lock(header.nodeid, payload),
            abi::OP_UNLOCK => self.op_unlock(header.nodeid, payload),
            abi::OP_GETLK => self.op_getlk(header.nodeid, payload),
            abi::OP_SYMLINK => self.op_symlink(header.nodeid, payload),
            abi::OP_READLINK => self.op_readlink(header.nodeid),
            abi::OP_LINK => self.op_link(header.nodeid, payload),
            abi::OP_STATFS => self.op_statfs(),
            _ => Err(abi::ENOSYS),
        }
    }

    // ------------------------------------------------------------- attrs

    /// Server attr → kernel attr, remembering the kind for later.
    ///
    /// Symlinks report S_IFLNK now that the module implements `get_link`.
    /// They used to be flattened to regular files, because an inode the VFS
    /// could not resolve was worse than a slightly dishonest mode.
    fn kernel_attr(&self, nodeid: u64, attr: &Attr) -> KernelAttr {
        let isdir = matches!(attr.kind, FileKind::Dir);
        self.record_kind(nodeid, isdir);
        let perm = match attr.mode & 0o7777 {
            // Exports served by a backend with no Unix modes report 0; an
            // inode with no permission bits is unusable to anything but root.
            0 => {
                if isdir {
                    0o755
                } else {
                    0o644
                }
            }
            bits => bits,
        };
        let kind_bits = match attr.kind {
            FileKind::Dir => abi::S_IFDIR,
            FileKind::Symlink => abi::S_IFLNK,
            FileKind::File => abi::S_IFREG,
        };
        KernelAttr {
            nodeid,
            size: attr.size,
            mtime_ns: nanos_since_epoch(attr.mtime),
            mode: kind_bits | perm,
            nlink: if isdir { 2 } else { 1 },
        }
    }

    fn op_lookup(&self, parent: u64, payload: &[u8]) -> Result<Vec<u8>, i32> {
        let name = name_of(payload)?;
        let (ino, attr) = self.fs.lookup(parent, name).map_err(|e| errno_of(&e))?;
        Ok(self.kernel_attr(ino, &attr).to_bytes())
    }

    fn op_getattr(&self, nodeid: u64) -> Result<Vec<u8>, i32> {
        let attr = self.fs.getattr(nodeid).map_err(|e| errno_of(&e))?;
        Ok(self.kernel_attr(nodeid, &attr).to_bytes())
    }

    // ----------------------------------------------------------- readdir

    /// The listing to page from. A cursor at the start always refetches; a
    /// continuation reuses the listing it started on, so paging cannot skip or
    /// duplicate entries because the directory changed underneath it.
    fn dir_entries(&self, nodeid: u64, offset: u64) -> Result<DirEntries, i32> {
        if offset > DOTS {
            if let Some(hit) = self.dirs.lock().unwrap().get(&nodeid) {
                if hit.at.elapsed() < DIR_CACHE_TTL {
                    return Ok(Arc::clone(&hit.entries));
                }
            }
        }
        let listing = self.fs.readdir(nodeid).map_err(|e| errno_of(&e))?;
        let mut entries = Vec::with_capacity(listing.len());
        // One acquisition for the whole listing rather than one per child.
        // The map is only ever touched from these few sites and none of them
        // is reachable from inside this loop, so holding it across the walk
        // cannot deadlock — and the per-entry version was locking and
        // unlocking ten thousand times to record ten thousand booleans.
        {
            let mut kinds = self.kinds.lock().unwrap();
            for (name, ino, attr) in listing {
                let isdir = matches!(attr.kind, FileKind::Dir);
                kinds.insert(ino, isdir);
                entries.push((name, ino, isdir));
            }
        }
        let entries = Arc::new(entries);
        self.dirs.lock().unwrap().insert(
            nodeid,
            DirListing {
                at: Instant::now(),
                entries: Arc::clone(&entries),
            },
        );
        Ok(entries)
    }

    fn op_readdir(&self, nodeid: u64, offset: u64, size: usize) -> Result<Vec<u8>, i32> {
        let limit = size.min(abi::MAX_PAYLOAD);
        let entries = self.dir_entries(nodeid, offset)?;
        let first = offset.saturating_sub(DOTS) as usize;
        let mut out = Vec::new();
        for (i, (name, ino, isdir)) in entries.iter().enumerate().skip(first) {
            let ty = if *isdir { abi::DT_DIR } else { abi::DT_REG };
            // `off` resumes AFTER this entry: child i sits at i + DOTS, so the
            // next cursor is i + DOTS + 1.
            if !abi::push_dirent(&mut out, limit, *ino, i as u64 + DOTS + 1, ty, name) {
                if out.is_empty() {
                    // Nothing fits at all — returning an empty reply would
                    // mean "end of directory" and silently truncate it.
                    return Err(abi::EINVAL);
                }
                break;
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------- open handles

    /// A server handle for `nodeid`, opening one if the cache has none good
    /// enough. Never holds the cache lock across an RPC.
    fn handle_for(&self, nodeid: u64, want_write: bool) -> Result<u64, i32> {
        {
            let mut cache = self.handles.lock().unwrap();
            cache.tick += 1;
            let tick = cache.tick;
            if let Some(h) = cache.map.get_mut(&nodeid) {
                if h.write || !want_write {
                    h.used = tick;
                    return Ok(h.fh);
                }
            }
        }
        let flags = OpenFlags {
            read: true,
            write: want_write,
            ..OpenFlags::default()
        };
        let (fh, _attr) = self.fs.open(nodeid, flags).map_err(|e| errno_of(&e))?;
        for stale in self.install_handle(nodeid, fh, want_write) {
            self.fs.release(stale);
        }
        Ok(fh)
    }

    /// Insert `fh` and return the handles that must be released: the one it
    /// replaced (a racing opener, or a read-only handle we outgrew) plus the
    /// LRU victim when the cache is full.
    fn install_handle(&self, nodeid: u64, fh: u64, write: bool) -> Vec<u64> {
        let mut cache = self.handles.lock().unwrap();
        cache.tick += 1;
        let used = cache.tick;
        let mut stale = Vec::new();
        let locked = cache.map.get(&nodeid).is_some_and(|h| h.locked);
        if let Some(old) = cache.map.insert(
            nodeid,
            Handle {
                fh,
                write,
                used,
                // Carry the flag across a replacement: the new handle is the
                // one a later unlock will be sent to.
                locked,
            },
        ) {
            stale.push(old.fh);
        }
        while cache.map.len() > HANDLE_CACHE_CAP {
            // Locked handles are not eviction candidates. If every entry is
            // locked the cache simply grows past its cap — bounded by the
            // number of files something has actually locked, and far better
            // than evicting a lock out from under a caller.
            let Some(victim) = cache
                .map
                .iter()
                .filter(|(_, h)| !h.locked)
                .min_by_key(|(_, h)| h.used)
                .map(|(id, _)| *id)
            else {
                break;
            };
            if let Some(h) = cache.map.remove(&victim) {
                stale.push(h.fh);
            }
        }
        stale
    }

    /// Forget (and release) any handle for `nodeid` — used when the file it
    /// names is gone. Releasing the handle released its locks with it, so the
    /// owner bookkeeping goes too.
    fn drop_handle(&self, nodeid: u64) {
        self.lock_owners.lock().unwrap().remove(&nodeid);
        let h = self.handles.lock().unwrap().map.remove(&nodeid);
        if let Some(h) = h {
            self.fs.release(h.fh);
        }
    }

    /// Record whether the handle for `nodeid` currently owns an advisory lock.
    fn mark_locked(&self, nodeid: u64, locked: bool) {
        if let Some(h) = self.handles.lock().unwrap().map.get_mut(&nodeid) {
            h.locked = locked;
        }
    }

    /// The file behind `nodeid` changed underneath our cached handle: drop
    /// what that handle has prefetched, keeping the handle itself.
    ///
    /// This used to release the handle outright, which fixed the staleness
    /// but would now silently drop any advisory lock held on it. The stale
    /// bytes live in the handle's readahead window, so clear exactly that.
    fn refresh_handle(&self, nodeid: u64) {
        let fh = self.handles.lock().unwrap().map.get(&nodeid).map(|h| h.fh);
        if let Some(fh) = fh {
            self.fs.invalidate_read_cache(fh);
        }
    }

    /// Release every cached handle. Called once the mount is finished with.
    pub fn close_handles(&self) {
        self.lock_owners.lock().unwrap().clear();
        let handles: Vec<Handle> = {
            let mut cache = self.handles.lock().unwrap();
            cache.map.drain().map(|(_, h)| h).collect()
        };
        for h in handles {
            self.fs.release(h.fh);
        }
    }

    // ------------------------------------------------------------- data

    /// Serve a READ as a complete reply frame — see the note in `handle`.
    ///
    /// The buffer is sized for the whole request up front and the file's
    /// bytes are read straight in behind the header; a short read just
    /// truncates it. Allocating zeroed and truncating afterwards is what
    /// keeps this out of `unsafe`, and the kernel is only ever shown the
    /// bytes that were actually read.
    fn op_read_framed(&self, h: &InHeader) -> Result<Vec<u8>, i32> {
        let size = h.size.min(abi::MAX_PAYLOAD as u32) as usize;
        if size == 0 {
            return Ok(abi::response(h.unique, 0, &[]));
        }
        let fh = self.handle_for(h.nodeid, false)?;
        let mut frame = vec![0u8; abi::OUT_HEADER_LEN + size];
        let n = self
            .fs
            .read_into(fh, h.offset, &mut frame[abi::OUT_HEADER_LEN..])
            .map_err(|e| errno_of(&e))?;
        frame.truncate(abi::OUT_HEADER_LEN + n);
        abi::fill_response_header(&mut frame, h.unique, 0);
        Ok(frame)
    }

    fn op_write(&self, nodeid: u64, offset: u64, data: &[u8]) -> Result<Vec<u8>, i32> {
        if data.is_empty() {
            return Ok(abi::write_out(0));
        }
        let fh = self.handle_for(nodeid, true)?;
        // The kernel updates i_size from the reply, and our own attr cache must
        // not keep serving the pre-write size to the next GETATTR. At protocol
        // 5 the write reply carries the new attributes and `write_at` re-seeds
        // the cache with them; against an older agent it invalidates instead.
        // Either way the stale entry is gone — but only the first spares the
        // extra round-trip that adding the attributes to the reply was for.
        let (n, _attr) = self.fs.write_at(fh, offset, data).map_err(|e| errno_of(&e))?;
        Ok(abi::write_out(n))
    }

    // ------------------------------------------------------------- links

    fn op_symlink(&self, parent: u64, payload: &[u8]) -> Result<Vec<u8>, i32> {
        let (name_bytes, target_bytes) = abi::parse_symlink_in(payload).ok_or(abi::EINVAL)?;
        let name = name_of(name_bytes)?;
        // The target is NOT run through `name_of`: it is a path, so slashes
        // are expected, and it is not ours to validate — the agent decides
        // whether where it lands is inside the export.
        let target = std::str::from_utf8(target_bytes).map_err(|_| abi::EINVAL)?;
        if target.is_empty() || target.contains('\0') {
            return Err(abi::EINVAL);
        }
        self.forget_dir(parent);
        let (ino, attr) = self.fs.symlink(parent, name, target).map_err(|e| errno_of(&e))?;
        Ok(self.kernel_attr(ino, &attr).to_bytes())
    }

    fn op_readlink(&self, nodeid: u64) -> Result<Vec<u8>, i32> {
        let target = self.fs.readlink(nodeid).map_err(|e| errno_of(&e))?;
        // Raw bytes, no terminator: the kernel NUL-terminates its own buffer
        // from the reply length.
        Ok(target.into_bytes())
    }

    fn op_link(&self, parent: u64, payload: &[u8]) -> Result<Vec<u8>, i32> {
        let (target, name_bytes) = abi::parse_link_in(payload).ok_or(abi::EINVAL)?;
        let name = name_of(name_bytes)?;
        self.forget_dir(parent);
        let (ino, attr) = self.fs.link(target, parent, name).map_err(|e| errno_of(&e))?;
        Ok(self.kernel_attr(ino, &attr).to_bytes())
    }

    fn op_statfs(&self) -> Result<Vec<u8>, i32> {
        let (block_size, blocks, blocks_free) = self.fs.statfs().map_err(|e| errno_of(&e))?;
        Ok(abi::statfs_out(blocks, blocks_free, block_size))
    }

    // ------------------------------------------------------------- locks

    /// Take an advisory byte-range lock on the server, so it means something
    /// to the other machines mounting this export.
    ///
    /// The handle is opened for WRITE where the export allows it, even for a
    /// shared lock. That is not about access: `handle_for` upgrades a
    /// read-only handle by opening a new one and releasing the old, and the
    /// lock lives on the old one — so a plain `read; flock; write` sequence
    /// would drop its own lock halfway through. Taking the write handle up
    /// front means no upgrade can happen while the lock is held. A read-only
    /// export cannot be written anyway, so falling back there loses nothing.
    ///
    /// Against a pre-v7 agent the range op does not exist, and the TAKE
    /// coarsens to the whole-file lock — safe, because it claims strictly
    /// more than was asked. The release below deliberately has no such
    /// fallback: see `op_unlock`.
    ///
    /// MAY BLOCK for as long as the caller asked it to: `wait` is F_SETLKW,
    /// and the whole point is to sleep until the holder lets go. Each request
    /// is dispatched on its own blocking task, so a sleeper parks one pool
    /// thread rather than the mount.
    fn op_lock(&self, nodeid: u64, payload: &[u8]) -> Result<Vec<u8>, i32> {
        let req = abi::parse_lock_in(payload).ok_or(abi::EINVAL)?;
        let fh = match self.handle_for(nodeid, true) {
            Ok(fh) => fh,
            Err(_) => self.handle_for(nodeid, false)?,
        };
        match self
            .fs
            .lock_range(fh, req.owner, req.kind, req.start, req.len, req.wait)
        {
            Ok(()) => {}
            Err(e) if is_version_mismatch(&e) => {
                self.fs.lock(fh, req.kind, req.wait).map_err(|e| errno_of(&e))?;
            }
            Err(e) => return Err(errno_of(&e)),
        }
        self.note_owner(nodeid, req.owner);
        Ok(Vec::new())
    }

    /// Release exactly `[start, start+len)` for one owner on `nodeid`.
    ///
    /// A missing handle is success, not an error. The VFS unlocks on close
    /// whether or not this owner ever locked, so "no handle here" is the
    /// ordinary path for every file that was opened and never locked.
    ///
    /// Against a pre-v7 agent this REFUSES with ENOLCK rather than falling
    /// back to the whole-file unlock — the mirror image of the take-side
    /// coarsening, and deliberately asymmetric: a coarsened take claims more
    /// than was asked, which is safe; a coarsened release drops every lock
    /// the handle holds, which is the bug the range ops exist to fix. The
    /// server-side lock then stays until the handle itself is released.
    fn op_unlock(&self, nodeid: u64, payload: &[u8]) -> Result<Vec<u8>, i32> {
        let req = abi::parse_unlock_in(payload).ok_or(abi::EINVAL)?;
        let fh = self.handles.lock().unwrap().map.get(&nodeid).map(|h| h.fh);
        let Some(fh) = fh else {
            return Ok(Vec::new());
        };
        match self.fs.unlock_range(fh, req.owner, req.start, req.len) {
            Ok(()) => {}
            Err(e) if is_version_mismatch(&e) => return Err(abi::ENOLCK),
            Err(e) => return Err(errno_of(&e)),
        }
        // A whole-file release is how the VFS spells "this owner is done"
        // (locks_remove_posix on close); a partial one leaves fragments, so
        // the owner — and the handle pin — must survive it.
        if req.start == 0 && req.len == 0 {
            self.forget_owner(nodeid, req.owner);
        }
        Ok(Vec::new())
    }

    /// F_GETLK: what would block this probe, if anything.
    ///
    /// Never answered locally, and never coarsened: a pre-v7 agent gets
    /// ENOLCK, matching what the module itself returned before the opcode
    /// existed. A local answer would say "free" while another machine held
    /// the range, and callers treat ENOLCK as "try the lock instead" — which
    /// IS checked properly.
    fn op_getlk(&self, nodeid: u64, payload: &[u8]) -> Result<Vec<u8>, i32> {
        let req = abi::parse_lock_in(payload).ok_or(abi::EINVAL)?;
        let fh = self.handle_for(nodeid, false)?;
        match self.fs.test_lock(fh, req.owner, req.kind, req.start, req.len) {
            Ok(conflict) => Ok(abi::getlk_out(conflict.as_ref())),
            Err(e) if is_version_mismatch(&e) => Err(abi::ENOLCK),
            Err(e) => Err(errno_of(&e)),
        }
    }

    /// Record that `owner` holds something on `nodeid`, pinning the cached
    /// handle against eviction (releasing it would release the lock).
    fn note_owner(&self, nodeid: u64, owner: u64) {
        self.lock_owners
            .lock()
            .unwrap()
            .entry(nodeid)
            .or_default()
            .insert(owner);
        self.mark_locked(nodeid, true);
    }

    /// `owner` released everything it held on `nodeid`; unpin the handle once
    /// no owner is left.
    fn forget_owner(&self, nodeid: u64, owner: u64) {
        let now_free = {
            let mut owners = self.lock_owners.lock().unwrap();
            match owners.get_mut(&nodeid) {
                Some(set) => {
                    set.remove(&owner);
                    set.is_empty() && owners.remove(&nodeid).is_some()
                }
                None => false,
            }
        };
        if now_free {
            self.mark_locked(nodeid, false);
        }
    }

    // -------------------------------------------------------- mutations

    fn op_create(&self, parent: u64, payload: &[u8], dir: bool) -> Result<Vec<u8>, i32> {
        let (mode, name_bytes) = abi::parse_create_in(payload).ok_or(abi::EINVAL)?;
        let name = name_of(name_bytes)?;
        let perm = mode & 0o7777;
        self.forget_dir(parent);
        if dir {
            let (ino, attr) = self.fs.mkdir(parent, name, perm).map_err(|e| errno_of(&e))?;
            return Ok(self.kernel_attr(ino, &attr).to_bytes());
        }
        let flags = OpenFlags {
            read: true,
            write: true,
            ..OpenFlags::default()
        };
        let (ino, fh, attr) = self
            .fs
            .create(parent, name, perm, flags)
            .map_err(|e| errno_of(&e))?;
        // Keep the handle: a create is nearly always followed by writes on the
        // same file, and the ABI gives us no other moment to open it.
        for stale in self.install_handle(ino, fh, true) {
            self.fs.release(stale);
        }
        Ok(self.kernel_attr(ino, &attr).to_bytes())
    }

    fn op_remove(&self, parent: u64, payload: &[u8], dir: bool) -> Result<Vec<u8>, i32> {
        let name = name_of(payload)?;
        // Resolve before removing: afterwards there is no path to look up, and
        // a cached handle to a deleted file must not linger.
        let victim = self
            .fs
            .ino
            .path_of(parent)
            .and_then(|p| self.fs.ino.ino_of(&p.join(name)));
        let res = if dir {
            self.fs.rmdir(parent, name)
        } else {
            self.fs.unlink(parent, name)
        };
        res.map_err(|e| errno_of(&e))?;
        self.forget_dir(parent);
        if let Some(ino) = victim {
            self.drop_handle(ino);
        }
        Ok(Vec::new())
    }

    fn op_rename(&self, parent: u64, payload: &[u8]) -> Result<Vec<u8>, i32> {
        let (newparent, from, to) = abi::parse_rename_in(payload).ok_or(abi::EINVAL)?;
        let from = name_of(from)?;
        let to = name_of(to)?;
        // The kernel enforces RENAME_NOREPLACE itself before sending, so a
        // rename that reaches us is always allowed to replace.
        self.fs
            .rename(parent, from, newparent, to, true)
            .map_err(|e| errno_of(&e))?;
        self.forget_dir(parent);
        self.forget_dir(newparent);
        Ok(Vec::new())
    }

    fn op_setattr(&self, nodeid: u64, payload: &[u8]) -> Result<Vec<u8>, i32> {
        let si = abi::parse_setattr_in(payload).ok_or(abi::EINVAL)?;
        let size = (si.valid & abi::SETATTR_SIZE != 0).then_some(si.size);
        let mode = (si.valid & abi::SETATTR_MODE != 0).then_some(si.mode & 0o7777);
        let mtime = (si.valid & abi::SETATTR_MTIME != 0)
            .then(|| UNIX_EPOCH.checked_add(Duration::from_nanos(si.mtime_ns)))
            .flatten();
        let attr = self
            .fs
            .setattr(nodeid, size, mtime, mode)
            .map_err(|e| errno_of(&e))?;
        Ok(self.kernel_attr(nodeid, &attr).to_bytes())
    }

    /// Drop a cached listing whose directory we just changed.
    fn forget_dir(&self, nodeid: u64) {
        self.dirs.lock().unwrap().remove(&nodeid);
    }

    // ---------------------------------------------------- notifications

    fn nodeid_of(&self, path: &RelPath) -> Option<u64> {
        if path.is_root() {
            Some(ROOT_INO)
        } else {
            self.fs.ino.ino_of(path)
        }
    }

    fn is_dir(&self, nodeid: u64) -> bool {
        self.kinds.lock().unwrap().get(&nodeid).copied().unwrap_or(false)
    }

    fn record_kind(&self, nodeid: u64, isdir: bool) {
        self.kinds.lock().unwrap().insert(nodeid, isdir);
    }

    /// Turn one server event into the notification the kernel needs to re-emit
    /// it as a genuine fsnotify event.
    ///
    /// MAY BLOCK ON THE NETWORK: `Created`/`Modified` stat the path to learn
    /// its kind and new size. Call it from a plain thread — never from the
    /// event pump's async task.
    pub fn notification_for(&self, ev: &FsEvent) -> Option<Notification> {
        let (parent_path, name) = ev.path.split()?;
        let parent = self.nodeid_of(&parent_path)?;
        let mut n = Notification {
            code: abi::NOTIFY_ATTRIB,
            parent,
            parent2: 0,
            size: 0,
            isdir: false,
            name: name.to_string(),
            name2: String::new(),
        };
        match &ev.kind {
            EventKind::Created | EventKind::Modified => {
                // The lookup also refreshes the attr the kernel will ask for
                // the moment a watcher reacts to this event.
                let (ino, attr) = self.fs.lookup(parent, name).ok()?;
                n.code = if matches!(ev.kind, EventKind::Created) {
                    abi::NOTIFY_CREATE
                } else {
                    abi::NOTIFY_MODIFY
                };
                n.size = attr.size;
                n.isdir = matches!(attr.kind, FileKind::Dir);
                self.record_kind(ino, n.isdir);
                self.forget_dir(parent);
                self.refresh_handle(ino);
            }
            EventKind::AttrChanged => {
                n.code = abi::NOTIFY_ATTRIB;
                if let Some(id) = self.nodeid_of(&ev.path) {
                    n.isdir = self.is_dir(id);
                    // A truncate reaches us as an attr change on some
                    // backends, and bytes prefetched over the longer file
                    // would go on answering for the range past the new EOF.
                    self.refresh_handle(id);
                }
            }
            EventKind::Removed => {
                n.code = abi::NOTIFY_DELETE;
                if let Some(id) = self.nodeid_of(&ev.path) {
                    n.isdir = self.is_dir(id);
                    self.drop_handle(id);
                }
                self.forget_dir(parent);
            }
            EventKind::RenamedFrom { to } => {
                let (to_parent_path, to_name) = to.split()?;
                let parent2 = self.nodeid_of(&to_parent_path)?;
                n.code = abi::NOTIFY_RENAME;
                n.parent2 = parent2;
                n.name2 = to_name.to_string();
                // `RemoteFs::apply_events` has already retargeted the inode
                // table, so the surviving nodeid is the destination's.
                if let Some(id) = self.nodeid_of(to) {
                    n.isdir = self.is_dir(id);
                    // The destination inherits the source's nodeid, so the
                    // handle cached under it outlives the move and would
                    // answer reads of the new name from bytes prefetched
                    // before it.
                    self.refresh_handle(id);
                }
                self.forget_dir(parent);
                self.forget_dir(parent2);
            }
            // Nothing addressable: the client flushed its caches, and the
            // kernel re-asks us on the next operation anyway.
            EventKind::ResyncRequired => return None,
        }
        Some(n)
    }
}

/// Did this fail only because the agent is older than the operation needs?
/// (The same predicate the FUSE backend keys its fallback on.)
fn is_version_mismatch(e: &FsError) -> bool {
    matches!(e, FsError::Remote(ErrorCode::VersionMismatch))
}
