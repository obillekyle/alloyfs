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
alloyfs mount <NAME> [options]
```

Two positionals mount a url at a mountpoint. One names a mount under
`client.mounts:` in the config, which supplies the url, the mountpoint and that
entry's settings; flags still override them. Which form runs is decided by how
many positionals there are, never by how the first one looks.

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

`alloyfs serve` picks up `./alloyfs.yml` from the directory it is run in, so
`alloyfs init && alloyfs serve` works with no `--config`. The full search order
is in [config.yml](#/configuration/config-file).

That does mean running `alloyfs serve` inside a directory someone else controls
will serve whatever a config there says to. Treat an `alloyfs.yml` you did not
write the way you would treat a `Makefile` you did not write.

`init` writes a config; it does not merge into one. Pointing it at a path that
already exists fails unless `--force` is passed, and `--force` overwrites rather
than appends — so adding a second export to an existing config is a text edit,
not a command.

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
