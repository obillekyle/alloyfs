# Installing on Linux

```bash
cargo build --release
sudo packaging/install.sh
```

Re-running the installer is the supported upgrade path — every step is
idempotent.

## What goes where

| Path | What |
|---|---|
| `/usr/local/bin/alloyfs` | the binary (`--prefix` moves it) |
| `/etc/systemd/system/alloyfs@.service` | the service template |
| `/usr/src/alloyfs-<version>/` | module source, for DKMS (`--with-module`) |
| `/etc/udev/rules.d/60-alloyfs.rules` | device permissions (`--with-module`) |
| `/etc/modules-load.d/alloyfs.conf` | load at boot (`--with-module`) |

**`~/.alloyfs` is never touched**, by install or uninstall. `data/` holds the
overlay, and uninstalling software is not allowed to be how you lose files that
exist on no server.

## Options

| Flag | Effect |
|---|---|
| `--with-module` | Also install the kernel module through DKMS, create the `alloyfs` group, install the udev rule |
| `--user NAME` | Enable `alloyfs@NAME` |
| `--start` | Start it now (needs `--user`) |
| `--no-service` | Binary only |
| `--no-autoload` | Do not load the module at boot |
| `--binary PATH` | Install a binary from elsewhere |

The installer deliberately does **not** run `cargo` — building under `sudo`
leaves a root-owned `target/` that breaks every later developer build.

## Uninstalling

```bash
sudo packaging/uninstall.sh
```

Reverses all of it: instances disabled before the binary goes, module unloaded
before its files are removed, no dangling unit or device node. `--keep-group`
leaves the `alloyfs` group in place. Running it twice is fine; the second run
says "nothing to remove" and exits cleanly.

## See also

- [Running as a service](#/deployment/service)
- [The Linux kernel module](#/backends/kernel-module) — including Secure Boot
