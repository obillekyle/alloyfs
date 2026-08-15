# config.yml

One file describes what a machine serves and how its mounts behave.

## Getting one

```bash
alloyfs init            # ./alloyfs.yml, exporting the current directory
alloyfs init --global   # ~/.alloyfs/config.yml instead
```

[`alloyfs init`](#/reference/cli) fills in the export for you rather than
leaving you to write one. See [Your first mount](#/getting-started/first-mount).

## Where it is found

`alloyfs serve` looks, in order:

1. **`./alloyfs.yml`**, `./alloyfs.yaml`, `./alloyfs.json` — the directory you
   are standing in. This is what `alloyfs init` writes, and the same convention
   cargo, npm and docker compose use: a tool run inside a project reads that
   project's config.
2. **The same names beside the executable** — a portable install, where the
   binary and its config travel together.
3. **`~/.alloyfs/config.{yml,yaml,json}`** (`%USERPROFILE%\.alloyfs\` on
   Windows) — the per-user default.

If nothing exists anywhere, `serve` writes a commented template to the per-user
location, so a first run leaves you a file to edit rather than an error telling
you to invent one.

`--config <PATH>` skips the search entirely.

**The agent logs which config it loaded**, every start. "Which config is this
agent actually serving" should never need guessing — especially now that the
answer depends on where you ran it from.

JSON is accepted because the YAML parser reads it: YAML 1.2 is a superset of
JSON, so a `.json` config is just a config, with no second parser and no second
set of quirks to learn.

**Mounting needs no config at all.** `alloyfs mount` takes everything from
flags; a mount config (a different, smaller schema — see below) is optional and
only ever loaded via `--config`. A first mount creates `~/.alloyfs/` for the
[overlay and cache](#/configuration/where-things-live), and nothing else.

## Serving

```yaml
agent:
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
not leak. **`client.exclude` is a suggestion**, merged into what the client
already wanted.

Sizes accept `2M`, `512K`, or plain bytes.

## Mount config

A mount can carry its own file instead of a long command line:

```yaml
exclude: [node_modules]
pin: ["*.lock"]
auto_cache_max: 2M
auto_cache_budget: 512M
detect_conflicts: false
no_server_defaults: false
# token: "shared-secret"
# data_dir: /custom/path
```

```bash
alloyfs mount ssh://host/projects /mnt/p --config ~/projects-mount.yml
```

CLI flags always override file values.
