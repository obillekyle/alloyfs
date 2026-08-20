# How it works

Three pieces, and one connection between them.

## The agent

`alloyfs serve` publishes folders named in its config. It watches them, hardens
every path against escaping the export root, holds advisory locks, and answers
requests. It never initiates anything.

## The connection

A binary, multiplexed protocol over any ordered byte stream — a TCP socket or
an SSH exec channel. Many requests are in flight at once and replies are matched
by id, so one slow read never blocks the rest. Payloads move in 128 KiB chunks,
and frames above 512 bytes are compressed when compression actually helps.

## The client

The interesting half. `RemoteFs` is a filesystem the mount backends drive:

- **an inode table**, so paths get stable numbers across renames
- **a readahead window** per open file, which is why sequential reads reach the
  transport's ceiling rather than paying a round-trip per block
- **an overlay**, where excluded paths live as real local files
- **an auto-download cache** for small files, so they read at disk speed
- **an event pump** that turns server events into cache invalidation and native
  notifications

## The mount backend

Whatever makes it look like a drive. FUSE on Linux, WinFsp on Windows, or the
[AlloyFS kernel module](#/backends/kernel-module). The backends are thin: they
translate one dialect of filesystem callbacks into `RemoteFs` calls.

That split is deliberate. The logic that can be wrong lives in one place and is
tested on every platform; only the driver plumbing is platform-specific.

## Where writes become durable

Writes into an existing file are write-through: when `write()` returns, the
bytes are on the server, and losing the connection mid-write fails that
operation loudly rather than silently discarding it later.

Bursts of NEW small files — and deletes — are different (v10+): they
acknowledge locally and coalesce for at most ~15 ms into one bulk exchange,
which is what makes an `npm install` or an untar onto a mount cost a handful
of round trips instead of two per file. Three promises bound the window:

- `fsync`/`flush` block until the server has everything the file was ever
  acknowledged for — an application that syncs keeps exactly the durability
  it asked for, and a refused write surfaces THERE as the error it is.
- Taking a lock, renaming, opening a pending path, and unmounting all drain
  the window first.
- `--write-through` turns the window off entirely.
