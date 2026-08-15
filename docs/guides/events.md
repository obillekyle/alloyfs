# Change events

The agent watches every export and pushes changes to connected clients:
debounced, coalesced, and sequenced so a client that reconnects can resume from
where it left off rather than re-scanning.

## What clients do with them

**Windows mounts** re-emit them natively through `ReadDirectoryChangesW`, so
editors and file managers refresh by themselves.

**Linux FUSE mounts** use them to invalidate kernel caches, so the next `stat`
or read is fresh. They do **not** fire `inotify` — see below.

**Sync mode** uses them to drive reconciliation.

## Watching from a script

```bash
alloyfs events tcp://127.0.0.1:7440/projects
```

NDJSON on stdout, one event per line — usable from anything that can read a
pipe. `--since <SEQ>` replays from a sequence number if you missed a window.

The agent also serves an SSE stream at
`/api/exports/<name>/events` — see the [HTTP API](#/reference/http-api).

## The inotify problem, and two ways around it

The Linux kernel only generates `inotify` from local VFS activity, and FUSE has
no passthrough — the 2021 RFC was never merged. So on a FUSE mount, a change
made on another machine can invalidate caches but cannot fire a watch. A
file-watcher inside the mount will not see it.

Two real answers:

1. **[`--backend kernel`](#/backends/kernel-module)** — the AlloyFS kernel module
   injects genuine `fsnotify` events, so a remote change is indistinguishable
   from a local one to any watcher.
2. **[Sync mode](#/guides/sync-mode)** — the files are ordinary local files, so
   every watcher already works.

Polling watchers (VS Code's fallback, `git status`) work everywhere regardless.
