# config.yml

One file describes what a machine serves and how its mounts behave.

## Where it is found

In order:

1. **`alloyfs.yml` beside the executable** — a portable install (binary plus
   config in one folder, or on a stick) works without touching the home
   directory, and wins over everything else.
2. **`~/.alloyfs/config.yml`** (`%USERPROFILE%\.alloyfs\config.yml`) — the
   normal location. Created from a commented template on first run, so a first
   run leaves you a file to edit rather than an error telling you to invent one.

`--config <PATH>` overrides both.

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
