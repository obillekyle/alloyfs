# Locking

`fcntl(F_SETLK)` on a Linux mount reaches the agent, so a lock taken on one
machine excludes the others. Read the limits below before relying on that
sentence — there are three, and two of them are sharp.

## What is supported

**Whole-file advisory locks**, shared or exclusive, on both Linux backends.

Blocking waits (`F_SETLKW`) work: the request sleeps until the holder releases,
bounded by peer liveness rather than a fixed timeout, so it fails fast if the
holder's machine disappears instead of hanging forever.

An upgrade or downgrade on a handle that already holds the file is honoured,
and a handle never conflicts with itself. An upgrade that cannot be granted
leaves the existing lock in place rather than dropping it.

## Byte ranges are discarded, and unlocking is where that bites

A range lock is coarsened to the whole file. Taking one is merely stricter than
asked. **Releasing one is not**: a partial `F_UNLCK` releases everything the
handle held on that file, so an application holding two disjoint ranges and
releasing one is left believing it still holds the other while the agent holds
nothing at all.

That is under-locking, and it is silent. It is also exactly the sequence SQLite
performs on every read transaction, which is the concrete reason the databases
warning below exists.

## `flock()` is LOCAL on `--backend fuse`

`flock()` does not reach the agent on the FUSE backend — the mount advertises
POSIX lock support but not `FUSE_FLOCK_LOCKS`, so the kernel handles `flock()`
itself and the lock never leaves the machine. Two processes on one machine
exclude each other; two machines do not.

It IS forwarded on `--backend kernel`, which implements both `.lock` and
`.flock`.

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

Locks are replayed onto the new connection. If one cannot be restored, that
handle is **poisoned** — reads, writes, locks and flushes on it return `EIO`.
Mutual exclusion may have been broken, and the application has to find out.

## `fcntl(F_GETLK)`

Returns `ENOLCK` on `--backend kernel` and `ENOSYS` on `--backend fuse`. The
protocol has no way to ask *who* holds a lock, and answering from the local
list would report "free" while another machine held it. Callers that see either
fall back to attempting the lock, which is checked properly — except SQLite,
which treats the failure as an I/O error.

## Do not host databases

SQLite and Postgres want byte-range locks and `F_GETLK`. Neither is available
in the form they need, and the partial-unlock behaviour above actively breaks
SQLite's locking protocol rather than merely restricting it. Put the database
on local disk.

SQLite's WAL mode is impossible on any network filesystem regardless of what
this one implements: WAL requires every process to share memory, which
processes on different machines cannot do. That is SQLite's own constraint.
