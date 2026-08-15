# Where things live

One tree, the same shape on both platforms:

```
~/.alloyfs/                    (%USERPROFILE%\.alloyfs on Windows)
  config.yml                   created on first use if absent
  data/<host>/                 DURABLE — losing it loses real work
    overlay/<export>/            files that exist on NO server
    sync/<export>-<tag>.json     sync baselines
  cache/<host>/                DISPOSABLE — delete it freely
    <export>/                    downloaded blobs
    <export>.manifest.json
```

## Why data and cache are split at the top

Everything under `cache/` can be deleted at any moment and costs only a
re-download. Everything under `data/` is unrecoverable — the
[overlay](#/guides/excludes) holds files that exist on no server, and the sync
baselines are what let [sync mode](#/guides/sync-mode) tell a delete from a
create.

A single mixed directory invites "just clear the whole thing", which would
silently destroy the first kind. Hence the split.

## Keyed by host, then export

Paths are keyed by host and port rather than by URL, so mounting the same export
over `ssh://` and `tcp://` shares one cache and one overlay instead of quietly
keeping two.

## Overriding

`--data-dir <PATH>` moves both trees under a root you choose:

```
<root>/data/<host>/
<root>/cache/<host>/
```

Useful for a portable install, or to keep the cache off a small system disk.

## Clearing

```bash
alloyfs cache      # what is cached, and how much space
alloyfs clear      # drop the cache; never touches the overlay
```
