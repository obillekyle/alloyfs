# Locking

`flock()` and `fcntl(F_SETLK)` on a mount reach the agent, so a lock taken on
one machine excludes the others.

## What is supported

**Whole-file advisory locks**, shared or exclusive. Byte ranges are coarsened to
the whole file — safe (over-locking) rather than unsafe (under-locking).

Blocking waits (`F_SETLKW`) work: the request sleeps until the holder releases,
bounded by peer liveness rather than a fixed timeout, so it fails fast if the
holder's machine disappears instead of hanging forever.

## What happens when a client dies

A lock is released when its handle closes. If a client vanishes without
disconnecting, the agent's lease reaper frees its handles and locks after about
30 seconds.

## Across a reconnect

Locks are replayed onto the new connection. If one cannot be restored, that
handle is **poisoned** — reads, writes, locks and flushes on it return `EIO`.
Mutual exclusion may have been broken, and the application has to find out.

## Backend differences

Locks are forwarded on both Linux backends. One gap on `--backend kernel`:

**`fcntl(F_GETLK)` returns `ENOLCK`.** The protocol has no way to ask *who*
holds a lock, and answering from the local list would report "free" while
another machine held it. Callers that see `ENOLCK` fall back to attempting the
lock, which is checked properly.

## Do not host databases

SQLite and Postgres want byte-range locks and `F_GETLK`. Neither is available in
the form they need. Put the database on local disk.
