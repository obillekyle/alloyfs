# Locking

`fcntl(F_SETLK)` and `fcntl(F_GETLK)` on a Linux mount reach the agent, so a
lock taken on one machine excludes the others. Byte ranges are honoured as
ranges from protocol v7 onward. The limits below are real and worth reading —
two of them are sharp.

## What is supported

**POSIX byte-range advisory locks**, shared or exclusive, on `--backend fuse`
against a v7 agent. Locks conflict when their ranges overlap, at least one is
exclusive, and their owners differ — so two readers share a range, a writer
excludes them, and disjoint ranges never interact.

Releasing part of a held range splits it: the rest stays held. Re-locking part
of a range with a different kind replaces exactly that part.

**`fcntl(F_GETLK)`** reports the lock that would block a request, or `F_UNLCK`
when the range is free. It is answered by the agent, never locally — a local
answer would report "free" while another machine held the range.

Blocking waits (`F_SETLKW`) work: the request sleeps until the range is free,
bounded by peer liveness rather than a fixed timeout, so it fails fast if the
holder's machine disappears instead of hanging forever.

An owner never conflicts with itself, so an upgrade or downgrade on a range it
already holds is granted. An upgrade that cannot be granted leaves the existing
lock in place rather than dropping it.

## Against a pre-v7 agent

A v7 client talking to an older agent falls back to whole-file locks:

- **Taking** a range lock coarsens to the whole file. That claims more than was
  asked for, which is safe if inconvenient.
- **Releasing** one refuses with `ENOLCK` rather than coarsening. A coarsened
  release drops every lock the handle holds, so an application releasing one of
  two ranges would be left believing it still held the other while the agent
  held nothing. Refusing is the honest answer.
- **`F_GETLK`** returns `ENOLCK`.

Upgrade the agent to get ranges; the client and agent negotiate this per
connection, so a mixed fleet works.

## `flock()` is LOCAL on `--backend fuse`

`flock()` does not reach the agent on the FUSE backend — the mount advertises
POSIX lock support but not `FUSE_FLOCK_LOCKS`, so the kernel handles `flock()`
itself and the lock never leaves the machine. Two processes on one machine
exclude each other; two machines do not.

It IS forwarded on `--backend kernel`, which implements both `.lock` and
`.flock`.

## `--backend kernel` has ranges too (module ABI 4)

The kernel module forwards byte ranges since its ABI 4: every lock op carries
the owner and the exact `(start, len)` in fcntl's terms, `F_GETLK` asks the
agent and reports the holder's kind and range (with `l_pid = -1`, the remote
convention — the holder's pid means nothing on this machine), and releasing
part of a range releases exactly that part. The same pre-v7 agent fallback
applies as on FUSE: taking coarsens, releasing and `F_GETLK` refuse with
`ENOLCK`. The module and daemon ship together and check the ABI at compile
time, so mixing an old module with a new daemon is not a supported state.

## Windows forwards nothing

WinFsp services lock requests entirely inside its own kernel driver; its
filesystem interface exposes no lock callback, so there is nothing for a
userspace filesystem to forward. Byte-range locks are fully correct between
processes on one Windows machine — better fidelity than the Linux backends —
and give no mutual exclusion at all between machines sharing an export.

## What happens when a client dies

A lock is released when its handle closes. If a client vanishes without
disconnecting, the agent's lease reaper frees its handles and locks after about
30 seconds.

Locks are keyed by handle rather than by process, which is the behaviour of
open-file-description locks: closing one file descriptor does not drop locks
taken through another.

## Across a reconnect

Every range a handle held is replayed onto the new connection. If any one of
them cannot be restored, that handle is **poisoned** — reads, writes, locks and
flushes on it return `EIO`. Restoring some of what was held and reporting
success would leave the application believing in exclusion it no longer has,
which is the outcome this exists to prevent.

## Databases

**Journal-mode SQLite works** on `--backend fuse` against a v7 agent, across
machines. Its locking protocol depends on three disjoint byte regions
(`PENDING`, `RESERVED`, and a 510-byte shared range) and on releasing one
without releasing the others — which is exactly what whole-file coarsening
could not express, and why earlier versions of this page said not to try.

**WAL mode cannot work**, here or on any network filesystem, and no amount of
work on this one will change that. WAL requires every process to share memory,
which processes on different machines cannot do; SQLite documents the
restriction itself. Use `journal_mode=DELETE`, `TRUNCATE` or `PERSIST`.
`locking_mode=EXCLUSIVE` avoids the shared memory but permits exactly one
connection ever, which buys nothing over journal mode.

**Postgres is still out.** It needs mmap'd shared memory and much stronger
durability guarantees than write-through RPC provides.

Also note there is no `mmap` at all on `--backend kernel`, so nothing that maps
a file — including executing a binary from the mount — works there.
