# Sync mode

A mount is not always what you want. Sync mode gives you an ordinary local
directory that AlloyFS keeps in step with the export, in both directions.

```bash
alloyfs sync ssh://host/projects ~/projects
```

## When to prefer it over a mount

- **You want files on local disk** — for offline work, or because the tool you
  are using is unhappy over a network filesystem.
- **You want `inotify` to work on Linux without the kernel module.** Files in a
  synced directory are real local files, so every watcher, hot-reloader and
  build tool already works. No cooperation from the kernel needed.
- **Latency matters more than freshness.** Reads are local-disk fast.

## How it reconciles

Three-way: local state, remote state, and a stored baseline from the last sync.
That is what lets it tell "you deleted this" apart from "they created it" — a
two-way comparison cannot.

Baselines live under `~/.alloyfs/data/<host>/sync/`, and they are **durable**:
losing one turns the next sync into a first sync.

## Conflicts

Last-writer-wins by default, with the loser preserved as
`<name>.sync-conflict-<timestamp>` rather than discarded. Pick a different rule
with `--policy`:

| Policy | Result |
|---|---|
| `newer` | Later mtime wins (default) |
| `local` | Your copy always wins |
| `remote` | The server's copy always wins |

## What it does not do

**Symlinks are not synced.** There is no wire representation for one, and
inventing a silent approximation would be worse than skipping them.

Sync mode ignores `--detect-conflicts`; it pre-checks conflicts itself, which is
the better answer when reconciling a whole directory rather than one save.
