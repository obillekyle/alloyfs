# The Linux kernel module

An out-of-tree filesystem driver, `alloyfs`, that exists to close exactly one
gap: **`inotify` for changes made on another machine**.

## Why it exists

The kernel only generates `inotify` from local VFS activity, and FUSE has no
passthrough. So on a FUSE mount, a change another machine made can invalidate
caches but cannot fire a watch — a file watcher inside the mount simply does not
see it.

The module injects genuine `fsnotify` events, so a remote create is
indistinguishable from a local one to any watcher. Renames arrive as a paired
`MOVED_FROM`/`MOVED_TO` with a shared cookie, the way a real rename does.

## Shape

A misc character device at `/dev/alloyfs` plus a filesystem type. The module is
deliberately thin — VFS glue and a request/response channel — while the
filesystem logic lives in a userspace Rust daemon. `alloyfs mount --backend kernel`
opens the device, mounts, and serves it.

## Installing

```bash
sudo packaging/install.sh --with-module
```

That installs the source through DKMS rather than a bare `.ko`. An out-of-tree
module is only valid for the kernel it was compiled against, and distributions
ship new kernels every few weeks — without DKMS the module silently stops
loading after the next upgrade, and the only symptom is `/dev/alloyfs` quietly
no longer existing.

## Who may open the device

The module registers it `0600 root:root`. The installed udev rule hands it to an
`alloyfs` group at `0660`:

```bash
sudo usermod -aG alloyfs "$USER"   # log out and back in
```

Not `0666`, and not `uaccess`: opening this device makes the opener a filesystem
**server** for every mount bound to it, which is enough to feed chosen bytes to
a process that thinks it is reading its own disk. Group membership is the
permission model.

## Secure Boot

With Secure Boot on, the kernel refuses unsigned modules and `modprobe` reports
`Key was rejected by service`. DKMS generates a signing key; enrolling it needs
a human at the boot-time MOK Manager screen:

```bash
sudo mokutil --import /var/lib/shim-signed/mok/MOK.der
# reboot, then choose "Enroll MOK"
```

No installer can automate that. `install.sh` detects the situation, completes
the installation, and prints the step rather than failing.

## Known gaps

- **`fcntl(F_GETLK)` returns `ENOLCK`.** The protocol cannot ask who holds a
  lock, and answering from the local list would report "free" while another
  machine held it. Taking locks works normally.
- **No page cache for file data**, so no `mmap`. Reads always go to the daemon:
  simple, and always coherent.

## Testing

Nine staged cases run the module inside QEMU against a purpose-built debug
kernel (lockdep, `DEBUG_ATOMIC_SLEEP`, `DEBUG_OBJECTS`, `slub_debug=FZPU`),
from a harness smoke test up to two mounts contending over a lock and a full
symlink suite.

```bash
cd kernel/test && ./run.sh --stage 9 --debug-kernel ~/kbuild/linux-6.14.11
```
