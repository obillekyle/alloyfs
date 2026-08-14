# Registers the alloyfs agent as a Windows Scheduled Task that starts at
# logon and restarts on failure. Run from an elevated PowerShell:
#   powershell -ExecutionPolicy Bypass -File scripts\install-agent-task.ps1
#   powershell ... -File scripts\install-agent-task.ps1 -Exe D:\tools\alloyfs.exe
# Remove with:
#   Unregister-ScheduledTask -TaskName "alloyfs-agent" -Confirm:$false
param(
    [string]$Exe = "C:\MyApps\alloyfs.exe",
    [string]$Arguments = "serve"
)

if (-not (Test-Path $Exe)) { throw "alloyfs.exe not found at $Exe (pass -Exe)" }

$action = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
    -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero)

Register-ScheduledTask -TaskName "alloyfs-agent" `
    -Action $action -Trigger $trigger -Settings $settings -Force
Write-Host "alloyfs-agent task registered (starts at next logon)."
Write-Host "Start it now with: Start-ScheduledTask -TaskName alloyfs-agent"
