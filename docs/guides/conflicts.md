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

## Sync mode ignores this

[Sync mode](#/guides/sync-mode) pre-checks conflicts and keeps the loser as
`.sync-conflict-<timestamp>`, which is a better answer when reconciling a whole
directory than failing one save.
