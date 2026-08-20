# Limitations

By design or by platform. Everything here is known and deliberate; none of it is
a bug report.

## Locking

**Byte-range locks work on `--backend fuse` and `--backend kernel` against a
v7 agent**, including `F_GETLK`. Against an older agent, taking a lock coarsens
to the whole file and releasing one (or probing with `F_GETLK`) refuses with
`ENOLCK` — a coarsened release drops every lock the handle holds, which is
worse than not answering.

**`flock()` is local on `--backend fuse`.** The mount advertises POSIX lock
support but not `FUSE_FLOCK_LOCKS`, so the kernel handles it without contacting
the agent. It is forwarded on `--backend kernel`.

**Locks are not forwarded at all on Windows.** WinFsp services lock requests
entirely inside its own kernel driver — its filesystem interface has no lock
callback to implement — so byte-range locks are fully correct between processes
on ONE Windows machine and provide no mutual exclusion whatsoever between
machines sharing an export.

**Journal-mode SQLite works** on `--backend fuse` at v7. **WAL mode does not,
and cannot**: it requires all processes to share memory, which processes on
different machines cannot do. That is SQLite's own constraint, not this
filesystem's. Postgres remains out entirely.

## Change notification

**`inotify` does not fire for remote changes on a FUSE mount.** The kernel only
generates it from local VFS activity and FUSE has no passthrough. Two ways
around it, both shipped: [`--backend kernel`](#/backends/kernel-module) and
[sync mode](#/guides/sync-mode). Windows mounts get native events. Polling
watchers work everywhere.

## Links

**Hard links share content but not cache coherence.** Two names for one inode
are a real hard link on the server, so a write through one changes the file
both see. But the client keys its caches by path and cannot know the two names
are the same file, so a read through the *other* name may serve bytes cached
before the write. Fixing it needs a stable inode identity on the wire, which the
protocol does not carry; NFS has the same hole.

**Symlinks are not synced by sync mode** — there is no wire representation.
They work fine on mounts.

**Creating a symlink on Windows needs privilege** —
`SeCreateSymbolicLinkPrivilege`, meaning administrator or Developer Mode.
Reading them is unprivileged.

## Platform

**Windows volumes are case-sensitive**, to mirror Linux exports faithfully.

**No `mmap` on `--backend kernel`.** There is no page cache for file data; reads
always go to the daemon. Simple, and always coherent.

**Hard links are not available through WinFsp.**

## Performance

**Throughput is roughly scp class** over SSH. Metadata-heavy workloads are
RTT-bound, mitigated by attr-priming readdir, pipelined 128 KiB chunks and
caching. A sequential read reaches the transport's ceiling: measured at 97–99%
of what `alloyfs bench` achieves with no filesystem in the way.

## Network partition

Write-through means **acknowledged writes are never lost**. In-flight operations
fail with `EIO`. Server-side leases free a dead client's locks after ~30 s.
Mounts reconnect automatically; a handle whose lock could not be restored is
poisoned rather than silently continuing.
