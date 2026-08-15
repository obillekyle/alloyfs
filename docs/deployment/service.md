# Running as a service

## Linux

The installer ships a systemd template, so one unit serves any number of users:

```bash
sudo systemctl enable --now alloyfs@yourname
systemctl status alloyfs@yourname
journalctl -u alloyfs@yourname -f
```

It runs `alloyfs serve` as that user, reading their `~/.alloyfs/config.yml`.
Restart on failure and start-at-boot are configured.

`sudo packaging/install.sh --user yourname --start` does the enable and start
for you.

## Windows

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-agent-task.ps1
Start-ScheduledTask -TaskName alloyfs-agent
```

That registers a Scheduled Task starting at logon, restarting on failure. Remove
it with:

```powershell
Unregister-ScheduledTask -TaskName "alloyfs-agent" -Confirm:$false
```

Point it at a different binary with `-Exe D:\tools\alloyfs.exe`.

## A note on mounts as services

Serving is a good fit for a service. **Mounting is less straightforward**, and
worth knowing before you try: drive letters on Windows are per-session, so a
drive mounted by a service account is not visible in your interactive session.
A UNC-style mount is usually the better answer there than a drive letter.

On Linux the same caution applies more mildly — a mount made in one namespace
may not be visible in another.
