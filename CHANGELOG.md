# Changelog

Notes are written by hand. `cutver` opens the heading and never fills it in —
a list of commit subjects is not a description of what changed for anyone
outside the repository.

## [Unreleased]

## [0.2.0] — 2026-08-15

## [0.1.1] — 2026-08-15

## [0.1.0] — 2026-08-15

First release. The version was computed from the commit history rather than
chosen: five `feat` commits and eight `fix`/`perf` ones since the first commit,
no breaking markers anywhere, so a minor from a `0.0.0` baseline.

That baseline is worth one sentence, because the manifests said `0.1.0` for most
of this project's life. Nothing of that name was ever tagged, built or shipped —
it was a placeholder from commit one. Left in place it would have made the first
real release compute as `0.2.0`, with `0.1.0` skipped and nothing to explain the
gap.

### What it is

A virtual drive service. An agent exports a folder; clients mount it as a real
drive over three backends — WinFsp on Windows, FUSE on Linux, and a purpose-built
Linux kernel module — plus a bidirectional file-sync mode for people who would
rather have local copies than a mount.

Ten crates in one workspace, versioned in lockstep through
`[workspace.package]`:

| | |
| --- | --- |
| `alloyfs-proto` | the wire protocol, currently v4 |
| `alloyfs-transport` | framing, TCP auth tokens, transparent compression |
| `alloyfs-common` | shared types and the Linux errno table |
| `alloyfs-agent` | the export side |
| `alloyfs-client` | the mount and sync side |
| `alloyfs-mount-fuse` `alloyfs-mount-winfsp` `alloyfs-mount-kernel` | one per backend |
| `alloyfs-http` | the status endpoint |
| `alloyfs-cli` | the `alloyfs` binary |

### Notable in this release

- **Symlinks work across every backend** — reparse points on WinFsp, native
  links in the kernel module, and one rewriting path shared between them rather
  than three that disagree.
- **Advisory locks survive a reconnect**, and a blocking wait is bounded by
  liveness rather than by hope.
- **Sequential readahead with a measured window.** The window is measured rather
  than rebuilt per request, which is where roughly 2x sequential mount
  throughput came from.
- **Auth and compression on the wire** (protocol v3), and negotiated mount
  defaults (v2), so a client and an agent of different ages agree on terms
  instead of guessing.
- **`statfs` tells the truth**, including a `df` that reflects the export rather
  than the loopback mount underneath it.

### Installing

From source, as before — `sudo packaging/install.sh`. The release also carries a
prebuilt `alloyfs` binary for Linux, macOS and Windows, which is enough for the
client and agent but not for the kernel module; that is DKMS's job and
`install.sh` still owns it.

### Not published to crates.io

Deliberate. `cargo publish` reserves a crate name permanently, and ten of them is
not a thing to do as a side effect of wanting version numbers. AlloyFS is a
system service installed from source, not a library to `cargo add`. A tag builds
the executables and attaches them here; nothing touches a registry.
