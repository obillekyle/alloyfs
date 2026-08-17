# Registers the AlloyFS agent as a Windows Scheduled Task that starts at logon
# and restarts on failure.
#
#   irm alloy.okyle.dev/service.ps1 | iex
#
# With options, download first (a piped script cannot take arguments):
#   irm alloy.okyle.dev/service.ps1 -OutFile service.ps1
#   .\service.ps1 -Exe D:\tools\alloyfs.exe
#
# Remove it with:
#   Unregister-ScheduledTask -TaskName "alloyfs-agent" -Confirm:$false
#
# ASCII only, deliberately: PowerShell 5.1 reads a .ps1 without a BOM as ANSI,
# so a stray smart quote here becomes a parse error on somebody else's machine.

param(
  [string]$Exe = "",
  [string]$Arguments = "serve",
  [string]$TaskName = "alloyfs-agent"
)

$ErrorActionPreference = 'Stop'

function Fail($m) { Write-Host "error: $m" -ForegroundColor Red; exit 1 }
function Dim($m) { Write-Host $m -ForegroundColor DarkGray }

# Find the binary the same way a user would, rather than assuming one layout:
# whatever is on PATH first, then where install.ps1 puts it.
if (-not $Exe) {
  $onPath = Get-Command alloyfs -ErrorAction SilentlyContinue
  if ($onPath) {
    $Exe = $onPath.Source
  } else {
    $default = Join-Path $env:LOCALAPPDATA 'Programs\alloyfs\alloyfs.exe'
    if (Test-Path $default) { $Exe = $default }
  }
}
if (-not $Exe) {
  Fail "cannot find alloyfs.exe. Install it first (irm alloy.okyle.dev/install.ps1 | iex), or pass -Exe."
}
if (-not (Test-Path $Exe)) { Fail "no such file: $Exe" }
$Exe = (Resolve-Path $Exe).Path

# A task registered for the current user needs no elevation; a task that runs
# before anyone logs in does. Registering per-user is the honest default here,
# because the agent serves that user's config and their files.
Dim "Registering '$TaskName' to run: $Exe $Arguments"

# Serving needs a config with at least one export. Saying so now beats a task
# that registers cleanly and then exits on every logon.
$configs = @(
  (Join-Path $env:USERPROFILE '.alloyfs\config.yml'),
  (Join-Path $env:USERPROFILE '.alloyfs\config.yaml'),
  (Join-Path $env:USERPROFILE '.alloyfs\config.json'),
  (Join-Path (Split-Path $Exe) 'alloyfs.yml')
)
if (-not ($configs | Where-Object { Test-Path $_ })) {
  Write-Host "warning: no config found. The task will start but serve nothing." -ForegroundColor Yellow
  Write-Host "         Create one with: alloyfs init --global" -ForegroundColor Yellow
}

$action = New-ScheduledTaskAction -Execute $Exe -Argument $Arguments
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet `
  -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
  -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero)

Register-ScheduledTask -TaskName $TaskName `
  -Action $action -Trigger $trigger -Settings $settings -Force | Out-Null

Write-Host ""
Write-Host "Registered '$TaskName' (starts at next logon)." -ForegroundColor Green
Write-Host "  start now:  Start-ScheduledTask -TaskName $TaskName"
Write-Host "  status:     Get-ScheduledTask -TaskName $TaskName"
Write-Host "  remove:     Unregister-ScheduledTask -TaskName $TaskName -Confirm:`$false"
Write-Host ""
Dim "Note: this serves exports. MOUNTING as a service is a different problem --"
Dim "drive letters are per-session, so a drive mounted by a service account is"
Dim "not visible in your interactive session."
