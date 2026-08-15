# CLI

```
alloyfs <COMMAND>
```

| Command | What it does |
|---|---|
| `serve` | Publish the exports named in the config |
| `mount` | Mount an export as a drive |
| `sync` | Keep a local directory in step with an export |
| `cache` | Show what is cached |
| `clear` | Drop the cache |
| `events` | Tail the change stream as NDJSON |
| `ping` | Round-trip time to an agent |
| `bench` | Timed pipelined read, no mount involved |
| `stress` | Concurrent load generator |
| `init` | Write a config for a directory, ready to serve |
| `update` | Update in place by re-running the installer |

## serve

```bash
alloyfs serve [--tcp ADDR] [--stdio] [--config PATH] [--export NAME=PATH]
```

`--stdio` serves one session on stdin/stdout — how `ssh://` mounts work. It does
not start the HTTP API; that belongs to the long-running agent, not to a
per-mount transport.

`--export NAME=PATH` publishes a folder without a config file, for a quick test.

`--tcp` overrides `agent.tcp_listen`; with neither, the default is
`127.0.0.1:7440`. A non-loopback address requires a token — see
[TCP authentication](#/configuration/auth).

## mount

```bash
alloyfs mount <URL> <MOUNTPOINT> [options]
```

See [Mounting a drive](#/guides/mounting) for the full option list.

## sync

```bash
alloyfs sync <URL> <DIR> [--policy newer|local|remote] [--once]
```

See [Sync mode](#/guides/sync-mode).

## events

```bash
alloyfs events <URL> [--since SEQ]
```

One JSON object per line. `--since` replays from a sequence number.

## init

```bash
alloyfs init                 # ./alloyfs.yml exporting the current directory
alloyfs init /srv/data       # export somewhere else
alloyfs init --name docs     # choose the export name
alloyfs init --global        # write ~/.alloyfs/config.yml instead
alloyfs init --force         # overwrite an existing file
```

The export name is derived from the directory name, lowercased and reduced to
`[a-z0-9-_]` — names end up in URLs and in on-disk paths, so anything that
could change how either parses becomes a dash.

A config in the current directory is **not** picked up automatically. Pass
`--config`, or move it next to the binary, or use `--global`. Auto-loading from
the working directory would mean running `alloyfs` in a directory someone else
controls could serve whatever a config there said to.

## update

```bash
alloyfs update              # the latest release
alloyfs update v0.1.1       # pin, or roll back
alloyfs update --dry-run    # print the command, run nothing
```

Re-runs the installer from `alloy.okyle.dev` rather than replacing the binary
itself — one implementation of download-verify-install instead of two. On
Windows the installer renames the running executable aside before writing the
new one, which is the only way to replace a binary that is currently executing.

## bench and ping

```bash
alloyfs ping tcp://127.0.0.1:7440
alloyfs bench ssh://host/projects big.bin --depth 16
```

`bench` measures the transport with no filesystem in the way, which is the
number a mount is being compared against. `--depth 1` shows what serial
round-trips cost.

Measure with `--auto-cache-max 0` on the mount, or you are timing your disk.
