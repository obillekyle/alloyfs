# Excludes and the overlay

Some directories should never cross the wire. `node_modules`, `target`, `.venv`
— big, regenerable, and different on every machine anyway.

```bash
alloyfs mount ssh://host/projects /mnt/p --exclude node_modules --exclude target
```

Patterns are gitignore-style globs, and repeatable.

## The overlay

An excluded path is not hidden — it is **local**. It lives as a real file in
your overlay directory and the mount routes it there transparently. You can
create, read and write it; the server never hears about it.

That is why the overlay lives under `data/`, not `cache/`:

```
~/.alloyfs/
  data/<host>/overlay/<export>/   ← files that exist on NO server
  cache/<host>/                   ← safe to delete, only costs a re-download
```

Deleting the overlay loses real work. Deleting the cache costs bandwidth. The
split exists so that "just clear the whole thing" cannot destroy the first.

## Two rules worth knowing

**Routing is pattern-based, not existence-based.** An excluded name always means
the local copy, deterministically — so a path cannot flip between local and
remote depending on what happens to exist.

**Crossing the boundary is refused.** Renaming or hard-linking between an
excluded path and a server path fails with `EXDEV`, the same error you get
moving a file between disks. Silently copying would be a surprise.

## Server-suggested excludes

An export can publish suggested settings, so every client mounting it starts
with sensible excludes without being told:

```yaml
exports:
  projects:
    path: /home/you/projects
    client:
      exclude: ["node_modules", "target"]
      auto_cache_max: 2M
```

Your own flags are unioned with the suggestion. `--no-server-defaults` ignores
it entirely.
