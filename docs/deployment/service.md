# Running as a service

## Linux

```bash
curl -fsSL https://alloy.okyle.dev/service.sh | sudo sh
```

That writes a systemd template unit, enables it for the user who ran `sudo`,
and starts it. One unit serves any number of users — the instance name is the
user whose config and exports it serves:

```bash
systemctl status alloyfs@yourname
journalctl -u alloyfs@yourname -f
```

It runs `alloyfs serve` as that user, reading their `~/.alloyfs/config.yml`.
Restart on failure and start-at-boot are configured.

Options need the script on disk first, since a piped script cannot take
arguments:

```bash
curl -fsSL https://alloy.okyle.dev/service.sh | sudo sh -s -- --user alice
```

`--exe /path/to/alloyfs` overrides binary detection, `--no-start` enables
without starting. It resolves the binary to an absolute path and bakes it into
the unit, because systemd does not search a user's `PATH` — a binary left in
`~/.local/bin` by the installer would otherwise produce a unit that fails at
boot.

Remove it with `sudo systemctl disable --now alloyfs@yourname`.

## Windows: the agent, as a scheduled task

```powershell
irm alloy.okyle.dev/service.ps1 | iex
```

That registers a Scheduled Task starting at logon, restarting on failure. It
finds the binary on your `PATH`, or where the installer put it. Remove it with:

```powershell
Unregister-ScheduledTask -TaskName "alloyfs-agent" -Confirm:$false
```

For options, download it first:

```powershell
irm alloy.okyle.dev/service.ps1 -OutFile service.ps1
.\service.ps1 -Exe D:\tools\alloyfs.exe
```

Both scripts warn if the user has no config, because an agent with no exports
starts cleanly, serves nothing, and looks healthy.

## Windows: `alloyfs service`

Mounts and agents registered with the Windows service manager, starting on
their own with no terminal window at logon. This is the supported way to have a
drive letter come back after a reboot.

Describe the machine in the config once, then register one service that runs
it:

```powershell
# In an ELEVATED shell.
alloyfs service setup
alloyfs service add alloyfs --start
alloyfs service list
```

That service runs `alloyfs start`: the agent from `server:`, then every mount
under `client.mounts:`. Adding a drive later is an edit to the config plus
`alloyfs service restart alloyfs` — there is no second place to register it.

| Command | What it does |
|---|---|
| `setup` | One-time: check WinFsp, create the instance store and lock it down. Safe to re-run |
| `add <ID>` | Register a service that runs part of this machine's config |
| `remove <ID>` | Stop it, unregister it, forget it |
| `start`/`stop`/`restart [ID]` | One instance, or every instance when no id is given |
| `list` | What is defined, its state, and the command it runs |
| `reset --confirm` | Remove every instance |

### A service points at the config; it does not copy it

`add` records **which part of the config to run**, never the settings
themselves:

```powershell
alloyfs service add alloyfs                  # alloyfs start
alloyfs service add drives --mounts-only     # alloyfs start --mounts-only
alloyfs service add agent  --server-only     # alloyfs start --server-only
alloyfs service add work   --mount work      # alloyfs mount work
```

`--mount NAME` names an entry under `client.mounts:`, exactly as
`alloyfs mount <NAME>` does — so the url, mountpoint, excludes, pins and cache
sizes are read from the config at launch and nowhere else. `service list`
prints the command each service runs, and running that command by hand is how
you reproduce anything that goes wrong.

Everything a mount needs therefore lives in one file:

```yaml
client:
  exclude: [node_modules]
  mounts:
    work:
      url: ssh://azure/projects
      at: "P:"
```

`--config PATH` pins a service to one file instead of letting it discover one.
The file is opened by the **logged-in user**, not by the service, so it has to
be a path that user can read.

`--start` starts it immediately as well as at boot; without it, the service is
registered and waits for the next one. Instance ids become service names, file
names and command-line arguments, so they are limited to letters, digits,
dashes and underscores.

`add` resolves the name before it registers anything, and refuses a name the
config does not define — listing the ones it does. That check is worth having
at the moment somebody is watching, because the same mistake at boot fails
inside a process with no window and reports nothing.

### Which config, read by whom

Three processes are involved and only one of them reads the config:

| Process | Runs as | Reads |
|---|---|---|
| `alloyfs service add` | you, elevated | the config, to check the name exists |
| the service | LocalSystem, session 0 | `C:\ProgramData\alloyfs\services\<id>.yml` — a reference, nothing else |
| the child it launches | you, in your session | **the config**, and everything in it |

The service cannot read `C:\Users\<name>`, which is why the reference lives in
ProgramData and why `setup` restricts that directory to SYSTEM and
Administrators: whoever can write there chooses what a SYSTEM service launches.
The child has no such problem — it runs as you, with your environment and your
home directory — so it resolves `~/.alloyfs/config.yml` the same way it would
if you had typed the command yourself.

One consequence worth knowing: if the elevated shell you run `add` in belongs
to a *different* account than the one that logs in, the two are reading
different configs. `--config PATH` pins both to the same file.

### Instances registered before this

A service defined by an older release carries its own copy of a mount
definition. Those keep working exactly as they were — they are read, never
rewritten, and they launch the command they always launched. `service list`
marks them `legacy`, because the config cannot override what they hold.

Converting one is a manual step for a reason: a url and a mountpoint do not say
which `client.mounts` entry they correspond to, and the process that reads the
file is the one that cannot open your config to find out.

```powershell
# after describing the mount under client.mounts:
alloyfs service remove work
alloyfs service add work --mount work
```

### Everything except `list` needs an elevated shell

And nothing here will ask for elevation. Run it unelevated and it says so and
stops.

A tool that silently re-launches itself through `runas` teaches people to click
through a UAC dialog they did not open, which is a worse habit than an error
message. Registering something that runs at boot is not a thing to arrange on
somebody's behalf behind a prompt they did not ask for.

Elevation is not cosmetics either. Without it a mount cannot register with the
Windows Mount Manager, and a session-local drive letter breaks
`GetFinalPathNameByHandle` round-trips — which is the reason **bun and similar
tools report `ENOENT` for files that plainly exist**. See
[Mounting a drive](#/guides/mounting) for the whole story.

`list` is deliberately readable from any shell: "what is registered" is a
question worth answering without a second terminal.

### A registered service does not mount from session 0

It launches the mount **into the interactive session, as the logged-in user**.

The service process itself runs as LocalSystem in session 0 and does nothing
useful. It waits for a console session, takes that user's token, swaps it for
the un-filtered token behind it (their elevated one), and starts a plain
`alloyfs start` or `alloyfs mount <name>` there with no window — in that user's
home directory, with that user's environment. The child then runs as the user,
in the user's session, elevated, reading the user's config — every property the
mount needs.

Mounting in session 0 instead breaks three separate things:

- **File access.** The WinFsp backend reports no security descriptor and lets
  WinFsp synthesize one from whoever mounted. Mounted by SYSTEM, every file
  carries a SYSTEM-derived descriptor, and an ordinary non-elevated process —
  bun, an editor, Explorer — is access-checked against it.
- **Visibility.** A drive letter created without the Mount Manager is
  session-local, so a session-0 mount is not on the desktop at all.
- **Credentials.** An `ssh://` mount running as SYSTEM looks for SYSTEM's keys
  and agent, which do not exist.

Because the child is an ordinary CLI invocation — the same one `service list`
prints — anything that goes wrong can be reproduced by hand, without the
service in the way.

What the supervisor does with it:

- **At boot, before anyone logs in**, there is no session to mount into. That is
  not a failure; it waits quietly and mounts at logon.
- **Logon, logoff and fast user switching** all change which session the mount
  belongs in, so the child is dropped and relaunched. Restarting a working mount
  costs a reconnect; leaving one pointed at a dead session costs the drive.
- **A mount that exits is restarted.** One that dies within ten seconds is
  failing rather than finishing, so it backs off instead of spinning.

### `ssh://` mounts authenticate as you

The mount runs as the logged-in user, so their SSH keys and agent are the ones
in scope — `add` says so out loud when the url is `ssh://`, because the opposite
arrangement is the usual one for services.

Key-based auth has to work **non-interactively**: check with `ssh <host> true`.
A key with a passphrase and no agent will simply hang at boot.

Neither `service add` nor `client.mounts:` has a `--remote-cmd`, so the far
side must answer to plain `alloyfs` over the non-interactive SSH channel.
`ssh <host> alloyfs --version` is the check that matters, and the usual reason
it fails is the
[`PATH` a non-interactive shell gets](#/getting-started/first-mount).

## Why mounting as a service needs this at all

Serving is a straightforward fit for a service. Mounting is not, and the reason
is worth knowing: **drive letters on Windows are per-session**. A drive mounted
by a service account is not visible in your interactive session, no matter how
healthy the service looks.

There is no UNC-path escape hatch here. AlloyFS mounts as a *disk* filesystem
with an empty volume prefix, deliberately — plain drive letter mounts then work
without the network-provider machinery — so there is no `\\alloyfs\...` name to
reach the volume by. The answer is `alloyfs service`, which sidesteps the
problem rather than working around it: the mount is created *in* your session,
by a process running as you, so it is an ordinary drive letter that happens to
have been started for you.

On Linux the same caution applies more mildly — a mount made in one namespace
may not be visible in another.
