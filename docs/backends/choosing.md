# Choosing a backend

A backend is whatever makes the export look like a drive. You rarely need to
think about it — the default is right almost always.

| | FUSE | WinFsp | Kernel module |
|---|---|---|---|
| Platform | Linux | Windows | Linux |
| Default | yes | yes (only option) | no |
| Operations | 21 | 19 | 17 |
| Needs installing | `fuse3` | WinFsp | the AlloyFS module, via DKMS |
| Real `inotify` for remote changes | no | n/a | **yes** |
| Native change notifications | no | **yes** | via inotify |
| Advisory locks | yes | yes | yes |
| `fcntl(F_GETLK)` | yes | n/a | **no** (`ENOLCK`) |
| `statfs` / `df` | yes | yes | yes |
| Hard links | yes | no | yes |
| Symlinks | yes | yes (reparse points) | yes |

## Pick FUSE unless you need otherwise

It has the widest operation coverage and needs nothing installed beyond `fuse3`.

## Pick the kernel module when watchers matter

Exactly one thing justifies it: **remote changes become genuine `inotify`
events**. If a file watcher, hot-reloader or build tool has to notice changes
another machine made, FUSE cannot do it and this can. See
[The Linux kernel module](#/backends/kernel-module).

```bash
alloyfs mount tcp://host/projects /mnt/p --backend kernel
```

If you do not need that, skip it — it is a kernel module, and not installing one
is always the cheaper option.

## Windows

WinFsp is the only backend, and there is no flag. Two things to know:

- Symlinks are reparse points, and **creating** one needs
  `SeCreateSymbolicLinkPrivilege` — administrator, or Developer Mode. Reading
  them is unprivileged.
- Registering with the Mount Manager needs Administrator. Without it you get a
  session drive, which works, but some tools mis-handle session drives — the
  mount logs a note if it falls back.
