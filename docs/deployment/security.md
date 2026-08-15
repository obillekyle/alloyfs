# Security

## The export boundary

Every path is canonicalised and checked against the export root before it is
touched. A path that resolves outside — including through a symlink — is
refused. The watcher does not follow symlinks out of the export either.

**Symlink creation is checked on where the link lands**, not on whether its
target exists today. A dangling `../../etc/passwd` is refused, because the day
that path appears the link becomes a hole in the boundary.

## Excluded paths do not leak

A server-side `exclude` makes a path invisible: it reports **not found**, never
**forbidden**. The difference matters — "forbidden" confirms the file exists.

Symlinks whose target lands inside an excluded path are refused for the same
reason; otherwise a link is a one-line way to read exactly what the config
says is invisible.

## Network exposure

- **A non-loopback TCP listener without a token is refused at startup.** Not a
  warning — the agent will not run.
- The same rule applies to the HTTP API.
- Tokens are compared in constant time.
- Token-protected listeners require protocol v3 and turn away older clients at
  the handshake rather than failing them confusingly later.

See [TCP authentication](#/configuration/auth).

## Prefer SSH where you can

An `ssh://` mount needs no listener, no open port and no token: reaching the
agent already required an SSH login, with your existing keys and host policy.
It is the lowest-exposure option and costs nothing extra.

## Read-only exports

```yaml
exports:
  reference:
    path: /srv/reference
    read_only: true
```

Enforced at the agent, on every mutating operation — over the wire and through
the HTTP API alike. It is not a client-side courtesy.

## The kernel module device

`/dev/alloyfs` is `0660 root:alloyfs`, not world-accessible. Opening it makes
the opener a filesystem server for every mount bound to it, which is enough to
feed chosen bytes to a process that believes it is reading its own disk. Group
membership is the permission model.
