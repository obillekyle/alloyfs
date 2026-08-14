//! Linux mount backend #2: the AlloyFS kernel module instead of FUSE.
//!
//! FUSE cannot turn a remote change into a real inotify event — there is no
//! kernel interface for "this file changed and nobody on this machine touched
//! it", only cache invalidation. The `alloyfs` module adds one, and this crate
//! is the daemon on the other side of it: it opens `/dev/alloyfs`, hands that
//! descriptor to `mount(2)`, and then answers the kernel's requests out of
//! `RemoteFs` — the same facade the FUSE backend drives.
//!
//! Layout:
//!
//! * [`abi`] — the wire format, mirroring `kernel/alloyfs/uapi/alloyfs.h`.
//! * [`server`] — one request in, one response out. No I/O, no libc.
//! * [`notify`] — server events → unsolicited notifications (`unique == 0`).
//! * `dev` (Linux only) — the device, `mount(2)`, and the serving loop.
//!
//! Only the last of those is platform-specific, so `cargo test` covers the
//! protocol on every host we build on.

pub mod abi;
pub mod notify;
pub mod server;

pub use notify::EventSink;
pub use server::Server;

#[cfg(target_os = "linux")]
mod dev;

#[cfg(target_os = "linux")]
pub use dev::{device_available, mount, MountHandle, DEVICE};
