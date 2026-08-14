# drive-sync

Cross-platform virtual drive service: any host runs an **agent** that exports
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
  invalidation on Linux; `drive-sync events` NDJSON tail anywhere
- Multi-client concurrency: whole-file advisory locks (fcntl forwarding on
  Linux), sessions with heartbeats, 30 s lease reaper for dead clients,
  write-conflict detection scaffolding
- HTTP API on the agent: status, browse, and an SSE event stream
- Server-side path hardening (canonicalize + escape check, watcher does not
  follow symlinks)

## Usage

```bash
# serve (on the machine with the files)
drive-sync serve --tcp 0.0.0.0:7440 --export projects=/home/you/projects

# mount over TCP (LAN)
drive-sync mount tcp://server:7440/projects /mnt/projects   # Linux
drive-sync mount tcp://server:7440/projects X:              # Windows

# mount over SSH — no daemon, no open port; reuses your ssh config.
# The remote side needs drive-sync on PATH and a config file (below).
drive-sync mount ssh://myhost/projects X:

# live change feed (NDJSON)
drive-sync events ssh://myhost/projects

# diagnostics
drive-sync ping ssh://myhost
drive-sync stress tcp://server:7440 --count 1000
```

Agent config (`~/.config/drive-sync/agent.toml` on Linux,
`C:\MyApps\drive-sync.toml` on Windows — picked up automatically, which is
what makes zero-argument `serve --stdio` over SSH work):

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
**only on the mounting machine**, stored under the local data dir
(`%LOCALAPPDATA%\drive-sync\overlay\…` / `~/.local/share/drive-sync/…`).
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
`drive-sync cache clear <url>` wipes a mount's blobs (never the overlay).

## Config files (YAML)

Agent (`serve --config agent.yml`; also the auto-discovered default —
`~/.config/drive-sync/agent.yml` on Linux, `C:\MyApps\drive-sync.yml` on
Windows; `.toml` variants still parse for existing deployments):

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

## Running the agent as a service

- **Linux (systemd)**: [scripts/drive-sync.service](scripts/drive-sync.service)
  is a template unit — copy to `/etc/systemd/system/drive-sync@.service`,
  then `systemctl enable --now drive-sync@youruser` (the instance name picks
  the user whose config and exports it serves). Logs land in journald
  (`journalctl -u drive-sync@youruser -f`); the agent restarts on failure.
- **Windows (Scheduled Task)**: [scripts/install-agent-task.ps1](scripts/install-agent-task.ps1)
  (elevated PowerShell; `-Exe` overrides the binary path) registers a
  `drive-sync-agent` task that starts `drive-sync serve` at logon and
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
  drives, producing its famous unhelpful ENOENT. drive-sync now registers
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
  dispatch cost, not the wire. `drive-sync bench <url> <path> --depth N`
  measures the transport without the kernel in the loop; mount with
  `DS_READ_STATS=1` to get per-file window counters on release.
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

- **Linux inotify on the mount does not fire for remote changes.** The kernel
  only generates inotify from local VFS activity, and FUSE has no passthrough
  (the 2021 RFC was never merged). drive-sync keeps *reads* fresh via kernel
  cache invalidation, and offers a userspace event stream (local socket + SSE)
  for tools that need change notifications on Linux. Windows mounts do get
  native events. Polling watchers (VS Code's fallback, `git status`) work fine
  everywhere.
- **Whole-file advisory locks only.** Byte-range locks are coarsened. Don't
  host live database files (SQLite/Postgres) on a shared mount.
- **Symlinks are resolved server-side** within the export (escaping links are
  refused); symlinks cannot be created through the mount.
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
`ds-mount-fuse` only compiles on Unix, `ds-mount-winfsp` only on Windows.

- **Linux**: `apt install build-essential pkg-config fuse3 libfuse3-dev`, then rustup.
- **Windows**: WinFsp 2.1+ (with SDK feature) is the one required install.
  The reference build uses the fully-portable llvm-mingw toolchain targeting
  `x86_64-pc-windows-gnullvm` (no Visual Studio needed); see
  `vendor/winfsp-sys/` for the delay-load patch that makes this possible.

## Architecture (one paragraph)

One binary. `ds-proto` defines a length-prefixed postcard frame protocol
(requests/responses multiplexed by correlation id, plus server-push event
frames) spoken over any byte stream — TCP today, SSH stdio next. The agent
(`ds-agent`) canonicalizes every path against the export root (escape-proof),
tracks per-file versions, and will fan out watcher events to subscribed
sessions. The client (`ds-client`) presents a synchronous `RemoteFs` facade
(inode↔path table, TTL attr cache) that platform backends adapt to their
callback dialect: `ds-mount-fuse` (fuser 0.17) and `ds-mount-winfsp`
(winfsp-rs 0.13). A future ProjFS backend slots in behind the same seam.
