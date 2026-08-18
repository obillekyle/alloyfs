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

```powershell
# In an ELEVATED shell.
alloyfs service setup
alloyfs service add work --url ssh://azure/projects --mount P: --start
alloyfs service list
```

| Command | What it does |
|---|---|
| `setup` | One-time: check WinFsp, create the instance store and lock it down. Safe to re-run |
| `add <ID>` | Define an instance and register it to start at boot |
| `remove <ID>` | Stop it, unregister it, forget the definition |
| `start`/`stop`/`restart [ID]` | One instance, or every instance when no id is given |
| `list` | What is defined, its kind, its state and its target |
| `reset --confirm` | Remove every instance and its definition |

`add` takes either a mount or an agent:

```powershell
alloyfs service add work --url ssh://azure/projects --mount P: `
    --exclude node_modules --pin "*.lock" --auto-cache-max 2M --detect-conflicts

alloyfs service add agent --config C:\alloyfs.yml --tcp 0.0.0.0:7440
```

`--start` starts it immediately as well as at boot; without it, the service is
registered and waits for the next one. Instance ids become service names, file
names and command-line arguments, so they are limited to letters, digits,
dashes and underscores.

Definitions live in `C:\ProgramData\alloyfs\services\<id>.yml`, not in your
profile: the service runs as LocalSystem and cannot see `C:\Users\<name>`.
`setup` restricts that directory to SYSTEM and Administrators, because whoever
can write there chooses what a SYSTEM service launches.

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
`alloyfs mount …` there with no window. The child then runs as the user, in the
user's session, elevated — every property the mount needs.

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

`service add` has no `--remote-cmd`, so the far side must answer to plain
`alloyfs` over the non-interactive SSH channel. `ssh <host> alloyfs --version`
is the check that matters, and the usual reason it fails is the
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
