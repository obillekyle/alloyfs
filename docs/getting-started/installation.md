# Installation

AlloyFS is a single binary. It needs a filesystem driver on the machine doing
the *mounting*; the machine doing the *serving* needs nothing extra.

## Windows

Install [WinFsp](https://winfsp.dev) (the standard installer is enough — the
Developer/SDK feature is only needed to build from source), then drop
`alloyfs.exe` somewhere on your `PATH`.

## Linux

FUSE is already present on any desktop distribution. If `/dev/fuse` is missing:

```bash
sudo apt install fuse3
```

Then place `alloyfs` on your `PATH`, or use the installer:

```bash
sudo packaging/install.sh
```

See [Installing on Linux](#/deployment/linux) for what that script touches, and
[The Linux kernel module](#/backends/kernel-module) for the optional driver that
makes `inotify` work for remote changes.

## Building from source

```bash
cargo build --release
```

The workspace builds on Windows (MSVC or the gnullvm toolchain) and Linux.
`scripts/verify.sh` runs formatting, clippy with warnings denied, the whole
test suite, and a release build — the same gate CI applies.

## Checking it works

```bash
alloyfs --version
alloyfs ping tcp://127.0.0.1:7440
```
