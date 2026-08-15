# Offline cache

Small files are cheap to keep locally and expensive to fetch repeatedly. The
auto-download cache stores them on disk so reads come from local storage rather
than the network.

```bash
alloyfs mount ssh://host/projects /mnt/p \
  --auto-cache-max 2M --auto-cache-budget 512M --pin '*.lock'
```

| Flag | Meaning |
|---|---|
| `--auto-cache-max` | Cache files up to this size. `0` disables caching |
| `--auto-cache-budget` | Total on-disk budget; least-recently-used evicted first |
| `--pin <GLOB>` | Always cache matching files regardless of size, exempt from eviction |

CLI mounts default to 2 MB files and a 512 MB budget when neither you nor the
server picks a value.

## Freshness

A cached blob serves reads only while it is provably current: the server's size,
mtime and version must all still match. When a change event arrives the entry is
invalidated and re-fetched, so the cache does not sit on stale bytes waiting for
a timeout.

Disable it entirely with `--auto-cache-max 0` when you want every read to hit
the server — useful when measuring, since otherwise you are timing your disk.

## Managing it

```bash
alloyfs cache            # what is cached, and how much space it uses
alloyfs clear            # drop the cache (never touches the overlay)
```

The cache lives under `~/.alloyfs/cache/<host>/` and is safe to delete at any
time by hand. It is deliberately kept apart from `data/`, which holds the
[overlay](#/guides/excludes) and sync baselines and is not safe to delete.
