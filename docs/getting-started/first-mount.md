# Your first mount

Two machines: one serves, one mounts. If you only have one, both roles work
fine on `127.0.0.1`.

## 1. Serve a folder

On the machine with the files, in the folder you want to share:

```bash
cd ~/projects
alloyfs init
alloyfs serve
```

`alloyfs init` writes an `alloyfs.yml` describing that directory — export name
derived from the folder, sensible excludes, loopback listener — so there is
nothing to hand-write. The first `alloyfs serve` brings it to the current
version-3 layout in place, keeping the original beside it as `alloyfs.yml.bak`,
and what you edit from then on is this:

```yaml
version: 3

server:
  tcp_listen: "127.0.0.1:7440"
  exports:
    projects:
      path: "/home/you/projects"
      read_only: false
      exclude:
        - "**/.git"
```

The same file also describes what this machine *mounts*, under a `client:`
section — one file for both halves of a machine.

Prefer it in your home config instead of per-directory? `alloyfs init --global`
writes `~/.alloyfs/config.yml`, which `alloyfs serve` finds on its own with no
`--config`.

Either way, edit the file and re-run — it is a plain YAML file, and
[config.yml](#/configuration/config-file) explains every key.

## 2. Mount it

```bash
# Linux
alloyfs mount tcp://127.0.0.1:7440/projects /mnt/projects

# Windows
alloyfs mount tcp://127.0.0.1:7440/projects X:
```

That is a real drive. `ls`, `git status`, and your editor all work against it.

**On Windows, mount from an elevated terminal.** Without Administrator the drive
letter is session-local rather than registered with the Mount Manager, and some
tools — bun in particular — then report `ENOENT` for files that exist. See
[Mounting a drive](#/guides/mounting).

## Over SSH instead

If you can `ssh host`, you can mount from it — no listener, no open port, no
firewall rule:

```bash
alloyfs mount ssh://host/projects /mnt/projects
```

The remote side needs `alloyfs` on its `PATH` and a config file naming the
export. AlloyFS spawns `alloyfs serve --stdio` over the SSH exec channel and
speaks the protocol on its stdin/stdout.

### The `PATH` that matters is the non-interactive one

This is the most common way a first `ssh://` mount fails. The Linux installer
puts the binary in `~/.local/bin`, which reaches your `PATH` through
`~/.profile` (or whatever the installer offered to append to) — and **that file
is read by a login shell, which `ssh host <command>` does not start**. The mount
runs its command over the SSH exec channel, so it gets the non-interactive
environment, where `~/.local/bin` is usually absent.

Check exactly what the mount will see:

```bash
ssh host alloyfs --version     # what the mount actually runs
ssh host 'echo $PATH'          # the PATH it will be run with
```

If the first fails while an interactive `ssh host` finds `alloyfs` perfectly
well, this is what happened. Three ways out, easiest first.

**Name the binary outright.** Nothing to change on the far side:

```bash
alloyfs mount ssh://host/projects /mnt/projects --remote-cmd '~/.local/bin/alloyfs'
```

Quote it. The tilde has to arrive at the *remote* shell, which is what expands
it; unquoted, your own shell would substitute your own home directory first.

**Put it somewhere already on that `PATH`:**

```bash
ssh host 'sudo ln -s ~/.local/bin/alloyfs /usr/local/bin/alloyfs'
```

**Or install it there to begin with**, on the machine being mounted:

```bash
curl -fsSL https://alloy.okyle.dev/install.sh -o install.sh
sudo ALLOYFS_INSTALL=/usr/local/bin sh install.sh
```

Editing the remote's shell startup files is the fiddliest option, because the
stock `~/.bashrc` opens by returning early for non-interactive shells — exactly
the case that needs the entry.

## Unmounting

Ctrl-C the mount process. On Linux you can also `fusermount3 -u /mnt/projects`.

## Next

- [Mounting a drive](#/guides/mounting) — every option that matters
- [Excludes and the overlay](#/guides/excludes) — keep `node_modules` local
