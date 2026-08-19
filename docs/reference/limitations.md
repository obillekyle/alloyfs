# Limitations

By design or by platform. Everything here is known and deliberate; none of it is
a bug report.

## Locking

**Whole-file advisory locks only, and the coarsening is NOT safe.** Byte ranges
are discarded, which over-locks on the way in — but the same coarsening applies
to unlocking, and there it *under*-locks: a partial `F_UNLCK` releases
everything the handle held on that file. An application that locks two disjoint
ranges and releases one believes it still holds the other, while the agent
holds nothing and another machine is free to take the file.

**`fcntl(F_GETLK)` returns `ENOLCK` on `--backend kernel`, and `ENOSYS` on
`--backend fuse`.** The protocol cannot ask who holds a lock, and answering from
the local list would report "free" while another machine held it.

**Locks are not forwarded at all on Windows.** WinFsp services lock requests
entirely inside its own kernel driver — its filesystem interface has no lock
callback to implement — so byte-range locks are fully correct between processes
on ONE Windows machine and provide no mutual exclusion whatsoever between
machines sharing an export.

**Do not host live database files** (SQLite, Postgres) on a shared mount. They
want byte ranges and `F_GETLK`, and the partial-unlock behaviour above means
SQLite loses its read lock during its own normal lock sequence.

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
