# AlloyFS

**AlloyFS** is a cross-platform virtual drive service: any host runs an **agent** that exports
folders; any other host **mounts** an export as a *real local drive* — a drive
letter on Windows (via [WinFsp](https://winfsp.dev)), a mountpoint on Linux
(via FUSE). Not SMB, not WebDAV, not a sync folder.

What makes it different from sshfs/rclone mount:

- **Multi-client concurrency** — several machines can mount the same export at
  once, with advisory locking, sessions/leases, and write-conflict detection
  (last-writer-wins, but detected and logged).
- **Server-pushed file events** — the agent watches exported folders with the
  OS-native watcher (inotify / `ReadDirectoryChangesW`), coalesces changes,
  and streams them to every client. On Windows mounts they re-emerge as real
  `ReadDirectoryChangesW` events (editors refresh by themselves).
- **Event-invalidated caching** — clients cache metadata and data; the event
  stream keeps caches honest instead of polling.
- **SSH transport** — reuse your existing `ssh` config; the agent can be
  spawned on demand over an SSH exec channel, no daemon or open port needed.

## Status

Working today, verified on Windows 11 + Ubuntu 24.04:

- TCP and SSH-stdio transports (multiplexed binary protocol, pipelined)
- Read/write mounts: FUSE mountpoint (Linux), WinFsp drive letter (Windows),
  write-through semantics, full mutation set, `git` works inside the mount
- Live change events end-to-end: server-side watching (debounced/coalesced,
  sequenced), pushed to every client; **native `ReadDirectoryChangesW`
  re-emission on Windows** (editors auto-refresh), sub-second kernel cache
  invalidation on Linux; `alloyfs events` NDJSON tail anywhere
- Multi-client concurrency: whole-file advisory locks (fcntl forwarding on
  Linux), sessions with heartbeats, 30 s lease reaper for dead clients,
  write-conflict detection scaffolding
- HTTP API on the agent: status, browse, and an SSE event stream
- Server-side path hardening (canonicalize + escape check, watcher does not
  follow symlinks)

## Usage

```bash
# serve (on the machine with the files)
alloyfs serve --tcp 0.0.0.0:7440 --export projects=/home/you/projects

# mount over TCP (LAN)
alloyfs mount tcp://server:7440/projects /mnt/projects   # Linux
alloyfs mount tcp://server:7440/projects X:              # Windows

# mount over SSH — no daemon, no open port; reuses your ssh config.
# The remote side needs alloyfs on PATH and a config file (below).
alloyfs mount ssh://myhost/projects X:

# live change feed (NDJSON)
alloyfs events ssh://myhost/projects

# diagnostics
alloyfs ping ssh://myhost
alloyfs stress tcp://server:7440 --count 1000
```

Agent config (`~/.alloyfs/config.yml`, created on first run — picked up
automatically, which is what makes zero-argument `serve --stdio` over SSH
work):

```toml
[agent]
tcp_listen = "0.0.0.0:7440"     # omit to disable TCP
http_listen = "127.0.0.1:7441"  # optional HTTP/SSE API

[exports.projects]
path = "/home/you/projects"
read_only = false
```

### HTTP API

Enabled by `http_listen` in the agent config. With `http_token` set, every
route requires `Authorization: Bearer <token>` (constant-time check); serving
on a non-loopback address **without** a token is refused at startup.

- `GET  /api/status`, `GET /api/exports`
- `GET  /api/exports/{name}/browse?path=sub/dir`
- `GET  /api/exports/{name}/file?path=a/b.txt` — download (streamed)
- `POST /api/exports/{name}/file?path=a/b.txt` — create/overwrite with the
  request body (≤256 MiB; use a mount for bigger)
- `POST /api/exports/{name}/mkdir?path=newdir`
- `POST /api/exports/{name}/delete?path=a/b.txt` — file or empty dir
- `GET  /api/exports/{name}/events` — SSE; `Last-Event-ID` resumes from the
  server's ring log

Mutations respect `read_only` and server excludes (excluded paths are 404
over HTTP too) and bump file versions, so mounted clients pick changes up
through the normal event flow.

```bash
curl -H "Authorization: Bearer $TOKEN" \
  --data-binary @notes.txt \
  "http://server:7441/api/exports/projects/file?path=notes.txt"
```

## Excludes

**Server-side** (per export, in the agent config): matching paths exist on
the server but are never listed, never resolvable (plain "not found" — their
existence doesn't leak), and never produce events. Gitignore-flavored globs:
a bare name (`node_modules`, `.git`) matches at any depth and covers
everything beneath it; `secret*`, `build/out`, `**/*.log` work as expected.

**Client-side** (`--exclude GLOB` per mount, repeatable): matching paths live
**only on the mounting machine**, stored in the overlay under
`~/.alloyfs/data/<host>/overlay/<export>/` — which is why that tree is the
one thing here you cannot re-download.
The server never sees them; local watchers on the mount still fire natively;
they persist across remounts. Renames across the boundary return EXDEV, which
every tool answers with copy+delete — so `mv` in/out of an excluded directory
just works. The classic use: `--exclude node_modules` keeps installs fast and
local while the project itself stays on the server.

## Auto-download cache

Files at or below `--auto-cache-max` (default `2M`, `0` disables) are fully
downloaded in a background walk at mount time and then served from local
disk; `--pin GLOB` forces caching regardless of size. The event stream keeps
copies fresh (stale blobs are rejected by a size+mtime+version check and
re-fetched in the background). `--auto-cache-budget` (default `512M`) bounds
the cache with LRU eviction — pinned files are never evicted.
`alloyfs cache clear <url>` wipes a mount's blobs (never the overlay).

## Where things live

One tree in your home directory, the same on both platforms
(`%USERPROFILE%\.alloyfs` on Windows):

```text
~/.alloyfs/
  config.yml                created on first run if absent
  data/<host>/              DURABLE — nothing here can be re-downloaded
    overlay/<export>/         files that exist on NO server
    sync/<export>-<tag>.json  sync baselines
  cache/<host>/             DISPOSABLE — delete any of it, any time
    <export>/                 downloaded blobs
```

The `data` / `cache` split is deliberate: `alloyfs cache clear` operates
only on the second tree, so it cannot reach the overlay. Directories are
named after the host and export rather than hashed, so you can navigate
them. Host **and port** distinguish two agents on one machine, but the
scheme does not — mounting one export over `ssh://` and `tcp://` shares an
overlay, since it is the same server namespace either way.

**Overrides**, in order:

1. `--config PATH` / `--data-dir PATH` on the command line.
2. An `alloyfs.yml` **beside the executable** — a portable install (binary
   and config in one folder, on a stick or a share) runs without touching
   the home directory.
3. `~/.alloyfs/config.yml`, created from a commented template if missing.

Upgrading from an older layout needs no action: an existing config is
found in its old location, and each mount's overlay and cache move
themselves the first time that mount runs.

## Config files (YAML)

Agent (`serve --config agent.yml`; also the auto-discovered default at
`~/.alloyfs/config.yml`, see "Where things live" below):

```yaml
agent:
  tcp_listen: "0.0.0.0:7440"
  # Required for any non-loopback tcp_listen: clients present it at connect
  # (mount --token / token: in the mount config). ssh mounts never need it.
  tcp_token: "change-me"
  http_listen: "127.0.0.1:7441"
exports:
  projects:
    path: /home/you/projects
    exclude:
      - "**/.git"
      - "secret*"
    # Suggested settings for every machine that mounts this export
    # (applied at attach; see "Negotiated mount defaults" below).
    client:
      exclude:
        - node_modules
      pin:
        - "docs/**"
      auto_cache_max: 2M
      auto_cache_budget: 512M
```

Mount (`mount <url> <point> --config mount.yml`; CLI flags override):

```yaml
exclude:
  - node_modules
  - "*.tmp"
pin:
  - "docs/**"
auto_cache_max: 2M
auto_cache_budget: 512M
# no_server_defaults: true   # ignore the export's `client:` suggestions
```

## Negotiated mount defaults

An export can publish a `client:` section (above) — the settings the server
*recommends* every mounting machine use. Clients fetch it at attach and merge
it under their own configuration:

- **Precedence**: CLI flag > mount config file > server suggestion > built-in
  default (`2M`/`512M`). An explicit `--auto-cache-max 0` is a real "off"
  that beats the server's suggestion.
- **Lists union**: server-suggested `exclude`/`pin` globs are added to the
  client's own (duplicates dropped, client entries first).
- **Opting out**: `--no-server-defaults` (or `no_server_defaults: true` in
  the mount config) skips the exchange entirely.

The exchange is protocol v2; a v1 peer on either side simply never performs
it, and mixed-version pairs keep working with local settings only.

## TCP authentication (protocol v3)

Raw `tcp://` mounts historically trusted the network. Now:

- `agent.tcp_token` in the agent config makes every TCP session authenticate
  (constant-time compare) before any other request is served. Clients pass
  `--token` (or `token:` in the mount config); the reconnect dialer re-sends
  it automatically after a connection drop.
- **Serving TCP on a non-loopback address without a token is refused at
  startup** — anyone who could reach the port could mount every export.
  Loopback listeners may stay tokenless by choice.
- `ssh://` mounts are untouched: reaching the stdio agent already required an
  ssh login, which is stronger auth than any shared secret.
- Token-protected listeners require protocol v3+ clients (older ones are
  turned away at the handshake with a version error they can decode).

## Wire compression (protocol v3)

Frames of 512+ bytes are transparently lz4-compressed whenever both ends
speak v3 — but only when compression actually shrinks them, so
already-compressed file data (archives, images, video) passes through
untouched. Source trees typically halve their wire volume or better; on slow
links (the ~2 MB/s ssh path this project grew up on) that translates almost
directly into throughput. No configuration, no flags: it's negotiated per
connection and silently off with any v2 peer.

## Write conflicts (`--detect-conflicts`)

By default a mount is last-writer-wins: if two machines have the same file
open and both save, the later save wins and the earlier one is gone. That is
what every network filesystem does, and it is almost always what you want —
the alternative is an editor that sometimes refuses to save.

```bash
alloyfs mount ssh://host/projects /mnt/p --detect-conflicts
```

With the flag on, each write carries the file version the handle last saw, and
the server **refuses** the write if the file has changed since. The application
gets `EIO` and the file on the server is untouched. It is a refusal, not a
report: being told your colleague's edit was overwritten, after it was
overwritten, is not a safeguard.

Worth knowing before turning it on:

- **Off is the default and stays the default.** A mount without the flag sends
  no version and behaves exactly as before.
- **Whole-file granularity.** The version bumps on any write, so two people
  editing opposite ends of one file still conflict.
- **A refused large write may be partial.** Writes are chunked, so if the
  conflict is detected on a later chunk, earlier chunks have already landed.
  The log names the offset.
- **Sync mode ignores it.** `alloyfs sync` pre-checks conflicts itself and
  keeps the loser as `.sync-conflict-<ts>`, which is a better answer when
  there is a whole directory to reconcile.

Set it per mount in a config file with `detect_conflicts: true`.

## Sync mode: a real directory instead of a mount

```bash
alloyfs sync ssh://azure/projects ~/projects
```

Bidirectional sync between a server export and a **real local directory**.
Remote changes are applied as genuine filesystem operations, which means
watchers on that directory receive genuine, full-fidelity inotify /
ReadDirectoryChangesW events — create, modify, delete, rename, all of it.
This is the answer to the one thing no mounted filesystem can do on Linux
(the kernel only generates inotify from local syscalls): vite, chokidar,
VS Code, `inotifywait` scripts all just work. Local edits are watched,
debounced, and pushed back; the server's origin filtering means your own
pushes never echo back at you.

- **Conflicts**: last-writer-wins by mtime (`--conflict-policy newer`,
  or force `remote`/`local`). The losing copy is **always** preserved
  beside the winner as `<name>.sync-conflict-<timestamp>` — and syncs to
  both sides like any other file. Deletes lose to edits, always.
- **State**: a per-(server, export, directory) baseline manifest under the
  data dir enables true three-way reconciliation at startup and after
  outages; a clean start transfers nothing. Reconnects resume from the last
  applied event; expired event history or a restarted server triggers a
  full (cheap) reconcile.
- **Excludes**: `--exclude GLOB` holds in both directions.
- **`--one-shot`**: reconcile once and exit — a protocol-native `rsync`.
- Limitations: symlinks aren't synced (no wire representation); like any
  sync engine, the tree lives on your disk in full — use the mount when
  you want on-demand access to more than fits locally.

When to pick which: **mount** for drive semantics and on-demand access to
big trees; **sync** for dev working copies on Linux where file events and
native-speed builds matter.

## Installing (Linux)

```bash
cargo build --release                 # as yourself — never under sudo
sudo packaging/install.sh --user you --start
sudo packaging/uninstall.sh           # reverses all of it
```

That installs the binary and the systemd unit and starts the agent. Add
`--with-module` to also install the optional kernel module (below). Both
scripts are idempotent — re-running `install.sh` is how you upgrade — and
both refuse with an explanation rather than a stack trace when run without
root. `install.sh` deliberately never invokes cargo: doing that under sudo
leaves a root-owned `target/` that breaks every later developer build.

| Path | What | Removed by `uninstall.sh` |
| --- | --- | --- |
| `/usr/local/bin/alloyfs` | the one binary (agent, client, CLI) | yes |
| `/etc/systemd/system/alloyfs@.service` | template unit; instances are disabled and stopped first | yes |
| `/usr/src/alloyfs-<version>/` | module sources for DKMS | yes |
| `/lib/modules/<kernel>/updates/dkms/alloyfs.ko*` | the built module | yes |
| `/etc/udev/rules.d/60-alloyfs.rules` | ownership of `/dev/alloyfs` | yes |
| `/etc/modules-load.d/alloyfs.conf` | loads the module at boot | yes |
| the `alloyfs` group | who may open `/dev/alloyfs` | yes (`--keep-group` to keep it) |
| `~/.alloyfs/` | config, `data/`, `cache/` | **no, never** |

That last row is the one deliberate asymmetry. `data/` holds the overlay —
files that exist on no server and cannot be re-downloaded — so uninstalling
the software is not allowed to be how you lose them. Delete it yourself if
you actually mean to. Useful flags: `--prefix DIR`, `--binary PATH`,
`--no-service`, `--no-autoload`.

`install.sh --user NAME` enables `alloyfs@NAME`; `--start` starts it too.

### The kernel module is optional

Everything above works without it. **FUSE is the default and fully supported
mount backend**; the kernel module exists to close exactly one gap — Linux
inotify does not fire for remote changes on a FUSE mount (see "Honest
limitations") — and it is Linux-only. If you don't need real inotify inside
a mount, skip `--with-module` entirely and nothing else changes.

The trade runs both ways, so pick per workload rather than assuming newer is
better. `--backend kernel` serves 13 operations; FUSE serves 21. What you gain
is that a remote change arrives as a real inotify event instead of a cache
invalidation. What you give up is `statfs`, `link`, and `fcntl(F_GETLK)` —
which returns `ENOLCK` rather than answering, because the wire protocol has no
way to ask *who* holds a lock and a confident wrong answer is worse than none.
Callers that get `ENOLCK` fall back to attempting the lock, which is checked
properly.

Locks themselves are forwarded on both backends: `flock()` and
`fcntl(F_SETLK/F_SETLKW)` reach the agent and exclude other machines. They are
whole-file on the wire, so a byte range is coarsened to the whole file — safe
(over-locking) rather than unsafe. Stage 8 of the kernel test suite mounts one
export twice and asserts the two mounts exclude each other.

With `--with-module`, the module is installed through
[DKMS](packaging/dkms.conf) rather than as a bare `.ko`. An out-of-tree
module is only valid for the exact kernel it was compiled against, and
distributions ship new kernels every few weeks; without DKMS the module
silently stops loading after the next upgrade and reboot, and the only
symptom is that `/dev/alloyfs` has quietly vanished. DKMS keeps the *source*
in `/usr/src` and rebuilds it from a kernel-upgrade hook, so the module
follows the kernel instead of rotting against it. The DKMS package version
is the workspace version, so `dkms status` and `alloyfs --version` always
agree.

### Who may open /dev/alloyfs

The module registers its device as `0600 root:root` — the right default,
since that is what devtmpfs creates the node with before udev runs, so it is
never briefly more open than intended. But the agent runs unprivileged (the
unit runs it as `User=%i`), so as shipped it cannot open the device at all.
[`packaging/60-alloyfs.rules`](packaging/60-alloyfs.rules) closes that gap by
handing the device to a dedicated `alloyfs` group at mode `0660`.

It is a group and not `0666` because opening this device makes the opener a
filesystem **server**: it answers every lookup, read and readdir for the
alloyfs mounts bound to it, and can inject fsnotify events into them. That is
enough to feed attacker-chosen bytes to a process that believes it is reading
its own disk, and to change a file's contents between two reads of it. So
membership in `alloyfs` *is* the permission model — adding a user to that
group is the deliberate statement that the account may serve filesystems on
this machine. `install.sh --with-module --user NAME` adds that one user;
everyone else stays out. (Mounting remains separately privileged, so an
unprivileged server with nothing pointed at it can affect nobody.)

### Secure Boot

On a Secure Boot machine with module signature enforcement (Ubuntu's default
— check `cat /sys/module/module/parameters/sig_enforce`), DKMS signs the
module with a machine-owner key that has to be **enrolled by a human at the
console**, which no installer can do on your behalf; that is the entire point
of Secure Boot. `install.sh` detects this, finishes the installation, and
tells you the one-time step:

```bash
sudo mokutil --import /var/lib/shim-signed/mok/MOK.der
# then reboot and choose "Enroll MOK" in the blue MOK Manager screen
```

Until that is done `modprobe alloyfs` fails with `Key was rejected by
service` and `/dev/alloyfs` does not exist. FUSE mounts are unaffected, and
every future DKMS rebuild is covered once the key is enrolled.

## Running the agent as a service

- **Linux (systemd)**: `sudo packaging/install.sh --user youruser --start`
  installs [scripts/alloyfs.service](scripts/alloyfs.service) as the template
  unit `alloyfs@.service` and enables the instance for you (the instance name
  picks the user whose config and exports it serves). By hand it is just a
  copy to `/etc/systemd/system/alloyfs@.service` plus
  `systemctl enable --now alloyfs@youruser`. Logs land in journald
  (`journalctl -u alloyfs@youruser -f`); the agent restarts on failure.
- **Windows (Scheduled Task)**: [scripts/install-agent-task.ps1](scripts/install-agent-task.ps1)
  (elevated PowerShell; `-Exe` overrides the binary path) registers a
  `alloyfs-agent` task that starts `alloyfs serve` at logon and
  restarts it on failure.
- Mounts don't need service treatment: a mount already **auto-reconnects**
  through server restarts, so an agent coming back after a reboot picks up
  its clients where they left off (open files reopen; uncontended locks are
  re-acquired — see below).

## Known issues

- **bun on Windows needs an elevated mount.** Solved mystery: bun (and the
  identical failure on rclone drives) requires the volume to be registered
  with the Windows Mount Manager — bun canonicalizes paths with
  `GetFinalPathNameByHandle`, which doesn't round-trip on session-local DOS
  drives, producing its famous unhelpful ENOENT. alloyfs now registers
  with the Mount Manager automatically **when run as Administrator** (and
  falls back to a session drive with a logged warning otherwise). bun also
  needs POSIX-semantics renames for its lockfile, which the volume now
  declares. With an elevated mount, `bun install` works with its default
  backend. (This diagnosis applies to every WinFsp filesystem, rclone
  included.)
- **Do not build with LTO.** `lto = "thin"` miscompiles the WinFsp path
  (reads through the drive hang); the release profile pins `lto = false`.
- Sequential reads use a per-handle readahead window (32 × 128 KiB in
  flight once a sequential-ish pattern is detected; out-of-order and
  overlapped kernel reads are tolerated, and recently served blocks are
  retained for sub-chunk re-reads). Measured on a ~2 MB/s, 60 ms ssh link
  with compressible data: raw pipelined transport 14.6 MB/s, 1 MiB kernel
  reads through the mount 6.4 MB/s (2× the previous ceiling), 128 KiB
  kernel reads 4.0 MB/s — small-read throughput is bounded by per-request
  dispatch cost, not the wire. `alloyfs bench <url> <path> --depth N`
  measures the transport without the kernel in the loop; mount with
  `ALLOYFS_READ_STATS=1` to get per-file window counters on release.
- Mounts **auto-reconnect**: on connection loss the client re-dials (tcp or
  a fresh ssh spawn) with backoff, re-attaches, re-opens every live file
  handle on the new session (open fds keep working — verified across a full
  server kill), and resubscribes the event stream from the last applied
  sequence number. Advisory locks are **replayed** on the new session: an
  uncontended lock survives; if another client won the lock during the gap,
  the handle is poisoned and further I/O on it fails with EIO — a loud
  signal that mutual exclusion was interrupted, which is the one thing an
  application can actually react to. Requests are bounded by a 30 s timeout
  (blocking lock waits instead by peer liveness — a busy server can hold a
  waiter indefinitely, a wedged one fails it in ~20 s) so a dead or stuck
  server can never hang the mount forever.
- **A killed (not unmounted) Windows mount can leave its drive letter
  registered** — remounting on the same letter then fails with "Object Name
  already exists". Unmount with Ctrl-C when possible; after a hard kill,
  pick a fresh letter or restart the WinFsp service
  (`net stop winfsp.launcher & net start winfsp.launcher`) / reboot to
  reclaim the letter.

## Honest limitations (by design or by platform)

- **Linux inotify on a FUSE mount does not fire for remote changes.** The
  kernel only generates inotify from local VFS activity, and FUSE has no
  passthrough (the 2021 RFC was never merged). On the default FUSE backend
  alloyfs keeps *reads* fresh via kernel cache invalidation, and offers a
  userspace event stream (local socket + SSE) for tools that need change
  notifications. Windows mounts get native events. Polling watchers (VS Code's
  fallback, `git status`) work fine everywhere. **Two ways out on Linux, both
  shipped:** `--backend kernel` (the module injects genuine fsnotify, so a
  remote change is indistinguishable from a local one) and `alloyfs sync`,
  which gives you an ordinary directory that every watcher already understands.
- **Whole-file advisory locks only.** Byte-range locks are coarsened to the
  whole file on every backend, and `fcntl(F_GETLK)` returns `ENOLCK` on
  `--backend kernel` because the protocol cannot ask who a holder is. Taking
  locks works and excludes other machines on both backends. Don't host live
  database files (SQLite/Postgres) on a shared mount regardless: they want
  byte ranges and `F_GETLK`.
- **Symlinks can be created and read** on FUSE and `--backend kernel`; a
  target that would land outside the export is refused at creation, including
  a dangling one, and so is a target inside an excluded path. Targets are
  stored verbatim — relative links stay relative. On Windows, see the WinFsp
  note below.
- **Hard links share content but not cache coherence.** Two names for one
  inode are a real hard link on the server, so a write through one changes the
  file both see. But the client keys its caches by path and cannot know the
  two names are the same file, so a read through the *other* name may serve
  bytes cached before the write until that handle's window turns over. Fixing
  it needs a stable inode identity on the wire, which the protocol does not
  carry; NFS has the same hole. Don't rely on cross-name coherence within a
  single mount session.
- **Windows volumes are case-sensitive** to faithfully mirror Linux exports.
- **Throughput ≈ scp class** over SSH; metadata-heavy workloads are RTT-bound
  (mitigated by attr-priming readdir, pipelined 128 KiB chunks, caching).
- **Network partition**: write-through means acknowledged writes are never
  lost; in-flight operations fail with EIO; server-side leases free a dead
  client's locks after ~30 s.

## License

MIT for this project's code (see `LICENSE`). `vendor/winfsp-sys/` is a
vendored upstream crate carrying a small build patch for the portable
llvm-mingw toolchain; it keeps its upstream licenses (winfsp-rs MIT, WinFsp
GPLv3-with-FLOSS-exception).

## Development

`bash scripts/verify.sh` is the local gate: fmt check, clippy (deny
warnings), the full test suite (unit + the in-process loopback integration
battery + the frozen wire-format goldens), and a release build. CI
(`.github/workflows/ci.yml`) runs the same on ubuntu + windows the moment
this repo ever lands on GitHub. If you change any protocol type, the golden
test tells you exactly what to do.

## Building

Rust workspace; `cargo build` at the root. Platform bridges are isolated:
`alloyfs-mount-fuse` only compiles on Unix, `alloyfs-mount-winfsp` only on Windows.

- **Linux**: `apt install build-essential pkg-config fuse3 libfuse3-dev`, then rustup.
  For the optional kernel module, additionally
  `apt install dkms linux-headers-$(uname -r)`; `packaging/install.sh
  --with-module` checks for both and says which one is missing.
- **Windows**: WinFsp 2.1+ (with SDK feature) is the one required install.
  The reference build uses the fully-portable llvm-mingw toolchain targeting
  `x86_64-pc-windows-gnullvm` (no Visual Studio needed); see
  `vendor/winfsp-sys/` for the delay-load patch that makes this possible.

## Architecture (one paragraph)

One binary. `alloyfs-proto` defines a length-prefixed postcard frame protocol
(requests/responses multiplexed by correlation id, plus server-push event
frames) spoken over any byte stream — TCP today, SSH stdio next. The agent
(`alloyfs-agent`) canonicalizes every path against the export root (escape-proof),
tracks per-file versions, and will fan out watcher events to subscribed
sessions. The client (`alloyfs-client`) presents a synchronous `RemoteFs` facade
(inode↔path table, TTL attr cache) that platform backends adapt to their
callback dialect: `alloyfs-mount-fuse` (fuser 0.17) and `alloyfs-mount-winfsp`
(winfsp-rs 0.13). A future ProjFS backend slots in behind the same seam.
