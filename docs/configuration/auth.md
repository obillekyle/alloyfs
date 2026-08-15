# TCP authentication

An SSH mount is already authenticated — reaching the process required an SSH
login. A TCP listener is not, so it has rules.

## The loopback rule

```yaml
agent:
  tcp_listen: "0.0.0.0:7440"
  tcp_token: "a-long-random-secret"
```

**A non-loopback listener without a token is refused at startup**, with an
error saying why. Anyone who can reach the port could otherwise mount every
export you publish. This is not a warning you can ignore — the agent will not
start.

Loopback without a token is fine and is the default: reaching `127.0.0.1` means
you are already on the machine.

## Mounting against a protected agent

```bash
alloyfs mount tcp://host:7440/projects /mnt/p --token a-long-random-secret
```

or `token:` in the mount config, which keeps it out of your shell history.

## How it works

Token-protected listeners require protocol v3 and turn away anything older at
the handshake — an older client could not decode the "auth required" answer and
would fail in a confusing way instead of a clear one.

The comparison is constant-time. A token that shares a prefix with the real one
takes exactly as long to reject as one that shares nothing.

## The HTTP API

Same shape, separately configured:

```yaml
agent:
  http_listen: "127.0.0.1:7441"
  http_token: "another-secret"
```

Presented as `Authorization: Bearer <token>`. The same loopback rule applies.
Note that `serve --stdio` never starts the HTTP API — it belongs to the
long-running agent, not to a per-mount transport.
