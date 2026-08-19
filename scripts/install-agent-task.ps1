# Registers the alloyfs agent as a Windows Scheduled Task that starts at
# logon and restarts on failure. Run from an elevated PowerShell:
#   powershell -ExecutionPolicy Bypass -File scripts\install-agent-task.ps1
#   powershell ... -File scripts\install-agent-task.ps1 -Exe D:\tools\alloyfs.exe
# Remove with:
#   Unregister-ScheduledTask -TaskName "alloyfs-agent" -Confirm:$false
#
# `alloyfs service` is the supported way to do this now; this remains for a
# scheduled task specifically.
param(
    # Where install.ps1 actually puts the binary. The previous default
    # (C:\MyApps\alloyfs.exe) predates that layout, so an official install
    # followed by this script threw "not found" before doing anything.
    [string]$Exe = "$env:LOCALAPPDATA\Programs\alloyfs\alloyfs.exe",
    [string]$Arguments = "serve"
)

if (-not (Test-Path $Exe)) {
    # Fall back to whatever is on PATH before giving up: a user who installed
    # somewhere else has it there, and the error is more useful for saying so.
    $onPath = (Get-Command alloyfs.exe -ErrorAction SilentlyContinue).Source
    if ($onPath) {
        $Exe = $onPath
    } else {
        throw "alloyfs.exe not found at $Exe, and not on PATH (pass -Exe)"
    }
}

$action = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
    -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero)

Register-ScheduledTask -TaskName "alloyfs-agent" `
    -Action $action -Trigger $trigger -Settings $settings -Force
Write-Host "alloyfs-agent task registered (starts at next logon)."
Write-Host "Start it now with: Start-ScheduledTask -TaskName alloyfs-agent"
