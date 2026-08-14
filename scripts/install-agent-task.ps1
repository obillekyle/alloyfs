# Registers the drive-sync agent as a Windows Scheduled Task that starts at
# logon and restarts on failure. Run from an elevated PowerShell:
#   powershell -ExecutionPolicy Bypass -File scripts\install-agent-task.ps1
#   powershell ... -File scripts\install-agent-task.ps1 -Exe D:\tools\drive-sync.exe
# Remove with:
#   Unregister-ScheduledTask -TaskName "drive-sync-agent" -Confirm:$false
param(
    [string]$Exe = "C:\MyApps\drive-sync.exe",
    [string]$Arguments = "serve"
)

if (-not (Test-Path $Exe)) { throw "drive-sync.exe not found at $Exe (pass -Exe)" }

$action = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
    -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero)

Register-ScheduledTask -TaskName "drive-sync-agent" `
    -Action $action -Trigger $trigger -Settings $settings -Force
Write-Host "drive-sync-agent task registered (starts at next logon)."
Write-Host "Start it now with: Start-ScheduledTask -TaskName drive-sync-agent"
