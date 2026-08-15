# Your first mount

Two machines: one serves, one mounts. If you only have one, both roles work
fine on `127.0.0.1`.

## 1. Serve a folder

On the machine with the files, create `~/.alloyfs/config.yml` — running any
command once will write a commented starter for you:

```yaml
agent:
  tcp_listen: "127.0.0.1:7440"

exports:
  projects:
    path: /home/you/projects
```

Then:

```bash
alloyfs serve
```

## 2. Mount it

```bash
# Linux
alloyfs mount tcp://127.0.0.1:7440/projects /mnt/projects

# Windows
alloyfs mount tcp://127.0.0.1:7440/projects X:
```

That is a real drive. `ls`, `git status`, and your editor all work against it.

## Over SSH instead

If you can `ssh host`, you can mount from it — no listener, no open port, no
firewall rule:

```bash
alloyfs mount ssh://host/projects /mnt/projects
```

The remote side needs `alloyfs` on its `PATH` and a config file naming the
export. AlloyFS spawns `alloyfs serve --stdio` over the SSH exec channel and
speaks the protocol on its stdin/stdout.

## Unmounting

Ctrl-C the mount process. On Linux you can also `fusermount3 -u /mnt/projects`.

## Next

- [Mounting a drive](#/guides/mounting) — every option that matters
- [Excludes and the overlay](#/guides/excludes) — keep `node_modules` local
