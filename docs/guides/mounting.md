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

Every flag also has a config-file equivalent, so you can keep a mount's settings
next to it rather than in shell history:

```bash
alloyfs mount ssh://host/projects /mnt/p --config ~/projects-mount.yml
```

CLI flags always win over file values.

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
