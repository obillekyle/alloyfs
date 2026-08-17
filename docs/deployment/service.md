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

## Windows

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

## A note on mounts as services

Serving is a good fit for a service. **Mounting is less straightforward**, and
worth knowing before you try: drive letters on Windows are per-session, so a
drive mounted by a service account is not visible in your interactive session.
A UNC-style mount is usually the better answer there than a drive letter.

On Linux the same caution applies more mildly — a mount made in one namespace
may not be visible in another.
