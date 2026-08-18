# Mounting a drive

```bash
alloyfs mount <URL> <MOUNTPOINT> [options]
alloyfs mount <NAME> [options]
```

The URL is `tcp://host:port/export` or `ssh://host/export`. The mountpoint is a
directory on Linux, a drive letter (`X:`) on Windows. A NAME is a mount the
config already describes, covered below.

## TCP or SSH

**SSH** needs no listener and no open port. AlloyFS runs `alloyfs serve --stdio`
on the far side through your existing SSH config — same hosts, same keys, same
jump hosts. The remote needs `alloyfs` on `PATH` and a config naming the export.

**TCP** is lower overhead on a LAN. A non-loopback listener **requires** a token;
the agent refuses to start otherwise, because anyone who can reach the port
could otherwise mount every export. See [TCP authentication](#/configuration/auth).

## Windows: mount from an elevated shell

Open the terminal with **Run as administrator** before mounting, or register the
mount with [`alloyfs service`](#/deployment/service), which arranges the same
thing at logon. An unelevated mount still works for most tools, but it breaks
one class of them in a way that is almost impossible to diagnose from the error.

A drive letter can be created two ways, and only one of them is a real volume:

- **Session-local DOS device** (`X:`) — a symbolic link in the session's own
  object directory, visible only to the session that created it. Needs no
  privileges, which is why it is what WinFsp filesystems fall back to.
- **Mount Manager registration** (`\\.\X:`) — a globally registered volume mount
  point. **Requires Administrator.**

AlloyFS asks for the second and falls back to the first with a warning in the
log.

The fallback is fine for `git`, editors, Explorer and most of everything else.
What it breaks is path canonicalisation: `GetFinalPathNameByHandle` maps a
volume device back to a drive letter *through* the Mount Manager's registration,
and a session-local drive has no registration to find, so the lookup does not
round-trip.

**bun turns that into `error: ENOENT: Bun could not find a file`** — no path, no
syscall, no hint that drive letters are involved — for files that plainly exist.
`bun install` inside an unelevated mount fails every time. The same failure
happens on rclone mounts, sshfs-win and WinFsp's own `memfs` sample, so it is a
property of the drive rather than of AlloyFS; the trace and the upstream report
are in [bun and WinFsp drives](#/upstream/bun-winfsp-enoent).

So: **a spurious `ENOENT` on a mounted drive means the mount was not elevated.**
Nothing about the file is wrong.

## Mounting by name

Mounts listed in a config have names, and one positional mounts one of them:

```yaml
client:
  exclude: [node_modules]
  mounts:
    work: { url: ssh://azure/projects, at: "P:" }
```

```bash
alloyfs mount work
```

The entry supplies the url, the mountpoint and its own settings, including
whatever it inherits from the `client:` defaults above it. Flags still win:
`alloyfs mount work --exclude target` mounts that same `work` excluding
`target` rather than `node_modules`.

Which form runs is decided by how many positionals there are, not by how the
first one looks — so a name is never dialled as a host, and a name that is not
in the file says so and lists the ones that are.

`--config <PATH>` chooses which file the name is looked up in; without it, the
usual [search order](#/configuration/config-file) applies.

`alloyfs start` mounts **every** entry under `client.mounts:` at once, alongside
the agent this machine's `server:` section describes, and runs until Ctrl-C. One
mount failing does not disturb the others — see the
[CLI reference](#/reference/cli).

## Options worth knowing

| Flag | What it does |
|---|---|
| `--exclude <GLOB>` | Keep matching paths local; never send them. See [Excludes](#/guides/excludes) |
| `--pin <GLOB>` | Always fully cache matching files, whatever their size |
| `--auto-cache-max <SIZE>` | Auto-download files up to this size (`0` = off) |
| `--auto-cache-budget <SIZE>` | Total cache budget; LRU eviction, pins exempt |
| `--detect-conflicts` | Refuse writes over concurrent changes. See [Write conflicts](#/guides/conflicts) |
| `--backend fuse\|kernel` | Linux only. See [Choosing a backend](#/backends/choosing) |
| `--no-server-defaults` | Ignore the export's suggested client settings |
| `--data-dir <PATH>` | Override where the overlay and cache live |
| `--token <TOKEN>` | Shared secret for a token-protected TCP agent |

Every flag also has a config-file equivalent, so a mount's settings can live
next to it rather than in shell history:

```yaml
# ~/projects-mount.yml
client:
  exclude: [node_modules]
  pin: ["*.lock"]
  auto_cache_max: 2M
```

```bash
alloyfs mount ssh://host/projects /mnt/p --config ~/projects-mount.yml
```

A `--config` passed to `mount` supplies the `client:` defaults, which is exactly
what asking "how should this mount behave" means. CLI flags always win over file
values. Older flat mount configs — `exclude:`, `pin:`, `auto_cache_max:` at the
top level, with no `client:` above them — still load, and are
[upgraded in place](#/configuration/config-file) the first time they are read.

## Reconnection

Mounts survive connection loss. The client re-dials, re-attaches, reopens its
handles and resubscribes to events from the last sequence it saw.

Advisory locks are replayed too — and if a lock cannot be restored, that handle
is **poisoned**: reads, writes and flushes on it fail with `EIO` rather than
carrying on as though mutual exclusion still held.

## Unmounting

Ctrl-C. On Linux, `fusermount3 -u <mountpoint>` also works; with
`--backend kernel`, Ctrl-C is the clean path because a killed daemon leaves a
mounted filesystem whose operations all fail until someone unmounts it.
