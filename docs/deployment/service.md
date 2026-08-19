# Running as a service

Two different jobs, and two tools that do not compete:

- **`alloyfs service`** — mounts and agents belonging to **one account**,
  registered with whatever the platform already uses to start things: the
  Windows service manager, or a systemd **user** unit. This is the supported way
  to have a drive come back after a reboot.
- **`service.sh` / `service.ps1`** — an **agent** on a machine with no
  interactive user. A system-wide systemd unit, or a Scheduled Task. No mounts,
  no per-account anything.

If a person logs into the machine and expects a drive, the first one is what you
want.

## `alloyfs service`

Describe the machine in the config once, then register one service that runs it:

```bash
# Linux — no root, and it refuses sudo.
alloyfs service setup
alloyfs service add alloyfs --start
alloyfs service list
```

```powershell
# Windows — in an ELEVATED shell.
alloyfs service setup
alloyfs service add alloyfs --start
alloyfs service list
```

That service runs `alloyfs start`: the agent from `server:`, then every mount
under `client.mounts:`. Adding a drive later is an edit to the config plus
`alloyfs service restart alloyfs` — there is no second place to register it.

| Command | What it does |
|---|---|
| `setup` | One-time: check the filesystem driver, create the instance store and restrict it. Safe to re-run |
| `add <ID>` | Register a service that runs part of this machine's config |
| `remove <ID>` | Stop it, unregister it, forget it |
| `start`/`stop`/`restart [ID]` | One instance, or every instance when no id is given |
| `list` | What is defined, its state, and the command it runs |
| `reset --confirm` | Remove every instance |

Every subcommand means the same thing on both platforms. What differs is
underneath:

| | Windows | Linux |
|---|---|---|
| Registered with | the service control manager | a systemd **user** unit in `~/.config/systemd/user` |
| Runs as | you, in your session — via a LocalSystem supervisor that borrows your token | you, in your session — the unit is already yours |
| Privilege | **needs an elevated shell** | **needs none, and refuses `sudo`** |
| Comes back | at boot | at **login**; at boot once the account lingers |
| Instance store | `C:\ProgramData\alloyfs\services` | `~/.alloyfs/services` |
| Filesystem driver | WinFsp | `/dev/fuse` + `fusermount3` |
| Logs | the service's own output | `journalctl --user -u alloyfs-<ID> -f` |

Three things on the Windows side have no Linux counterpart at all, because they
exist to work around Windows service isolation and there is nothing to work
around here: the elevation requirement, the WinFsp check, and the supervisor
that watches for logon/logoff to move a mount between sessions. `service run`,
the entry point the Windows service manager invokes, likewise does not exist on
Linux — systemd starts `alloyfs` itself.

### A service points at the config; it does not copy it

`add` records **which part of the config to run**, never the settings
themselves:

```bash
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
      at: "P:"          # /mnt/work on Linux
```

`--config PATH` pins a service to one file instead of letting it discover one.
On Windows the file is opened by the **logged-in user**, not by the service, so
it has to be a path that user can read.

`--start` starts it immediately as well as at boot; without it, the service is
registered and waits. Instance ids become service names, unit names, file names
and command-line arguments, so they are limited to letters, digits, dashes and
underscores.

`add` resolves the name before it registers anything, and refuses a name the
config does not define — listing the ones it does. That check is worth having
at the moment somebody is watching, because the same mistake at startup fails
where nobody is looking and reports nothing.

### Linux: a systemd user unit

`add` writes `~/.config/systemd/user/alloyfs-<ID>.service`, reloads the user
manager and enables it. Everything after that is ordinary systemd:

```bash
systemctl --user status alloyfs-work
journalctl --user -u alloyfs-work -f
```

The unit is **generated**, and rewritten on every `add` — edits to it do not
survive. It carries no mount settings, only an absolute `ExecStart` naming which
part of the config to run. Absolute because systemd searches no `PATH`: a unit
saying plain `alloyfs` fails at startup on a binary the installer put in
`~/.local/bin`.

**A user unit, not a system unit.** A system unit (`/etc/systemd/system` with
`User=%i`) buys one thing over this — it starts with nobody logged in — and
costs a great deal:

- It needs root for every operation, to register a preference root is not
  involved in acting on.
- It runs the process **outside** the user's session. No `XDG_RUNTIME_DIR`, no
  session bus, and no `SSH_AUTH_SOCK` — which is fatal for `ssh://` mounts, the
  most common kind.

A user unit already runs as the user, in their session, with their environment
and their keys. That is precisely what the Windows side spends a LocalSystem
service and a token swap to achieve, and on Linux it is the default. The one
case where a system unit is the right answer — an agent on a box nobody logs
into — is what `service.sh` is for, below.

**Lingering.** The cost of a user unit is that the user manager starts at login
and stops at logout, so `enable` means "at next login", not "at boot". On a
laptop that is usually what was wanted. On a server it is not, and the failure
is silent — the drive is simply not there. `setup` and `add` say which applies;
to change it:

```bash
sudo loginctl enable-linger "$USER"
```

That is the only command in the Linux half that wants root, and it is a property
of the account rather than of AlloyFS.

**Two lines in the generated unit are load-bearing**, and worth knowing about
before anybody edits a copy of it:

- `KillSignal=SIGINT`. AlloyFS unmounts on SIGINT. systemd's default stop signal
  is SIGTERM, which it does not handle — so with the default, `systemctl --user
  stop` kills the process with the filesystem still mounted and leaves a
  mountpoint that answers `ENOTCONN` to everything until somebody runs
  `fusermount -u` by hand.
- **No sandboxing directives.** `PrivateTmp`, `ProtectHome`, `ProtectSystem` and
  their relatives each give the unit a private mount namespace, and a mount made
  inside one is invisible to the shell that asked for it. `NoNewPrivileges` is a
  subtler version of the same trap: `fusermount3` is setuid root, which is how an
  unprivileged process is allowed to mount at all, and `NoNewPrivileges`
  suppresses setuid. Instances that mount therefore do not set it; agent-only
  instances, which never call `fusermount3`, do.

### Windows: everything except `list` needs an elevated shell

And nothing there will ask for elevation. Run it unelevated and it says so and
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

Linux refuses the opposite thing. `alloyfs service` under `sudo` would write
into **root's** `~/.config/systemd/user` and talk to root's user manager,
registering a mount for an account that never logs in — quietly, because every
individual step succeeds. So it stops and names the account to run as instead.

### Windows: a registered service does not mount from session 0

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

None of this exists on Linux. A systemd user unit is already in the right
session, and `Restart=on-failure` with `RestartSec=5` is the whole of the
supervision.

### Which config, read by whom

On Windows three processes are involved and only one of them reads the config:

| Process | Runs as | Reads |
|---|---|---|
| `alloyfs service add` | you, elevated | the config, to check the name exists |
| the service | LocalSystem, session 0 | `C:\ProgramData\alloyfs\services\<id>.yml` — a reference, nothing else |
| the child it launches | you, in your session | **the config**, and everything in it |

The service cannot read `C:\Users\<name>`, which is why the reference lives in
ProgramData and why `setup` restricts that directory to SYSTEM and
Administrators: whoever can write there chooses what a SYSTEM service launches.

One consequence worth knowing: if the elevated shell you run `add` in belongs
to a *different* account than the one that logs in, the two are reading
different configs. `--config PATH` pins both to the same file.

Linux has two processes and no such gap. `add` and the unit both run as you, so
the instance store sits in `~/.alloyfs/services` beside the config it points at,
restricted to the account that owns it. There is no privileged reader to keep
out and no second account to disagree with.

### Instances registered before this

A service defined by an older release carries its own copy of a mount
definition. Those keep working exactly as they were — they are read, never
rewritten, and they launch the command they always launched. `service list`
marks them `legacy`, because the config cannot override what they hold.

Converting one is a manual step for a reason: a url and a mountpoint do not say
which `client.mounts` entry they correspond to, and the process that reads the
file is the one that cannot open your config to find out.

```bash
# after describing the mount under client.mounts:
alloyfs service remove work
alloyfs service add work --mount work
```

### `ssh://` mounts authenticate as you

The mount runs as you, so your SSH keys and agent are the ones in scope — `add`
says so out loud when the url is `ssh://`, because the opposite arrangement is
the usual one for services.

Key-based auth has to work **non-interactively**: check with `ssh <host> true`.
A key with a passphrase and no agent will simply hang at startup.

On Linux there is one more step to know about. A user unit inherits the **user
manager's** environment, not your shell's, so `SSH_AUTH_SOCK` is set in a
terminal and absent in the unit unless something puts it there. The mount then
works when typed by hand and hangs when systemd starts it:

```bash
systemctl --user import-environment SSH_AUTH_SOCK
```

Neither `service add` nor `client.mounts:` has a `--remote-cmd`, so the far
side must answer to plain `alloyfs` over the non-interactive SSH channel.
`ssh <host> alloyfs --version` is the check that matters, and the usual reason
it fails is the
[`PATH` a non-interactive shell gets](#/getting-started/first-mount).

## The agent alone, on a machine nobody logs into

These install a **system-wide** agent. No mounts, one unit for the whole box,
starting at boot with no session involved — the case a user unit cannot cover.

### Linux

```bash
curl -fsSL https://alloy.okyle.dev/service.sh | sudo sh
```

That writes a systemd **template** unit to `/etc/systemd/system/alloyfs@.service`,
enables it for the user who ran `sudo`, and starts it. One unit serves any number
of users — the instance name is the user whose config and exports it serves:

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
the unit, for the same reason `alloyfs service` does.

Remove it with `sudo systemctl disable --now alloyfs@yourname`.

**Do not use this for mounts.** A system unit runs outside the user's session,
so an `ssh://` mount has no agent to authenticate with, and it needs root for
every change. `alloyfs service` is the answer for anything that mounts.

### Windows

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

Linux has a milder version of the same caution, and it bites in one specific
place: a mount made inside a private mount namespace is invisible outside it.
That is why the generated unit sets no sandboxing directives, and why an
`alloyfs mount` run under a hardened unit somebody wrote by hand can look
perfectly healthy while the mountpoint stays empty.
