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

## serve

```bash
alloyfs serve [--tcp ADDR] [--stdio] [--config PATH] [--export NAME=PATH]
```

`--stdio` serves one session on stdin/stdout — how `ssh://` mounts work. It does
not start the HTTP API; that belongs to the long-running agent, not to a
per-mount transport.

`--export NAME=PATH` publishes a folder without a config file, for a quick test.

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

## bench and ping

```bash
alloyfs ping tcp://127.0.0.1:7440
alloyfs bench ssh://host/projects big.bin --depth 16
```

`bench` measures the transport with no filesystem in the way, which is the
number a mount is being compared against. `--depth 1` shows what serial
round-trips cost.

Measure with `--auto-cache-max 0` on the mount, or you are timing your disk.
