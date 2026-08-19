@echo off
REM AlloyFS installer for cmd.exe -- the interactive front door.
REM
REM   curl -fsSL https://alloy.okyle.dev/install.cmd -o install.cmd && install.cmd
REM
REM Asks for administrator rights ONCE, up front, then runs install.ps1 with
REM them. install.ps1 is the silent installer and prompts for nothing; the only
REM thing needing elevation -- the WinFsp driver -- is already covered by the
REM time it runs.
REM
REM Consent belongs before the work, not halfway through a download, and a
REM silent installer that stops to ask a question is not silent. That is the
REM whole reason these are two files.
REM
REM Environment variables pass through and mean what install.ps1 documents:
REM
REM   set ALLOYFS_VERSION=v0.1.1
REM   set ALLOYFS_SKIP_WINFSP=1
REM   install.cmd

setlocal EnableExtensions

REM Resolved BEFORE elevating, and carried into the elevated session. This is
REM the part that is easy to get wrong: UAC on a standard user's machine
REM prompts for a DIFFERENT account, and the elevated process then has that
REM account's %LOCALAPPDATA%. Resolving first means AlloyFS lands in the
REM profile of whoever ran this, not in whichever administrator authorised it.
if not defined ALLOYFS_INSTALL set "ALLOYFS_INSTALL=%LOCALAPPDATA%\Programs\alloyfs"

where powershell >nul 2>&1
if errorlevel 1 (
  echo error: powershell was not found on PATH.
  exit /b 1
)

REM `net session` fails unelevated and succeeds elevated -- the standard cmd
REM test, needing nothing installed. Already elevated: no second prompt.
net session >nul 2>&1
if not errorlevel 1 goto :direct

echo Requesting administrator rights ^(to install the WinFsp driver^)...

REM The elevated command is passed BASE64-ENCODED, and that is not decoration.
REM `Start-Process -ArgumentList` in Windows PowerShell joins its array with
REM spaces and quotes nothing, so a redirected profile at
REM "D:\Some Other\Programs\alloyfs" arrives at the child as "D:\Some" and the
REM install silently lands somewhere nobody asked for. Measured, not guessed.
REM -EncodedCommand takes a single token with no spaces or quotes in it, so
REM there is nothing left for the argument parser to mangle.
powershell -NoProfile -Command ^
  "$q=[char]39; $i='$env:ALLOYFS_INSTALL='+$q+$env:ALLOYFS_INSTALL+$q+'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; irm https://alloy.okyle.dev/install.ps1 | iex; Write-Host; Read-Host '+$q+'Press Enter to close'+$q; $e=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($i)); try { $p=Start-Process powershell -Verb RunAs -Wait -PassThru -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-EncodedCommand',$e; exit $p.ExitCode } catch { exit 1223 }"

if errorlevel 1223 (
  echo.
  echo Administrator rights were declined.
  echo.
  echo AlloyFS itself needs none -- it installs into your own profile. Only the
  echo WinFsp driver does. To install AlloyFS alone:
  echo.
  echo   powershell -NoProfile -Command "irm alloy.okyle.dev/install.ps1 ^| iex"
  echo.
  echo then install WinFsp yourself from https://winfsp.dev
  exit /b 1
)
exit /b %ERRORLEVEL%

:direct
REM -NoProfile so a slow or broken profile cannot derail the install.
REM -ExecutionPolicy Bypass applies to this process only; it changes no machine
REM setting and leaves no policy behind.
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://alloy.okyle.dev/install.ps1 | iex"
exit /b %ERRORLEVEL%
