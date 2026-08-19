@echo off
REM AlloyFS installer, for cmd.exe.
REM
REM   curl -fsSL https://alloy.okyle.dev/install.cmd -o install.cmd && install.cmd
REM
REM A thin front door to install.ps1, which does the actual work. cmd has no
REM way to download and verify a release, so this hands over to PowerShell
REM rather than reimplementing any of it badly.
REM
REM Environment variables are inherited, so the ones install.ps1 documents work
REM here too:
REM
REM   set ALLOYFS_VERSION=v0.1.1
REM   set ALLOYFS_SKIP_WINFSP=1
REM   install.cmd
REM
REM AlloyFS installs per-user and needs no administrator rights. Installing the
REM WinFsp driver does, and raises a UAC prompt when it gets there. Running
REM this whole script "as administrator" is NOT the way to grant that: an
REM elevated cmd may carry a different account's profile, and AlloyFS would
REM install into it rather than into yours.

setlocal

where powershell >nul 2>&1
if errorlevel 1 (
  echo error: powershell was not found on PATH.
  echo        Install AlloyFS with the PowerShell installer instead:
  echo          irm alloy.okyle.dev/install.ps1 ^| iex
  exit /b 1
)

REM -NoProfile so a slow or broken user profile cannot derail the install.
REM -ExecutionPolicy Bypass applies to this one process only; it changes no
REM machine setting and leaves no policy behind.
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://alloy.okyle.dev/install.ps1 | iex"

exit /b %ERRORLEVEL%
