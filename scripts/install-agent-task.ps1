# Registers the drive-sync agent as a Windows Scheduled Task that starts at
# logon and restarts on failure. Run from an elevated PowerShell:
#   powershell -ExecutionPolicy Bypass -File scripts\install-agent-task.ps1
# Remove with:
#   Unregister-ScheduledTask -TaskName "drive-sync-agent" -Confirm:$false

$exe = "C:\MyApps\drive-sync.exe"
if (-not (Test-Path $exe)) { throw "drive-sync.exe not found at $exe" }

$action = New-ScheduledTaskAction -Execute $exe -Argument "serve"
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
    -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero)

Register-ScheduledTask -TaskName "drive-sync-agent" `
    -Action $action -Trigger $trigger -Settings $settings -Force
Write-Host "drive-sync-agent task registered (starts at next logon)."
Write-Host "Start it now with: Start-ScheduledTask -TaskName drive-sync-agent"
