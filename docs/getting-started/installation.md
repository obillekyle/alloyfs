# Installation

## The one-liner

**Windows**

```powershell
irm alloy.okyle.dev/install.ps1 | iex
```

**Windows, interactively** -- asks for administrator rights once, then installs
the WinFsp driver too:

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

On Windows there is one thing that does, and it is the driver rather than
AlloyFS.

### WinFsp

A mount on Windows goes through [WinFsp](https://winfsp.dev), which is a kernel
driver. Without it `alloyfs` runs perfectly well and every mount fails.

The two Windows installers divide the work:

- **`install.ps1` is silent.** It prompts for nothing and elevates nothing, so
  it is safe to run unattended. It installs WinFsp only if it is already
  running with administrator rights, and otherwise says that mounting will not
  work yet and how to fix it.
- **`install.cmd` is interactive.** It asks for administrator rights once, up
  front, and then runs the silent installer with them. Consent comes before the
  work rather than halfway through a download.

AlloyFS itself installs into your own profile and needs no rights either way.
The cmd installer resolves that location BEFORE elevating and carries it
across, so that a UAC prompt answered with a different administrator account
still installs AlloyFS for you rather than for them.

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

[WinFsp](https://winfsp.dev). `install.cmd` installs it for you; `install.ps1`
does too when already elevated, and otherwise tells you it is missing rather
than leaving it to the first failed mount.

Installing it by hand is fine as well. The standard installer is enough — the
Developer/SDK feature is only needed to build AlloyFS from source.

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
