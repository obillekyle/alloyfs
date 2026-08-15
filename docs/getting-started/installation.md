# Installation

## The one-liner

**Windows**

```powershell
irm alloy.okyle.dev/install.ps1 | iex
```

**Linux**

```bash
curl -fsSL https://alloy.okyle.dev/install.sh | sh
```

Both fetch the binary for your platform from the latest release, check it really
is an executable rather than an error page, put it somewhere sensible, and add
it to your `PATH`. Neither needs root.

| Variable | Effect |
|---|---|
| `ALLOYFS_VERSION` | Install a specific tag (`v0.1.1`) instead of the latest |
| `ALLOYFS_INSTALL` | Install somewhere other than the default |
| `GITHUB_TOKEN` | Optional; raises the GitHub API rate limit |

Defaults are `%LOCALAPPDATA%\Programs\alloyfs` and `~/.local/bin`.

Your configuration lives in `~/.alloyfs`, deliberately somewhere else — so
reinstalling or removing AlloyFS never touches the [overlay](#/guides/excludes)
or your [sync baselines](#/guides/sync-mode).

## Staying up to date

```bash
alloyfs update              # the latest release
alloyfs update v0.1.1       # pin, or roll back
alloyfs update --dry-run    # print the command, run nothing
```

`update` re-runs the installer rather than replacing the binary itself. That
keeps one implementation of "download, verify, put on `PATH`" instead of two
that drift — and a filesystem binary does not need a TLS stack linked into it
for the sake of one rarely-used command.

## The driver each platform needs

The machine doing the **serving** needs nothing beyond the binary. The machine
doing the **mounting** needs a filesystem driver.

### Windows

Install [WinFsp](https://winfsp.dev). The standard installer is enough — the
Developer/SDK feature is only needed to build AlloyFS from source. The installer
warns you if it is missing.

### Linux

FUSE is present on any desktop distribution. If `/dev/fuse` is missing:

```bash
sudo apt install fuse3
```

The optional [kernel module](#/backends/kernel-module) is separate, and only
worth installing if you need `inotify` to fire for changes other machines make.

## Building from source

```bash
cargo build --release
```

The workspace builds on Windows (MSVC or gnullvm) and Linux. `scripts/verify.sh`
runs formatting, clippy with warnings denied, the full test suite and a release
build — the same gate CI applies.

For a system-wide Linux install with the service and optionally the kernel
module, see [Installing on Linux](#/deployment/linux).

## Checking it worked

```bash
alloyfs --version
alloyfs --help
```

Then [make your first mount](#/getting-started/first-mount).
