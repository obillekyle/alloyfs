# Design and performance

`architecture.md` is the map — which crate holds what. This is the reasoning:
why the pieces are shaped the way they are, and what the performance work
actually measured.

## The budget is latency, not bandwidth

A mount is not a download. Copying a file is bandwidth-bound and easy; *using*
a directory is a long sequence of small, dependent questions, and each one that
reaches the server costs a full round trip before the next can start.

That single fact decides most of the design. On the reference link — a remote
agent at ~60 ms RTT — the arithmetic is unforgiving:

```
one round trip                       60 ms
a 19-entry directory, one RTT/file  ~1.2 s
the same at two RTT/file            ~2.4 s
```

Two and a half seconds to list nineteen files, on a link with bandwidth to
spare. Nothing about faster hardware or a fatter pipe improves it. The only
lever is asking fewer questions, and every optimisation below is a variation on
that: remove a round trip, batch one into another, or answer it locally.

Bandwidth work exists (chunking, prefetch, compression) and matters for large
sequential reads. It is the smaller half.

## Layers, and the one rule that shapes them

```
alloyfs-proto      795   wire format — pure data, no I/O, no platform
alloyfs-transport  620   multiplexing over TCP or an SSH exec channel
alloyfs-agent     1694   the server: exports, watching, locks, path hardening
alloyfs-client    5383   the engine: RemoteFs, caches, overlay, readahead
mount-winfsp      1368   Windows
mount-kernel      1611   Linux, via the C module
mount-fuse         593   Linux, via libfuse
alloyfs-cli       5954   the binary
kernel/alloyfs    3217   the optional Linux kernel module (C)
```

**The backends are thin on purpose, and the numbers show it.** Three backends
total 3,572 lines against the client engine's 5,383. They translate one dialect
of filesystem callbacks into `RemoteFs` calls and hold no filesystem logic of
their own. Everything that can be *wrong* — cache freshness, conflict
detection, handle lifetime, reconnection — lives in one place, is tested once,
and is fixed once for all three platforms.

The kernel module follows the same rule from the other side: the C is VFS glue,
and the filesystem logic sits in a userspace Rust daemon. A bug there is found
in milliseconds on a laptop instead of by booting a VM. Which backend to run is
a separate question — see [choosing a backend](#/backends/choosing).

## The wire

**postcard, and variants are append-only.** postcard encodes enum variants by
index and is not self-describing, so inserting a variant shifts every later one
and silently changes what deployed peers decode. New operations are appended,
never inserted. A golden test freezes the encoding of every variant: a byte
changing there means "decide about the protocol version", never "update the
test".

That constraint has teeth. Protocol 5 needed a write reply carrying the file's
post-write attributes. Adding fields to the existing `Written` was the obvious
shape and is not implementable — one variant index cannot decode to two shapes,
and an older peer would read the trailing attributes as the start of the next
message. It became a new variant, `WrittenAttr`, appended last.

**Negotiation is a range intersection**, not a match. Each side offers
`[min, max]`; the session runs at the highest both understand. Features are
gated on the result rather than assumed.

| Version | Added |
|---|---|
| v1 | The base operation set |
| v2 | Server-suggested mount defaults |
| v3 | TCP token auth, transparent frame compression |
| v4 | Symlink creation and readlink |
| v5 | `WrittenAttr` — attributes on the write reply |

A client that would send a v4 request to a v3 peer refuses with an error naming
which side is too old, rather than emitting bytes the peer will misread.

**Framing.** `MAX_FRAME_LEN` is 1 MiB; bulk data moves in `DATA_CHUNK` blocks of
128 KiB. Requests are multiplexed over one connection and answered by id, so a
slow read never blocks an unrelated stat.

**Two carriers, one protocol.** `tcp://` reaches a long-lived agent.
`ssh://host/export` spawns `alloyfs serve --stdio` down an existing SSH exec
channel — no listening port, no shared token, and authentication is the SSH key
already present. The difference is not cosmetic: an ssh agent is per-connection
and exits with the mount, which changes what server-side state can be relied on
across a restart (see *What a cursor cannot buy*).

## Four caches, four different questions

They are separate because they answer different questions with different
lifetimes. Collapsing them would mean the shortest-lived one setting the
freshness of everything.

| Cache | Answers | Lifetime | Invalidated by |
|---|---|---|---|
| Kernel metadata (`file_info_timeout`) | "what are this file's attributes" without entering user space | 1 s | expiry; the event stream fans out as `ReadDirectoryChangesW` |
| Attribute cache (`ATTR_TTL`) | the same, without entering the network | 5 s | event stream, per inode |
| Auto-cache (blobs on disk) | "what are this file's *bytes*" | across restarts | size/mtime/version mismatch, or an event |
| Overlay | paths the client owns outright | permanent | never — the server has no copy to disagree with |

**The attribute cache is filled by listing, not by asking.** A readdir reply
carries a full `Attr` per entry, and every one is cached on arrival. A directory
listing therefore pre-answers the stat of everything in it. Measured: 19 files
cost 6 getattrs, not 19.

**The auto-cache is the only one that survives a restart.** It stores whole
files under a size ceiling (2 MiB by default from the CLI, 512 MiB total budget;
off by default in the library) plus anything pinned by glob — see
[the cache guide](#/guides/cache) for the knobs. Its manifest
records size, mtime and version per blob, and — since the cursor work — the
event sequence the cache was last current at.

**The overlay is not a cache at all**, though it sits in the same path. A
client-excluded glob is redirected to local disk: the file is real, created on
first write, and the server never hears about it. This is the half of
[excludes](#/guides/excludes) that redirects rather than hides — an export-side
exclude is the other kind, and a path it hides cannot be created either. This is what makes
`node_modules` workable over a mount — per-platform content the client should
own, kept out of the round-trip economy entirely, with the lockfile still
syncing so the two installs stay honest.

## Measured decisions

Every row was measured before and after, adjacently, on the same link. The
reference link is ~60 ms RTT and ~2 MB/s.

| Change | Before | After |
|---|---|---|
| Tolerant readahead windowing | 3.2 MB/s | 6.4 MB/s (1 MiB reads) |
| Write reply carries attributes (v5) | 2 RTT/write | 1 RTT |
| Release stops awaiting a discarded reply | 1 RTT/close | 0 |
| Cached read-only open skips the server | 961 ms | 178 ms (`ls -la`, 19 files) |

**Tolerant windowing** was a bug wearing performance clothing. The prefetch
window cleared on any offset that was not exactly the expected next one — and
WinFsp's multi-threaded dispatch plus the cache manager's overlapped read-ahead
make slightly out-of-order offsets the *normal* shape of a sequential stream.
Real copies ran with a perpetually collapsing window, discarding ~2 MiB of
useful in-flight data per hiccup and burning wire on aborted fetches. Now only a
genuine far seek clears. The window is 32 blocks (4 MiB); 32 beat 16 by ~8% once
it stopped collapsing.

**The lazy open** was the largest single win and the least obvious. Browsing was
dominated by opens, not metadata: `ls -la` over 19 files put 44 requests on the
wire, 25 of them opens. Most wanted nothing — `ls`, git, and Explorer's property
handlers open a file, look at it and close it again. The server was only being
asked for an attribute so the client could decide whether its cached blob was
current, and the readdir had already supplied that attribute. A read-only open
of a file the cache holds at the right version now skips the server entirely.

Writes, truncation, append and `O_EXCL` always reach the server: a cached blob
describes what a file *was*, which is no basis for changing it. Anything that
genuinely needs a handle takes one out at that moment.

## A decision measured and then not taken

The backlog carried a per-handle prefetch pump — move the window top-up off the
read path so it refills continuously rather than once per kernel read. It was
measured first, and the premise did not hold.

Reading 64 MiB in 1 MiB reads, with statistics enabled: FUSE over loopback
served 510 of 512 blocks from the window against 2 synchronous fetches and zero
clears, sustaining 415 MB/s where the raw transport benchmark tops out at 479.
WinFsp over a real link served 504 against 16, again zero clears. The window was
already full; the synchronous fetches were cold start.

The reason was already in the read path: the top-up runs *before* the caller
blocks on its own blocks, so prefetches ride the connection while it waits —
which is the decoupling the pump was meant to introduce. Building it would have
added a lifecycle with four ways to be wrong (cancel on write, on far seek, on
release, on reconnect) to chase headroom that was not there.

Recorded because a plausible optimisation that measurement kills is worth as
much as one it confirms, and costs more to re-derive.

## What keeps a cache honest

Serving bytes from disk is only safe if staleness is detectable. Three
mechanisms overlap, deliberately:

**Size and mtime are co-primary with version.** Server versions live in memory
and reset when the agent restarts — which for `ssh://` is every mount — so a
version match alone proves nothing. A blob may serve reads only if size *and*
mtime match, and version matches unless either side reports 0.

This is the mechanism that actually carries correctness. Verified directly: with
the mount stopped, a cached file was modified on the server; on restart the
change was read back rather than served from the blob, because the attribute the
readdir fetched no longer matched the manifest.

**The event stream invalidates per entry.** The agent watches each export and
broadcasts changes with a monotonic sequence number. Clients apply them to the
attribute cache and the blob cache by path, and fan them out to the platform's
own change notification. Self-origin events are stripped server-side, so a
client's own writes update its caches through synchronous hooks instead.

**Leases bound the damage.** A dead client's locks and handles are reclaimed
after ~30 s. Requests time out at 30 s; liveness is checked by a 10 s keepalive
with a 10 s grace, so a wedged peer fails a waiting caller instead of hanging it
forever.

### What a cursor cannot buy

The cache manifest records the event sequence it was last current at, and a
mount resumes its subscription from there: the server replays what changed, and
anything unmentioned is provably current — one request to validate a whole tree
instead of one per file. If the server cannot replay from that point it answers
`TooOld`, and the client drops its caches rather than trusting them.

**On `ssh://` this buys nothing across a restart**, and the reason is worth
stating plainly. That transport spawns the agent per connection, so it exits
with the mount and the next one begins with an empty ring log. Asked to replay
from a cursor it has never heard of, a fresh agent returns no events *and* no
`TooOld` — the staleness test compares against the log's head, and there is no
head. The gain is real only against a long-lived `tcp://` agent whose log
outlives any one client.

Tree-level validation over ssh needs a token the *server* persists, derived from
export state rather than from a session counter. That is not built.

## Failure and recovery

**Write-through, always.** Mounts do not buffer. When `write()` returns the
bytes are on the server; an acknowledged write is never lost. In-flight
operations fail with `EIO` on a partition rather than silently succeeding.

**Handles outlive connections.** The handle the kernel holds is stable; the
server-side handle behind it is rewritten on reconnect. The supervisor reopens
each one and replays any advisory lock it held. A handle whose lock could not be
restored is *poisoned* — subsequent I/O fails with `EIO` rather than continuing
as though mutual exclusion still held. Silence there would be the dangerous
answer.

**Unmount is part of shutdown.** Every backend detaches on SIGINT. Skipping it
leaves a mountpoint whose every operation fails with `ENOTCONN` until it is
cleared by hand — and on Windows, a drive letter that survives its process needs
a reboot to release. The FUSE backend unmounts lazily on purpose: a plain
unmount refuses while any shell sits in the directory, which would convert a
stale mount into a hung stop *plus* a stale mount.

## Where the cost still is

Honest remainder, in rough order:

- **The first visit to a cold directory** still costs a readdir plus whatever
  the client opens for real. Cheaper than it was, not free.
- **Metadata does not persist.** Directory listings and attributes are
  in-memory only, so a restart re-fetches them even though the blobs survived.
- **Per-kernel-read dispatch** is the gap between through-mount throughput and
  raw transport capacity on a fast link. Measured, not yet closed.
- **The kernel module builds only against 6.14-era kernels.** Three VFS changes
  in 7.0 need version guards before the supported range can widen.
