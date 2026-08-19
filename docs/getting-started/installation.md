# Installation

## The one-liner

**Windows**

```powershell
irm alloy.okyle.dev/install.ps1 | iex
```

From `cmd.exe`, which cannot pipe a download into a shell:

```bat
curl -fsSL https://alloy.okyle.dev/install.cmd -o install.cmd && install.cmd
```

**Linux**

```bash
curl -fsSL https://alloy.okyle.dev/install.sh | sh
```

All of them fetch the binary for your platform from the latest release, check it
really is an executable rather than an error page, put it somewhere sensible,
and add it to your `PATH`. **AlloyFS itself installs per-user and needs no
administrator rights.**

On Windows there is one exception, and it is the only prompt you should see.

### WinFsp

A mount on Windows goes through [WinFsp](https://winfsp.dev), which is a kernel
driver. Without it `alloyfs` runs perfectly well and every mount fails, so the
installer offers to fetch and install it when it is missing. That step needs
administrator rights and raises a UAC prompt; nothing else in the install does.

The elevation is scoped to that one step on purpose. Running the whole
installer as an administrator is **not** the way to grant it: if you
authenticate as a different account, `%LOCALAPPDATA%` and the user `PATH`
become *that* account's, and AlloyFS installs into a profile nobody is logged
into.

Before running it, the installer checks the MSI's Authenticode signature and
refuses anything that is not validly signed. It installs the driver and
runtime, not the SDK — that is a build dependency, and nothing about mounting
needs it.

If WinFsp needs a reboot to finish, the installer says so rather than leaving
you to discover it at the first mount.

| Variable | Effect |
|---|---|
| `ALLOYFS_VERSION` | Install a specific tag (`v0.1.1`) instead of the latest |
| `ALLOYFS_INSTALL` | Install somewhere other than the default |
| `GITHUB_TOKEN` | Optional; raises the GitHub API rate limit |
| `ALLOYFS_SKIP_WINFSP` | Windows: do not offer to install the driver |
| `WINFSP_VERSION` | Windows: install a specific WinFsp tag (`v2.1`) |

Defaults are `%LOCALAPPDATA%\Programs\alloyfs` and `~/.local/bin`.

`~/.local/bin` reaches your `PATH` through a login shell's profile, which
`ssh host <command>` never starts — so a machine that will be mounted **from**
elsewhere over `ssh://` usually wants `ALLOYFS_INSTALL=/usr/local/bin` instead.
See [Your first mount](#/getting-started/first-mount).

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
