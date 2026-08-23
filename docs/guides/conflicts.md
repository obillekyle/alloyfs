# Write conflicts

By default a mount is last-writer-wins. Two machines with the same file open,
both saving: the later save wins and the earlier one is gone. Every network
filesystem behaves this way, and it is almost always what you want — the
alternative is an editor that sometimes refuses to save.

When you would rather be stopped:

```bash
alloyfs mount ssh://host/projects /mnt/p --detect-conflicts
```

Or per mount, in a config file:

```yaml
detect_conflicts: true
```

## What it does

Each write carries the file version the handle last saw. If the file has changed
since, the server **refuses** the write: nothing is written, and the application
gets `EIO`.

It is a refusal, not a report. Being told your colleague's edit was overwritten,
*after* it was overwritten, is not a safeguard.

## Sharp edges

- **Off is the default and stays the default.** A mount without the flag sends
  no version and behaves exactly as before.
- **Whole-file granularity.** The version bumps on any write, so two people
  editing opposite ends of one file still conflict.
- **A refused large write may be partial.** Writes are chunked; if the conflict
  is detected on a later chunk, earlier chunks have landed. The log names the
  offset.
- **Your own writes never conflict with themselves.** Each chunk advances the
  expected version, so a large write does not trip over its own bumps.


## The agent also reports collisions it cannot prevent

Independently of `--detect-conflicts`, the agent warns when **two different
sessions hold one path open for writing at the same time**:

```
WARN two sessions have this file open for writing  path=db.sqlite session=7 other=4
```

This exists because of the Windows lock gap. WinFsp services lock requests
inside its own kernel driver and exposes no callback, so a Windows client's
byte-range locks are enforced on its own machine and nowhere else — two Windows
machines writing one SQLite database through the same export believe they are
serialised and are not. The agent is never asked for the lock, so it cannot
enforce anything; but it is the one process that sees both opens, so it is the
only place the collision can be observed at all.

**It reports, it never refuses.** Refusing the second open would invent a
restriction the protocol never promised and would break the ordinary case of
one client reopening a file it already has open. Two handles in the *same*
session are not reported for the same reason — one machine's own locks do
apply there.

### What it does not cover

- **One agent only.** The registry lives in the agent process, so it sees two
  clients of one agent. Two agents exporting the same directory — a second
  `alloyfs serve` on the same machine, or an export reached through a
  different host — each see one writer and neither warns.
- **It is not a lock.** By the time the warning is written, both writers are
  already running. It turns silent corruption into something findable in
  `alloyfs logs`; it does not stop it.
- **Use `--detect-conflicts` when you want to be stopped**, or keep the
  database off the mount. On Linux-to-Linux mounts, byte-range locks are
  forwarded and work properly — see [Locking](#/guides/locking).

## Sync mode ignores this

[Sync mode](#/guides/sync-mode) pre-checks conflicts and keeps the loser as
`.sync-conflict-<timestamp>`, which is a better answer when reconciling a whole
directory than failing one save.
