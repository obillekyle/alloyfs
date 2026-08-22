# CLI

```
alloyfs <COMMAND>
```

| Command | What it does |
|---|---|
| `serve` | Publish the exports named in the config |
| `mount` | Mount an export as a drive |
| `start` | Run the whole config: the agent, plus every configured mount |
| `service` | Register mounts and agents to start on their own (Windows SCM / systemd user units) |
| `sync` | Keep a local directory in step with an export |
| `cache` | Manage the local cache (`cache clear` drops it) |
| `events` | Tail the change stream as NDJSON |
| `logs` | Read what a long-running command logged |
| `doctor` | Check the local things that stop a drive from working |
| `config` | Validate the config, or print the settled values |
| `ping` | Round-trip time to an agent |
| `bench` | Timed pipelined read, no mount involved |
| `stress` | Concurrent load generator |
| `tree` | Fetch an export's tree index in one exchange — the walk diagnostic |
| `bulk` | Compare bulk-read strategies against an export, with timings |
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

`--tcp` overrides `server.tcp_listen`; with neither, the default is
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

## start

```bash
alloyfs start [--config PATH] [--server-only] [--mounts-only]
```

Everything one config describes, in one command: the agent from `server:`, then
every mount under `client.mounts:`, running together until Ctrl-C. A v3 config
describes both halves of a machine, and starting them separately means two
terminals and a remembered order.

The agent is started when the config defines at least one export — a `server:`
holding only a listen address has nothing to serve, so nothing is run for it.

The agent starts first and the mounts **wait for it to accept connections**
before dialling (up to ten seconds). A config whose own `client.mounts` point
back at its own `server:` — a loopback mount of a local export — is an ordinary
thing to write, and without the wait the mount races the listener and loses. A
`0.0.0.0` listener is dialled on loopback, since a wildcard is what to bind, not
an address to connect to.

**One failure does not take the rest down.** An unreachable host must not stop
the agent, and a broken mount must not unmount the working ones. Whatever failed
is named at exit, rather than a single "start failed" about a five-mount config.

`--server-only` skips the mounts; `--mounts-only` skips the agent. Neither is
needed for a config that only has one half. A config with nothing in it prints
what to add and exits cleanly — a machine that has not been set up yet is not an
error.

## service

```bash
alloyfs service setup                 # one-time: check WinFsp, lock down the store
alloyfs service add <ID> [--config PATH] [--start]      # runs `alloyfs start`
alloyfs service add <ID> --mount <NAME>                 # one mount from client.mounts
alloyfs service add <ID> --mounts-only | --server-only  # one half of the config
alloyfs service list                  # what is defined, and the command it runs
alloyfs service start|stop|restart [ID]     # one, or every one
alloyfs service remove <ID>
alloyfs service reset --confirm       # remove every instance
```

Windows only, for now. Mounts and agents that come back at boot with no terminal
window. **Every subcommand except `list` needs an elevated shell**, and none of
them will raise their own privileges.

A service records **which part of the config to run**, not a copy of it. With no
flags it runs `alloyfs start`, so one service brings back the agent and every
drive; `--mount NAME` runs a single entry from `client.mounts:`, named the same
way `alloyfs mount <NAME>` names it. The url, mountpoint, excludes and cache
sizes stay in the config and are read at launch, by the logged-in user, whose
config it is.

See [Running as a service](#/deployment/service) for what a registered service
actually does, which process reads the config, why it needs Administrator, and
the Linux equivalent.

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
alloyfs update --check      # is there a newer release? exits 1 if so
alloyfs update --rollback   # put back the previous binary, no network
```

`--check` reports two numbers because they answer different questions and
often disagree: `stable` is the newest non-prerelease, and `newest` is the
newest thing published at all. On a machine running an alpha the stable one
is usually *older* — so the comparison is by version precedence, not string
equality, and it will not talk you into a downgrade. Exit status is 1 when an
update is available and 0 otherwise, so a cron job can act on the status
alone.

`--rollback` restores the copy `alloyfs update` keeps beside the binary
(`alloyfs.exe.prev` / `alloyfs.prev`). No network and no remembering which
tag you were on — which is the point, since the case for rolling back is
usually that the new release is what broke. The binary it displaces is kept
as `alloyfs.rolled-back`. Anything already running keeps its old image until
restarted: `alloyfs service restart <id>`.

Re-runs the installer from `alloy.okyle.dev` rather than replacing the binary
itself — one implementation of download-verify-install instead of two. On
Windows the installer renames the running executable aside before writing the
new one, which is the only way to replace a binary that is currently executing.

## config

```bash
alloyfs config validate            # does it parse, and what does it describe?
alloyfs config print               # the settled values
alloyfs config print --mount work  # just one mount
```

`validate` parses the file the same way every other command does, which is
the whole check: `deny_unknown_fields` makes a typo'd key a hard error rather
than a silently ignored line, so a config that loads is one whose every key
reached a struct.

`print` shows what the merge actually produced — client defaults with each
mount's overrides applied, where lists *replace* rather than union. Two
layers are deliberately absent and the output says so: CLI flags land on top
at mount time, and a v2+ server may suggest excludes, pins and cache sizes
underneath. Both need the mount to run, and this command reads a file.
Tokens print as `(set)`, never their value.

Note that no command creates a config as a side effect any more. `alloyfs
init` writes one; everything else reports that it found none.

## doctor

```bash
alloyfs doctor
```

Checks the local things that stop a drive from working, and says which one
is wrong instead of leaving you to guess which command happens to test it:
the filesystem driver (WinFsp / FUSE, with the install URL when it is
missing), whether the service supervisor is reachable, which config file is
actually in force and whether it parses, where the logs are and whether
there are any, and what services are registered with their current state. On
Linux it also warns when user services will not start until login (linger).

Local and fast, deliberately. Whether a server answers is a real question
and it belongs to `alloyfs ping <url>`, which reports a round-trip time —
probing the network here would make the command slow and hang on exactly the
machines that need it. Exit status is non-zero if any check fails; the table
goes to stdout and log lines to stderr, so `alloyfs doctor 2>/dev/null` pipes
cleanly.

## logs

```bash
alloyfs logs                # what has been logged, newest first
alloyfs logs webdav -f      # follow one, Ctrl-C to stop
alloyfs logs start -n 500   # more history than the default 200 lines
```

The commands that run for hours — `serve`, `mount`, `start`, `sync`, and any
service instance — tee their output to `~/.alloyfs/logs/<name>.log` as well as
to the terminal. That file is the only record a Windows service instance has:
it runs with no console, so its stderr goes nowhere.

A service instance logs under its own id, and its supervisor writes to the same
file — so a mount that keeps dying and the backoff messages about it are read
together. Files rotate at 8 MiB, keeping one previous generation
(`<name>.log.1`). On Linux, `journalctl --user -u alloyfs-<id>` has the same
output.

`RUST_LOG` sets the level for both destinations: `RUST_LOG=debug alloyfs start`.

## bench and ping

```bash
alloyfs ping tcp://127.0.0.1:7440
alloyfs bench ssh://host/projects big.bin --depth 16
```

`bench` measures the transport with no filesystem in the way, which is the
number a mount is being compared against. `--depth 1` shows what serial
round-trips cost.

Measure with `--auto-cache-max 0` on the mount, or you are timing your disk.
