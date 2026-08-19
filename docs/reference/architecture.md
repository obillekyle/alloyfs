# Architecture

The map of which crate holds what. For why the pieces are shaped this way, and
what the performance work measured, see
[Design and performance](#/reference/design).

Bottom to top:

```
alloyfs-proto      the wire format — pure data, no I/O, no platform
alloyfs-transport  multiplexing over TCP or an SSH exec channel
alloyfs-agent      the server: exports, watching, locks, path hardening
alloyfs-client     the client engine: RemoteFs, cache, overlay, readahead
mount-{fuse,winfsp,kernel}   three thin backends
alloyfs-cli        the binary
kernel/alloyfs     the optional Linux kernel module (C)
```

## Why the split matters

**`alloyfs-proto` does no I/O.** It is pure data, so both ends can never
disagree about what bytes mean, and it is testable on any platform. A golden
test freezes the encoding of every variant: if a byte changes, a deployed peer
can no longer talk to us, so a failure means "decide about the protocol
version", never "update the test".

**The backends are thin.** They translate one dialect of filesystem callbacks
into `RemoteFs` calls. The logic that can be wrong lives in one place and is
tested everywhere; only driver plumbing is platform-specific. The kernel module
follows the same rule — the C is VFS glue, and a userspace Rust daemon holds the
filesystem logic, which is why its bugs are found in milliseconds on a laptop
rather than in a VM boot.

## Protocol versioning

The handshake picks `min(client_max, server_max)`. New request types are
**appended** — postcard encodes variant indices, so inserting one would shift
every later variant and break the wire. Features are gated on the negotiated
version:

| Version | Added |
|---|---|
| v1 | The base operation set |
| v2 | Server-suggested mount defaults |
| v3 | TCP token auth, transparent frame compression |
| v4 | Symlink creation and readlink |
| v5 | `WrittenAttr` — the write reply carries the file's new attributes |

A client that would send a v4 request to a v3 peer refuses with a clear error
naming which side is too old, rather than sending bytes the peer would decode as
something else.

## Write-through

Mounts do not buffer. When `write()` returns, the bytes are on the server;
acknowledged writes are never lost. In-flight operations fail with `EIO` on a
partition, and server-side leases free a dead client's locks after ~30 s.
