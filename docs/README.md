# AlloyFS

Export a folder on one machine; mount it as a real drive on another.

AlloyFS is a virtual drive service for Windows and Linux. The **agent** publishes
folders; **clients** mount them as an ordinary drive letter (WinFsp) or
mountpoint (FUSE), or — on Linux — through a custom kernel filesystem that can
deliver remote changes as genuine `inotify` events.

```bash
# on the machine with the files
alloyfs init && alloyfs serve --config alloyfs.yml

# on another machine
alloyfs mount ssh://host/projects /mnt/projects     # Linux
alloyfs mount ssh://host/projects X:                # Windows
```

No daemon to expose and no open port required: an `ssh://` mount spawns the
agent over your existing SSH config and speaks the protocol on stdin/stdout.

## What it is good at

- **Working directly on remote files.** `git` works inside a mount. Editors
  save without a sync step.
- **Live change events.** Server-side watching, pushed to every client. Windows
  mounts get native `ReadDirectoryChangesW` re-emission, so editors auto-refresh.
- **Not being all-or-nothing.** Excluded paths (`node_modules`) stay local and
  never cross the wire. Small files can be cached for offline reads.

## What it is not

Not a sync service by default — a mount is write-through, so an acknowledged
write is on the server. If you want files on local disk instead, that is
[sync mode](#/guides/sync-mode), which is a different tool in the same binary.

Not a database host. Whole-file advisory locks only; see
[Limitations](#/reference/limitations).

## Install

```bash
curl -fsSL https://alloy.okyle.dev/install.sh | sh    # Linux
```

```powershell
irm alloy.okyle.dev/install.ps1 | iex                 # Windows
```

Then, in the folder you want to share:

```bash
alloyfs init
alloyfs serve --config alloyfs.yml
```

`alloyfs init` writes the config for that directory, so there is nothing to
hand-write before the first `serve`. Keep it up to date with `alloyfs update`.

## Where to go next

- [Installation](#/getting-started/installation) — the one-liner, and updating
- [Your first mount](#/getting-started/first-mount) — end to end in two commands
- [How it works](#/getting-started/how-it-works) — the shape of the system
