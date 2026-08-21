//! Linux mount backend: adapts fuser's callback dialect to `RemoteFs`.
//!
//! fuser 0.17 API notes (they bit us once): `Filesystem` methods take `&self`,
//! inode/handle numbers are newtypes (`INodeNo`, `FileHandle`), errors are the
//! typed `Errno`, and `mount2` takes a `Config`.
#![cfg(unix)]

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    MountOption, OpenAccMode, OpenFlags as FuseOpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyLock, ReplyOpen, ReplyStatfs, ReplyWrite, Request as FuseRequest,
    SessionACL, TimeOrNow, WriteFlags,
};

use alloyfs_client::{FsError, RemoteFs, ROOT_INO};
use alloyfs_proto::{Attr, FileKind, OpenFlags};

/// Kernel-side cache lifetime for attrs/entries we reply with: 30 s,
/// revoked by events. The pump calls `apply_events_native` below, which
/// pushes `inval_entry`/`inval_inode` into the kernel per changed path, so
/// a server-side change reaches the dcache within notify latency; the TTL
/// only bounds staleness if the pump dies silently — the same role the
/// user-mode 5 s floor plays. Repeat stats inside the window are answered
/// by the kernel without a round trip into this process (mirrors the
/// WinFsp backend's FileInfoTimeout, same value, same contract).
const KERNEL_TTL: Duration = Duration::from_secs(30);

/// Shared with the kernel-module backend: one Linux errno table, not two.
/// They had already drifted on `NoSuchExport` before this was unified.
fn errno(e: &FsError) -> Errno {
    Errno::from_i32(alloyfs_client::posix_errno(e))
}

fn file_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::File => FileType::RegularFile,
        FileKind::Dir => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
    }
}

struct DsFuse {
    fs: Arc<RemoteFs>,
    uid: u32,
    gid: u32,
}

impl DsFuse {
    fn file_attr(&self, ino: u64, attr: &Attr) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: attr.size,
            blocks: attr.size.div_ceil(512),
            atime: attr.mtime,
            mtime: attr.mtime,
            ctime: attr.ctime,
            crtime: attr.ctime,
            kind: file_type(attr.kind),
            perm: (attr.mode & 0o7777) as u16,
            nlink: 1,
            // The server's uid/gid mean nothing on this machine; present
            // everything as owned by whoever mounted.
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn open_flags(flags: FuseOpenFlags) -> OpenFlags {
        let (read, write) = match flags.acc_mode() {
            OpenAccMode::O_RDONLY => (true, false),
            OpenAccMode::O_WRONLY => (false, true),
            OpenAccMode::O_RDWR => (true, true),
        };
        OpenFlags {
            read,
            write,
            truncate: flags.0 & libc::O_TRUNC != 0,
            append: flags.0 & libc::O_APPEND != 0,
            excl: flags.0 & libc::O_EXCL != 0,
        }
    }
}

macro_rules! ok_name {
    ($name:expr, $reply:expr) => {
        match $name.to_str() {
            Some(n) => n,
            None => {
                $reply.error(Errno::EINVAL); // non-UTF-8 names are not served
                return;
            }
        }
    };
}

impl Filesystem for DsFuse {
    fn init(&mut self, _req: &FuseRequest, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        // Without FUSE_POSIX_LOCKS the kernel does per-mount local locking
        // and our setlk forwarding is dead code — locks would never be shared
        // across clients (found the hard way in M6 verification).
        if let Err(unsupported) = config.add_capabilities(fuser::InitFlags::FUSE_POSIX_LOCKS) {
            tracing::warn!(?unsupported, "kernel refused POSIX lock forwarding");
        }
        // Every op here is priced in wire round trips, so the kernel-side
        // ceilings are the real throughput cap: max_background bounds
        // in-flight readahead and async writes (fuser's default is 16, and
        // 64 outstanding 128 KiB requests is only 8 MiB), and the
        // congestion threshold is where the kernel stops pipelining
        // rather than blocking submitters.
        let _ = config.set_max_background(64);
        let _ = config.set_congestion_threshold(48);
        // Optional niceties, kernel permitting: without PARALLEL_DIROPS the
        // kernel serializes same-directory lookups under i_rwsem — exactly
        // the calls a remote fs wants overlapped — and CACHE_SYMLINKS lets
        // the page cache hold link targets instead of re-asking readlink.
        if let Err(unsupported) = config
            .add_capabilities(fuser::InitFlags::FUSE_PARALLEL_DIROPS | fuser::InitFlags::FUSE_CACHE_SYMLINKS)
        {
            tracing::debug!(?unsupported, "kernel lacks optional readdir/symlink niceties");
        }
        Ok(())
    }

    fn lookup(&self, _req: &FuseRequest, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = ok_name!(name, reply);
        match self.fs.lookup(parent.0, name) {
            Ok((ino, attr)) => reply.entry(&KERNEL_TTL, &self.file_attr(ino, &attr), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn forget(&self, _req: &FuseRequest, ino: INodeNo, _nlookup: u64) {
        self.fs.ino.forget(ino.0);
    }

    fn getattr(&self, _req: &FuseRequest, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.fs.getattr(ino.0) {
            Ok(attr) => reply.attr(&KERNEL_TTL, &self.file_attr(ino.0, &attr)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn setattr(
        &self,
        _req: &FuseRequest,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mtime = mtime.map(|t| match t {
            TimeOrNow::SpecificTime(t) => t,
            TimeOrNow::Now => SystemTime::now(),
        });
        match self.fs.setattr(ino.0, size, mtime, mode) {
            Ok(attr) => reply.attr(&KERNEL_TTL, &self.file_attr(ino.0, &attr)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readdir(
        &self,
        _req: &FuseRequest,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.fs.readdir(ino.0) {
            Ok(e) => e,
            Err(e) => {
                reply.error(errno(&e));
                return;
            }
        };
        // The kernel's offset counts entries (incl. "." / "..") it already got.
        let mut all: Vec<(u64, FileType, String)> = Vec::with_capacity(entries.len() + 2);
        all.push((ino.0, FileType::Directory, ".".into()));
        all.push((ROOT_INO, FileType::Directory, "..".into()));
        for (name, child_ino, attr) in entries {
            all.push((child_ino, file_type(attr.kind), name));
        }
        for (i, (child_ino, ft, name)) in all.into_iter().enumerate().skip(offset as usize) {
            // add() returns true when the reply buffer is full.
            if reply.add(INodeNo(child_ino), (i + 1) as u64, ft, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &FuseRequest, ino: INodeNo, flags: FuseOpenFlags, reply: ReplyOpen) {
        match self.fs.open(ino.0, Self::open_flags(flags)) {
            Ok((fh, _attr)) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn create(
        &self,
        _req: &FuseRequest,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let name = ok_name!(name, reply);
        // Reuse the open conversion rather than re-testing the modifier bits:
        // that copy was uncovered, since the flag tests only exercise
        // `open_flags`. A create is always read+write regardless of the mode
        // the caller asked for.
        let of = OpenFlags {
            read: true,
            write: true,
            ..Self::open_flags(FuseOpenFlags(flags))
        };
        match self.fs.create(parent.0, name, mode, of) {
            Ok((ino, fh, attr)) => reply.created(
                &KERNEL_TTL,
                &self.file_attr(ino, &attr),
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn read(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: FuseOpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.fs.read(fh.0, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: FuseOpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        // `write_at` rather than `write`: at protocol 5 the reply carries the
        // post-write attributes, and it re-seeds the cache with them. Calling
        // `invalidate_attr` here would throw that away and put the round-trip
        // back — the next GETATTR would have to ask again for what the write
        // reply already said. Against an older agent the reply carries nothing
        // and `write_at` invalidates instead, which is exactly what this used
        // to do by hand.
        match self.fs.write_at(fh.0, offset, data) {
            Ok((n, _attr)) => reply.written(n),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn flush(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.fs.flush(fh.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn release(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: FuseOpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.fs.release(fh.0);
        reply.ok();
    }

    fn mkdir(
        &self,
        _req: &FuseRequest,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name = ok_name!(name, reply);
        match self.fs.mkdir(parent.0, name, mode) {
            Ok((ino, attr)) => reply.entry(&KERNEL_TTL, &self.file_attr(ino, &attr), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&self, _req: &FuseRequest, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = ok_name!(name, reply);
        match self.fs.unlink(parent.0, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&self, _req: &FuseRequest, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = ok_name!(name, reply);
        match self.fs.rmdir(parent.0, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rename(
        &self,
        _req: &FuseRequest,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let name = ok_name!(name, reply);
        let newname = ok_name!(newname, reply);
        // RENAME_NOREPLACE asks us to fail if the target exists.
        let noreplace = flags.contains(fuser::RenameFlags::RENAME_NOREPLACE);
        // Every other flag the kernel forwards is unimplemented, and the
        // contract is to say so rather than approximate it. RENAME_EXCHANGE
        // asks for an atomic swap of two names and RENAME_WHITEOUT for an
        // overlayfs whiteout; a plain replacing rename performs neither, so
        // running one as the other destroys the target's content and reports
        // success. `mv --exchange` (coreutils 9.4+) and ostree are the
        // ordinary ways to reach it. bitflags keeps unknown bits via
        // `from_bits_retain`, so this also refuses flags added after this was
        // written instead of silently mistranslating them.
        if !flags.difference(fuser::RenameFlags::RENAME_NOREPLACE).is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }
        match self.fs.rename(parent.0, name, newparent.0, newname, !noreplace) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn link(&self, _req: &FuseRequest, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        // Same-export hard links (what git object storage and bun-style
        // installers use). The link gets its own inode number in our table —
        // a documented simplification (same content, distinct ino).
        let newname = ok_name!(newname, reply);
        match self.fs.link(ino.0, newparent.0, newname) {
            Ok((link_ino, attr)) => reply.entry(&KERNEL_TTL, &self.file_attr(link_ino, &attr), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn symlink(
        &self,
        _req: &FuseRequest,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let link_name = ok_name!(link_name, reply);
        // The target is passed through as the user typed it. It is not a path
        // we resolve — it may be relative, and it may not exist yet. The
        // server decides whether where it lands is allowed.
        let Some(target) = target.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.fs.symlink(parent.0, link_name, target) {
            Ok((ino, attr)) => reply.entry(&KERNEL_TTL, &self.file_attr(ino, &attr), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readlink(&self, _req: &FuseRequest, ino: INodeNo, reply: ReplyData) {
        match self.fs.readlink(ino.0) {
            // No trailing NUL: the kernel takes the reply length as the length
            // of the target, and a NUL inside it becomes part of the name.
            Ok(target) => reply.data(target.as_bytes()),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn setlk(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        _pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        let len = fuse_range_len(start, end);
        let owner = lock_owner.0;
        let result = match typ {
            t if t == libc::F_RDLCK => {
                self.fs
                    .lock_range(fh.0, owner, alloyfs_proto::LockKind::Shared, start, len, sleep)
            }
            t if t == libc::F_WRLCK => {
                self.fs
                    .lock_range(fh.0, owner, alloyfs_proto::LockKind::Exclusive, start, len, sleep)
            }
            t if t == libc::F_UNLCK => self.fs.unlock_range(fh.0, owner, start, len),
            _ => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        // A pre-v7 agent cannot do ranges. Coarsening is what this did for
        // every lock until v7, and as a FALLBACK it is defensible for taking
        // one — it claims more than was asked. It is not defensible for
        // releasing one, where it drops every lock the handle holds, so an
        // unlock against an old peer refuses rather than silently unlocking
        // more than the caller named.
        let result = match result {
            Err(e) if is_version_mismatch(&e) => {
                let kind = match typ {
                    t if t == libc::F_RDLCK => alloyfs_proto::LockKind::Shared,
                    t if t == libc::F_WRLCK => alloyfs_proto::LockKind::Exclusive,
                    _ => {
                        reply.error(Errno::ENOLCK);
                        return;
                    }
                };
                self.fs.lock(fh.0, kind, sleep)
            }
            other => other,
        };
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn getlk(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        // Without this the kernel forwards F_GETLK here — it only answers
        // locally when FUSE_POSIX_LOCKS is absent, and this mount advertises
        // it — and fuser's default replies ENOSYS straight to the application.
        // SQLite issues F_GETLK from `unixCheckReservedLock` whenever a
        // journal file exists and treats a failure as an I/O error, so the
        // missing implementation turned every recovery into a hard failure.
        let kind = match typ {
            t if t == libc::F_RDLCK => alloyfs_proto::LockKind::Shared,
            t if t == libc::F_WRLCK => alloyfs_proto::LockKind::Exclusive,
            _ => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        match self
            .fs
            .test_lock(fh.0, lock_owner.0, kind, start, fuse_range_len(start, end))
        {
            // Free: F_GETLK reports F_UNLCK and leaves the rest untouched.
            Ok(None) => reply.locked(start, end, libc::F_UNLCK, pid),
            Ok(Some(c)) => {
                let ctyp = match c.kind {
                    alloyfs_proto::LockKind::Shared => libc::F_RDLCK,
                    alloyfs_proto::LockKind::Exclusive => libc::F_WRLCK,
                };
                // Back to fuser's inclusive end. A conflict reported with
                // len 0 runs to EOF, which is u64::MAX here.
                let cend = if c.len == 0 {
                    u64::MAX
                } else {
                    c.start.saturating_add(c.len).saturating_sub(1)
                };
                reply.locked(c.start, cend, ctyp, c.pid)
            }
            // ENOLCK against a pre-v7 agent, matching what the kernel backend
            // has always returned — and never a local answer, which would
            // report "free" while another machine held the range.
            Err(e) if is_version_mismatch(&e) => reply.error(Errno::ENOLCK),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn statfs(&self, _req: &FuseRequest, _ino: INodeNo, reply: ReplyStatfs) {
        match self.fs.statfs() {
            Ok((bsize, blocks, bfree)) => {
                reply.statfs(blocks, bfree, bfree, 0, 0, bsize, 255, bsize);
            }
            Err(e) => reply.error(errno(&e)),
        }
    }
}

/// Native re-emission, Linux edition: remote changes can't become inotify
/// events (no kernel support — documented limitation), but we CAN evict the
/// kernel's dentry/attr/page caches so the very next stat/read/listing is
/// fresh instead of waiting out a TTL.
pub fn apply_events_native(fs: &RemoteFs, notifier: &fuser::Notifier, batch: &[alloyfs_proto::FsEvent]) {
    use alloyfs_proto::EventKind;
    let inval_name = |parent: &alloyfs_proto::RelPath, name: &str| {
        if let Some(pino) = if parent.is_root() {
            Some(ROOT_INO)
        } else {
            fs.ino.ino_of(parent)
        } {
            // ENOENT here just means the kernel had nothing cached — fine.
            let _ = notifier.inval_entry(INodeNo(pino), std::ffi::OsStr::new(name));
        }
    };
    for ev in batch {
        match &ev.kind {
            EventKind::Created | EventKind::Removed => {
                if let Some((parent, name)) = ev.path.split() {
                    inval_name(&parent, name);
                }
            }
            EventKind::RenamedFrom { to } => {
                if let Some((parent, name)) = ev.path.split() {
                    inval_name(&parent, name);
                }
                if let Some((parent, name)) = to.split() {
                    inval_name(&parent, name);
                }
            }
            EventKind::Modified | EventKind::AttrChanged => {
                if let Some(ino) = fs.ino.ino_of(&ev.path) {
                    // Whole-file data+attr invalidation (offset 0, len 0 = all).
                    let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
                }
            }
            EventKind::ResyncRequired => {
                // Nothing targeted to do; kernel entries expire via their TTL.
            }
        }
    }
}

/// Mount `fs` at `mountpoint` and serve until unmounted (blocking call —
/// run it on a blocking thread). `on_ready` receives the kernel notifier once
/// the session exists, before dispatch starts.
pub fn mount(
    fs: Arc<RemoteFs>,
    mountpoint: &Path,
    volume_name: &str,
    on_ready: impl FnOnce(fuser::Notifier) + Send,
) -> anyhow::Result<()> {
    // fuser::Config is #[non_exhaustive]: no struct literal allowed outside
    // its crate — construct by default-then-mutate.
    let mut config = Config::default();
    // No AutoUnmount: fuser 0.17 only allows it with allow_other/allow_root
    // (which need /etc/fuse.conf changes). A crashed mount leaves a stale
    // mountpoint that `fusermount3 -u` clears — acceptable for now.
    config.mount_options = vec![
        MountOption::FSName(format!("alloyfs:{volume_name}")),
        MountOption::DefaultPermissions,
    ];
    config.acl = SessionACL::Owner;
    // fuser dispatches on ONE thread unless told otherwise, and every callback
    // runs to completion before the next is read off /dev/fuse. A blocking
    // `fcntl(F_SETLKW)` therefore froze the entire mount — every read, write
    // and stat, from every process — until the lock came free. Worse, if the
    // holder was on this same mount its unlock is itself a FUSE request that
    // could never be dispatched, so the wait never ended and Ctrl-C could not
    // clear it: FUSE_INTERRUPT queues behind the stuck request.
    //
    // SQLite never hit this (it only ever issues non-blocking F_SETLK), but
    // byte ranges make blocking waits genuinely useful, and `python
    // fcntl.lockf` blocks by default.
    //
    // Linux only: fuser refuses n_threads != 1 on other platforms. Four is
    // enough to keep a blocked lock from starving ordinary traffic without
    // multiplying per-request state; `RemoteFs` is Sync throughout (atomics
    // and DashMap), so concurrent callbacks need nothing further.
    #[cfg(target_os = "linux")]
    {
        config.n_threads = Some(4);
        // Four threads against ONE /dev/fuse fd still serialize on that
        // fd's request queue; clone_fd gives each worker its own, which is
        // the whole point of having the threads. Linux-only, like
        // n_threads — fuser errors on it elsewhere.
        config.clone_fd = true;
    }
    let adapter = DsFuse {
        fs,
        // SAFETY: geteuid/getegid are always safe to call.
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    };
    tracing::info!(mountpoint = %mountpoint.display(), "mounting (fuse)");
    // Session (not mount2) so we can hand out the notifier for cache
    // invalidation; dispatch runs on fuser's background thread and we block
    // on join() until unmount.
    let session = fuser::Session::new(adapter, mountpoint, &config)?;
    let bg = session.spawn()?;
    on_ready(bg.notifier());
    bg.join()?;
    tracing::info!("unmounted");
    Ok(())
}

#[cfg(test)]
mod tests;

/// fcntl's `l_len` from fuser's INCLUSIVE end.
///
/// fuser hands the kernel's `fuse_file_lock` through as `(start, end)` with
/// `end` inclusive, and spells "to the end of the file" as `end == u64::MAX`.
/// The wire uses fcntl's own convention, where that is `len == 0`, so the two
/// conversions have to happen exactly here and nowhere else.
fn fuse_range_len(start: u64, end: u64) -> u64 {
    if end == u64::MAX {
        0
    } else {
        end.saturating_sub(start).saturating_add(1)
    }
}

/// Did this fail only because the agent is older than the operation needs?
fn is_version_mismatch(e: &FsError) -> bool {
    matches!(e, FsError::Remote(alloyfs_proto::ErrorCode::VersionMismatch))
}
