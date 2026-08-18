# config.yml

One file describes both halves of a machine: what it **serves**, and what it
**mounts**.

```yaml
version: 3

server:
  tcp_listen: "127.0.0.1:7440"
  exports:
    projects:
      path: /home/you/projects

client:
  exclude: [node_modules]
  mounts:
    work: { url: ssh://azure/projects, at: "P:" }
```

`alloyfs serve` reads the `server:` half, `alloyfs mount work` reads one entry
of the `client:` half, and [`alloyfs start`](#/reference/cli) runs the whole
file at once.

## Nothing in it is required

Not `version`, not either section, not anything inside them.

- A machine that only mounts writes **no `server:` at all**.
- A machine whose exports are temporarily commented out leaves `server:` with
  nothing under it, which YAML reads as `null`. That loads too — commenting a
  block out is how people disable things, and a parser that punishes it is a
  parser people fight.
- The same holds one level down: `server.exports` and `client.mounts` may be
  absent, null, or `{}`. All three mean "nothing here".

`version:` is a forward-compatibility lever rather than the mechanism. The
three config layouts that have existed use completely disjoint top-level keys,
so a file identifies itself by its own contents; an existing config keeps
working with nothing added to it. A `version:` that this build does not
understand fails loudly instead of being half-read.

## Getting one

```bash
alloyfs init            # ./alloyfs.yml, exporting the current directory
alloyfs init --global   # ~/.alloyfs/config.yml instead
```

[`alloyfs init`](#/reference/cli) fills in the export rather than leaving you to
write one. See [Your first mount](#/getting-started/first-mount).

## Where it is found

Every command that reads a config looks in the same order:

1. **`./alloyfs.yml`**, `./alloyfs.yaml`, `./alloyfs.json` — the directory you
   are standing in. This is what `alloyfs init` writes, and the same convention
   cargo, npm and docker compose use: a tool run inside a project reads that
   project's config.
2. **The same names beside the executable** — a portable install, where the
   binary and its config travel together.
3. **`~/.alloyfs/config.{yml,yaml,json}`** (`%USERPROFILE%\.alloyfs\` on
   Windows) — the per-user default.

If nothing exists anywhere, a commented v3 template is written to the per-user
location, so a first run leaves you a file to edit rather than an error telling
you to invent one.

`--config <PATH>` skips the search entirely.

**The agent logs which config it loaded**, every start. "Which config is this
agent actually serving" should never need guessing — especially now that the
answer depends on where you ran it from.

JSON is accepted because the YAML parser reads it: YAML 1.2 is a superset of
JSON, so a `.json` config is just a config, with no second parser and no second
set of quirks to learn.

**Mounting needs no config at all.** `alloyfs mount <url> <mountpoint>` takes
everything from flags. A first mount creates `~/.alloyfs/` for the
[overlay and cache](#/configuration/where-things-live), and nothing else.

## `server:` — what this machine offers

```yaml
server:
  # A non-loopback address REQUIRES tcp_token; the agent refuses otherwise.
  tcp_listen: "127.0.0.1:7440"
  # tcp_token: "change-me"
  # http_listen: "127.0.0.1:7441"
  # http_token: "change-me"

  exports:
    projects:
      path: /home/you/projects
      read_only: false
      exclude:
        - "**/.git"
        - "*.key"
      # Settings suggested to anyone who mounts this export
      client:
        exclude: [node_modules, target]
        pin: ["*.lock"]
        auto_cache_max: 2M
        auto_cache_budget: 512M
```

**Server-side `exclude` is enforcement**: matching paths are invisible to every
client and report "not found" rather than "forbidden", so their existence does
not leak. **`exports.<name>.client` is a suggestion**, merged into what the
mounting machine already wanted. It is not the same thing as the top-level
`client:` section, despite the shared word — the two are compared below.

### OS bookkeeping is hidden by default

`System Volume Information`, `$RECYCLE.BIN`, `Thumbs.db`, `desktop.ini`,
`.DS_Store`, `.Spotlight-V100`, `.Trashes`, `lost+found`, `.Trash-*` and
friends are excluded in both directions without being listed.

This is not tidiness. Mounting an export on Windows makes the *mounting*
machine's volume service create `System Volume Information` inside the served
folder — a Linux `~/webdav` grows one the moment it appears as a drive letter,
and a recycle bin follows the first delete. There is no arrangement of two
machines where one's recycle bin is the other's business.

They are matched case-insensitively even on a case-sensitive server, because
the casing is Windows's choice rather than yours and it varies between versions
(`$RECYCLE.BIN` and `$Recycle.Bin` both occur in the wild).

Turn them off per export when the export genuinely *is* a whole volume you are
backing up:

```yaml
server:
  exports:
    wholedisk:
      path: /mnt/backup-source
      default_excludes: false
```

Sizes accept `2M`, `512K`, or plain bytes.

## `client:` — what this machine mounts

Keys directly under `client:` are defaults. `client.mounts` names the mounts
they apply to, and each entry is a `url`, an `at`, and any overrides:

```yaml
client:
  exclude: [node_modules, .venv]
  auto_cache_max: 2M
  detect_conflicts: false

  mounts:
    work:
      url: ssh://azure/projects
      at: "P:"                    # drive letter on Windows, directory on Linux
    media:
      url: tcp://nas:7440/media
      at: "M:"
      exclude: []                 # inherit nothing
      auto_cache_max: 0           # stream it; do not fill the disk
```

```bash
alloyfs mount work      # one of them, by name
alloyfs start           # the agent plus every mount in the file
```

`at` is named that rather than `mountpoint` because it reads as a sentence next
to `url`, and this file is meant to be read.

Every key a mount may override:

| Key | Effect |
|---|---|
| `exclude` | Local-only globs; see [Excludes](#/guides/excludes) |
| `pin` | Always fully cache matching files |
| `auto_cache_max` | Auto-download files up to this size (`0` = off) |
| `auto_cache_budget` | Total cache budget; LRU eviction, pins exempt |
| `data_dir` | Where the overlay and cache live |
| `no_server_defaults` | Ignore the export's suggested client settings |
| `detect_conflicts` | Refuse writes over concurrent changes |
| `token` | Shared secret for a token-protected TCP agent |

CLI flags override whatever the file resolved to.

### A mount's list replaces, it does not union

An unset key inherits the `client:` default. A key the mount states wins
outright — and for a list that means **replacement**, not a merge:

```yaml
client:
  exclude: [node_modules, .venv]
  mounts:
    work: { url: tcp://h/x, at: "P:", exclude: [target] }   # target only
    media: { url: tcp://h/m, at: "M:", exclude: [] }        # nothing at all
```

`work` excludes `target` and nothing else. `.venv` and `node_modules` do not
come along.

Union reads as the friendly choice right up until one mount needs to *not*
inherit a pattern, at which point there is no way to say it. Replacement is
what makes `exclude: []` able to mean "inherit nothing", and it is the same
rule scalars follow, which is the rule nobody is surprised by.

## Two different `client:` blocks

The word appears in two places and they are unrelated mechanisms:

| | Written by | Means | Merge |
|---|---|---|---|
| `server.exports.<name>.client` | the **serving** machine | settings suggested to whoever mounts this export | lists are **unioned** with the mounter's own |
| top-level `client:` | the **mounting** machine | this machine's own mounts and their defaults | a mount's list **replaces** the one above it |

The difference is deliberate. A server suggestion and a client's own excludes
were written by two different people who each stated an intent, so letting
either replace the other would lose one. Inside one config file both lists were
written by the same person, and replacing is how they say "not that, this".

Server suggestions arrive at attach time over protocol v2+; sizes apply only
where the mount made no explicit choice, so `auto_cache_max: 0` is an off
switch a server cannot override. `--no-server-defaults` (or
`no_server_defaults: true`) skips the exchange entirely rather than merely
discarding the answer.

## Older files upgrade themselves

Two layouts predate this one: the `agent:` + `exports:` agent config, and the
flat mount config (`exclude`, `pin`, `auto_cache_max`, … at the top level) that
`alloyfs mount --config` took.

Both are still readable, and reading one **rewrites it in place** as v3 —
`alloyfs serve`, `alloyfs mount`, `alloyfs start`, whichever touches it first.
The original is kept beside it with `.bak` appended (`alloyfs.yml` becomes
`alloyfs.yml.bak`), because the rewrite cannot preserve comments: a serialiser
round-trips values, not the prose around them, and losing somebody's annotated
config without a copy would be unforgivable for a convenience feature. The
rewritten file opens with a short header saying what happened, since a config
that silently changed shape under you is alarming to find.

The write is staged through a temporary file and renamed, so an interrupted
upgrade leaves either the old config or the new one and never half of either.

There is no permanent legacy mode: the old layouts are a one-time reader rather
than a second code path to keep working forever. A carried compatibility mode
is the thing that quietly becomes permanent, and then every later change has to
be made twice.

A file that cannot be rewritten — read-only, or a directory this process cannot
write to — still loads. It converts in memory and logs why it could not be
upgraded on disk. Refusing to start because a *cosmetic* upgrade failed would
be the wrong trade entirely.

A **half-converted** file is refused rather than guessed at. A config carrying
both `server:` and `exports:` gets an error naming which keys pointed where:
picking one half could serve folders nobody meant to publish.
