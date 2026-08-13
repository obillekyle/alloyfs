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
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags as FuseOpenFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    Request as FuseRequest, SessionACL, TimeOrNow, WriteFlags,
};

use ds_client::{FsError, RemoteFs, ROOT_INO};
use ds_proto::{Attr, ErrorCode, FileKind, OpenFlags};

/// Kernel-side cache lifetime for attrs/entries we reply with. Short on
/// purpose: real invalidation arrives with the event stream in M5.
const KERNEL_TTL: Duration = Duration::from_secs(1);

fn errno(e: &FsError) -> Errno {
    match e {
        FsError::Remote(code) => match code {
            ErrorCode::NotFound => Errno::ENOENT,
            ErrorCode::PermissionDenied => Errno::EACCES,
            ErrorCode::AlreadyExists => Errno::EEXIST,
            ErrorCode::NotADirectory => Errno::ENOTDIR,
            ErrorCode::IsADirectory => Errno::EISDIR,
            ErrorCode::NotEmpty => Errno::ENOTEMPTY,
            ErrorCode::InvalidPath => Errno::EINVAL,
            ErrorCode::BadHandle => Errno::EBADF,
            ErrorCode::ReadOnly => Errno::EROFS,
            ErrorCode::WouldBlock => Errno::EAGAIN,
            ErrorCode::CrossDevice => Errno::EXDEV,
            _ => Errno::EIO,
        },
        FsError::Transport(_) => Errno::EIO,
    }
}

fn file_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::File => FileType::RegularFile,
        FileKind::Dir => FileType::Directory,
        FileKind::Symlink => FileType::Symlink, // presented resolved; only reached transiently
    }
}

struct DsFuse {
    fs: Arc<RemoteFs>,
    uid: u32,
    gid: u32,
}

impl DsFuse {
    fn file_attr(&self, ino: u64, a: &Attr) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: a.mtime,
            mtime: a.mtime,
            ctime: a.ctime,
            crtime: a.ctime,
            kind: file_type(a.kind),
            perm: (a.mode & 0o7777) as u16,
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

    fn readdir(&self, _req: &FuseRequest, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
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
        let of = OpenFlags {
            read: true,
            write: true,
            truncate: flags & libc::O_TRUNC != 0,
            append: flags & libc::O_APPEND != 0,
            excl: flags & libc::O_EXCL != 0,
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
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: FuseOpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.fs.write(fh.0, offset, data) {
            Ok(n) => {
                self.fs.invalidate_attr(ino.0); // size/mtime changed server-side
                reply.written(n);
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn flush(&self, _req: &FuseRequest, _ino: INodeNo, fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
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

    fn mkdir(&self, _req: &FuseRequest, parent: INodeNo, name: &OsStr, mode: u32, _umask: u32, reply: ReplyEntry) {
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
        match self.fs.rename(parent.0, name, newparent.0, newname, !noreplace) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn link(
        &self,
        _req: &FuseRequest,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        // Same-export hard links (what git object storage and bun-style
        // installers use). The link gets its own inode number in our table —
        // a documented simplification (same content, distinct ino).
        let newname = ok_name!(newname, reply);
        match self.fs.link(ino.0, newparent.0, newname) {
            Ok((link_ino, attr)) => {
                reply.entry(&KERNEL_TTL, &self.file_attr(link_ino, &attr), Generation(0))
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn setlk(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        _start: u64,
        _end: u64,
        typ: i32,
        _pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        // POSIX byte-range locks coarsened to whole-file server-side locks —
        // documented limitation (don't run databases on the mount).
        let result = match typ {
            t if t == libc::F_RDLCK => self.fs.lock(fh.0, ds_proto::LockKind::Shared, sleep),
            t if t == libc::F_WRLCK => self.fs.lock(fh.0, ds_proto::LockKind::Exclusive, sleep),
            t if t == libc::F_UNLCK => self.fs.unlock(fh.0),
            _ => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        match result {
            Ok(()) => reply.ok(),
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
pub fn apply_events_native(fs: &RemoteFs, notifier: &fuser::Notifier, batch: &[ds_proto::FsEvent]) {
    use ds_proto::EventKind;
    let inval_name = |parent: &ds_proto::RelPath, name: &str| {
        if let Some(pino) = if parent.is_root() { Some(ROOT_INO) } else { fs.ino.ino_of(parent) } {
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
        MountOption::FSName(format!("drive-sync:{volume_name}")),
        MountOption::DefaultPermissions,
    ];
    config.acl = SessionACL::Owner;
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
