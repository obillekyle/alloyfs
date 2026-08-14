//! `/dev/alloyfs` + `mount(2)`: the Linux half.
//!
//! THE FD RULE: the kernel resolves `-o fd=N` in the MOUNTING process's
//! descriptor table, so the process that opens the device must be the process
//! that mounts. That is why `mount` below opens the device itself and calls
//! `mount(2)` inline instead of shelling out to mount(8).
//!
//! THE ORDER RULE: `fill_super` sends a GETATTR for the root and sleeps until
//! it is answered, so the serving thread has to be running BEFORE `mount(2)`
//! is called — otherwise the mount deadlocks against its own daemon.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use alloyfs_client::RemoteFs;
use anyhow::{bail, Context};

use crate::abi;
use crate::notify::{self, EventSink, Frames};
use crate::server::Server;

pub const DEVICE: &str = "/dev/alloyfs";

/// Is the kernel module loaded? The CLI uses this to fail with something
/// better than ENOENT when `--backend kernel` is asked for on a machine that
/// has never had the module installed.
pub fn device_available() -> bool {
    Path::new(DEVICE).exists()
}

/// An open connection to the device. One per mount; every thread shares it.
struct Dev {
    fd: libc::c_int,
}

impl Dev {
    fn open() -> io::Result<Self> {
        let path = CString::new(DEVICE).expect("device path has no NUL");
        // Non-blocking + poll so the serving loop can notice a shutdown
        // request; a thread parked in a blocking read() cannot be woken by
        // closing the fd underneath it.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Wait until a request is pending. `false` = timed out or interrupted.
    fn wait_readable(&self, timeout_ms: libc::c_int) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(err);
        }
        // POLLERR (the connection was torn down) counts as readable: the
        // read() below is what turns it into the ENODEV that ends the loop.
        Ok(rc > 0)
    }

    fn read_request(&self, buf: &mut [u8]) -> io::Result<usize> {
        debug_assert!(buf.len() >= abi::READ_BUF_LEN);
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

impl Frames for Arc<Dev> {
    /// One response (or notification) per write(2) — the device takes exactly
    /// one frame per call and never partially accepts one.
    fn write_frame(&self, buf: &[u8]) -> io::Result<()> {
        let n = unsafe { libc::write(self.fd, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize != buf.len() {
            return Err(io::Error::other(format!(
                "device took {n} of {} bytes",
                buf.len()
            )));
        }
        Ok(())
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Handed to the caller once the mount is live: where to feed server events,
/// and how to take the mount down again.
pub struct MountHandle {
    sink: EventSink,
    stop: Arc<AtomicBool>,
    target: CString,
}

impl MountHandle {
    /// Sink for `RemoteFs::start_event_pump` — this is what makes remote
    /// changes arrive as real inotify events.
    pub fn events(&self) -> EventSink {
        self.sink.clone()
    }

    /// Unmount and end the serving loop. Lazy (MNT_DETACH) so a process
    /// sitting inside the mount cannot make Ctrl-C hang.
    pub fn shutdown(&self) {
        let rc = unsafe { libc::umount2(self.target.as_ptr(), libc::MNT_DETACH) };
        if rc != 0 {
            tracing::warn!(error = %io::Error::last_os_error(), "umount failed");
        }
        // Even a successful umount only ends the loop once the superblock is
        // actually dropped; the flag guarantees it either way.
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn do_mount(fd: libc::c_int, target: &CString) -> io::Result<()> {
    let source = CString::new("none").unwrap();
    let fstype = CString::new("alloyfs").unwrap();
    let data = CString::new(format!("fd={fd}")).unwrap();
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            data.as_ptr().cast(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read requests, serve each on the blocking pool, write each answer back.
/// Answers may overtake each other — the kernel matches on `unique`, so a slow
/// read never blocks a fast getattr queued behind it.
fn serve_loop(dev: Arc<Dev>, server: Arc<Server>, rt: tokio::runtime::Handle, stop: Arc<AtomicBool>) {
    let mut buf = vec![0u8; abi::READ_BUF_LEN];
    while !stop.load(Ordering::Relaxed) {
        match dev.wait_readable(200) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) => {
                tracing::error!(error = %e, "poll on the device failed");
                return;
            }
        }
        loop {
            let n = match dev.read_request(&mut buf) {
                Ok(n) => n,
                Err(e) => match e.raw_os_error() {
                    Some(libc::EAGAIN) => break, // drained
                    Some(libc::EINTR) => break,
                    // The superblock is gone (unmounted) or the connection was
                    // aborted: this is the normal end of a mount's life.
                    Some(libc::ENODEV) => {
                        tracing::info!("connection closed by the kernel");
                        return;
                    }
                    _ => {
                        tracing::error!(error = %e, "reading a request failed");
                        return;
                    }
                },
            };
            if n < abi::IN_HEADER_LEN {
                tracing::warn!(n, "short request from the kernel; ignored");
                continue;
            }
            let req = buf[..n].to_vec();
            let server = server.clone();
            let dev = dev.clone();
            rt.spawn_blocking(move || match server.handle(&req) {
                Some(resp) => {
                    if let Err(e) = dev.write_frame(&resp) {
                        // ENOENT here is benign: the caller was killed and the
                        // kernel already abandoned the request.
                        tracing::debug!(error = %e, "response not delivered");
                    }
                }
                None => tracing::warn!("malformed request from the kernel; dropped"),
            });
        }
    }
}

/// Mount `fs` at `mountpoint` and serve it until unmounted (blocking — run it
/// on a blocking thread). `on_ready` receives the handle once the mount is
/// live, before any application can touch it.
///
/// Must be called from within a tokio runtime: requests are served on its
/// blocking pool, which is where the concurrency comes from.
pub fn mount(
    fs: Arc<RemoteFs>,
    mountpoint: &Path,
    volume_name: &str,
    on_ready: impl FnOnce(MountHandle) + Send,
) -> anyhow::Result<()> {
    if !device_available() {
        bail!(
            "{DEVICE} does not exist — the alloyfs kernel module is not loaded (try: sudo modprobe alloyfs)"
        );
    }
    if !mountpoint.is_dir() {
        bail!("mountpoint {} is not a directory", mountpoint.display());
    }
    let rt = tokio::runtime::Handle::try_current()
        .context("the kernel mount backend must run inside a tokio runtime")?;
    let target =
        CString::new(mountpoint.as_os_str().as_bytes()).context("mountpoint contains an interior NUL")?;

    let dev = Arc::new(Dev::open().map_err(|e| match e.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => {
            anyhow::anyhow!("cannot open {DEVICE}: {e} (the device is root-only)")
        }
        _ => anyhow::anyhow!("cannot open {DEVICE}: {e}"),
    })?);
    let server = Arc::new(Server::new(fs));
    let stop = Arc::new(AtomicBool::new(false));

    // The serving loop FIRST: mount(2) below blocks on a root GETATTR that
    // only this thread can answer.
    let serve = {
        let (dev, server, rt2, stop) = (dev.clone(), server.clone(), rt.clone(), stop.clone());
        std::thread::Builder::new()
            .name("alloyfs-kernel".into())
            .spawn(move || serve_loop(dev, server, rt2, stop))
            .context("spawning the device serving thread")?
    };

    tracing::info!(mountpoint = %mountpoint.display(), volume = volume_name, "mounting (kernel)");
    let mounted = do_mount(dev.fd, &target);
    if let Err(e) = mounted {
        stop.store(true, Ordering::Relaxed);
        let _ = serve.join();
        let hint = match e.raw_os_error() {
            Some(libc::EPERM) => " (mounting needs CAP_SYS_ADMIN — run as root)",
            Some(libc::ENODEV) => " (no alloyfs filesystem registered — is the module loaded?)",
            _ => "",
        };
        bail!("mount({}) failed: {e}{hint}", mountpoint.display());
    }

    let (sink, rx) = notify::channel();
    let notifier = {
        let (server, dev, stop) = (server.clone(), dev.clone(), stop.clone());
        std::thread::Builder::new()
            .name("alloyfs-kernel-notify".into())
            .spawn(move || notify::run(rx, server, dev, stop))
            .context("spawning the notification thread")?
    };

    on_ready(MountHandle {
        sink,
        stop: stop.clone(),
        target,
    });

    let _ = serve.join();
    stop.store(true, Ordering::Relaxed);
    let _ = notifier.join();
    // Hand every borrowed handle back to the server before the connection goes.
    server.close_handles();
    tracing::info!("unmounted");
    Ok(())
}
