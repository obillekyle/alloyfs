# AlloyFS installer.
#
#   irm alloy.okyle.dev/install.ps1 | iex
#
# Environment:
#   $env:ALLOYFS_VERSION      install this tag instead of the latest (e.g. v0.1.1)
#   $env:ALLOYFS_INSTALL      install here instead of %LOCALAPPDATA%\Programs\alloyfs
#   $env:GITHUB_TOKEN         optional; raises the GitHub API rate limit
#   $env:ALLOYFS_SKIP_WINFSP  do not offer to install the WinFsp driver
#   $env:WINFSP_VERSION       install this WinFsp tag instead of the latest
#
# This is the SILENT installer: it prompts for nothing, elevates nothing, and
# is safe to run unattended. AlloyFS installs per-user and needs no rights at
# all. WinFsp is a kernel driver and does need them, so it is installed only
# when this is already running elevated; otherwise the script says so and
# carries on.
#
# For an interactive install that asks for those rights once and then does
# everything, use install.cmd.

$ErrorActionPreference = 'Stop'

$Repo = 'obillekyle/alloyfs'
$InstallDir = if ($env:ALLOYFS_INSTALL) { $env:ALLOYFS_INSTALL }
              else { Join-Path $env:LOCALAPPDATA 'Programs\alloyfs' }

function Die($msg) { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }
function Dim($msg) { Write-Host $msg -ForegroundColor DarkGray }

# --- what are we running on -------------------------------------------------

# Only x86_64 is published. ARM64 Windows can run x64 under emulation, but a
# filesystem driver is not something to run emulated by surprise.
$arch = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($arch -ne 'X64') {
  Die "no build is published for $arch. Build from source: cargo build --release"
}
$target = 'x86_64-pc-windows-msvc'
$asset  = "alloyfs-$target.exe"

# --- auth -------------------------------------------------------------------

$token = if ($env:GITHUB_TOKEN) { $env:GITHUB_TOKEN } elseif ($env:GH_TOKEN) { $env:GH_TOKEN } else { $null }
$headers = @{ Accept = 'application/vnd.github+json'; 'User-Agent' = 'alloyfs-installer' }
if ($token) { $headers['Authorization'] = "Bearer $token" }

# --- which version ----------------------------------------------------------

$version = $env:ALLOYFS_VERSION
if (-not $version) {
  Write-Host 'Looking up the latest release...'
  try {
    $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    $version = $rel.tag_name
  } catch { $version = $null }
}

if (-not $version) {
  Die @"
could not reach the GitHub release API.

       Usually a network problem or an unauthenticated rate limit. A token
       raises the limit:

         `$env:GITHUB_TOKEN = 'ghp_...'

       Or skip the lookup entirely by naming the version:

         `$env:ALLOYFS_VERSION = 'v0.1.1'
         irm alloy.okyle.dev/install.ps1 | iex
"@
}

Write-Host "Installing AlloyFS $version ($target)" -ForegroundColor Cyan

# --- download ---------------------------------------------------------------

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("alloyfs-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
$out = Join-Path $tmp 'alloyfs.exe'

try {
  if ($token) {
    # With a token, use the API asset endpoint: the browser download URL
    # redirects to a signed link that rejects the Authorization header.
    $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/tags/$version" -Headers $headers
    $a = $rel.assets | Where-Object { $_.name -eq $asset } | Select-Object -First 1
    if (-not $a) { Die "release $version has no asset named $asset" }
    $dl = $headers.Clone(); $dl['Accept'] = 'application/octet-stream'
    Invoke-WebRequest -Uri $a.url -Headers $dl -OutFile $out
  } else {
    Invoke-WebRequest -Uri "https://github.com/$Repo/releases/download/$version/$asset" -OutFile $out
  }
} catch {
  Die "download failed: $($_.Exception.Message)"
}

# Verify we got a PE image and not an HTML error page. Without this the
# installer cheerfully writes a 404 page to your PATH and calls it alloyfs.exe.
$magic = [IO.File]::ReadAllBytes($out) | Select-Object -First 2
if ($magic[0] -ne 0x4D -or $magic[1] -ne 0x5A) {
  Die "downloaded file is not a Windows executable. This usually means the URL returned an error page."
}

# Checksum, when the release publishes one. Releases from before this
# existed have no .sha256 asset, and refusing those would break rolling back
# to them -- so a MISSING sum warns and continues, while a sum that is
# present and does not match is fatal. The magic-byte check above catches an
# error page; this catches a truncated download or a swapped asset.
$want = $null
try {
  $sumUrl = "https://github.com/$Repo/releases/download/$version/$asset.sha256"
  $want = (Invoke-WebRequest -Uri $sumUrl -UseBasicParsing).Content.Trim()
} catch {
  Write-Host "note: $version publishes no checksum; skipping verification" -ForegroundColor DarkGray
}
if ($want) {
  $got = (Get-FileHash -Path $out -Algorithm SHA256).Hash.ToLower()
  if ($got -ne $want.ToLower()) {
    Die "checksum mismatch for ${asset}: expected $want, got $got. Refusing to install."
  }
  Write-Host 'Checksum verified.' -ForegroundColor Green
}

# --- install ----------------------------------------------------------------

New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
$dest = Join-Path $InstallDir 'alloyfs.exe'

# A running alloyfs holds its own image open, so a plain copy fails with
# "being used by another process". Rename the old one aside instead -- Windows
# permits renaming a running executable, and the stale file is cleaned up on
# the next install.
if (Test-Path $dest) {
  $old = "$dest.old"
  Remove-Item $old -Force -ErrorAction SilentlyContinue
  try { Rename-Item $dest $old -Force } catch { }
}
Move-Item $out $dest -Force
Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "Installed to $dest" -ForegroundColor Green

# --- PATH -------------------------------------------------------------------

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath -notlike "*$InstallDir*") {
  [Environment]::SetEnvironmentVariable('Path', "$userPath;$InstallDir", 'User')
  $env:Path = "$env:Path;$InstallDir"
  Dim "Added $InstallDir to your user PATH. Open a new terminal for it to apply everywhere."
}

# --- WinFsp: the driver that makes a mount possible ------------------------
#
# AlloyFS on Windows is a WinFsp filesystem. Without the driver, `alloyfs` runs
# and every mount fails, which is a confusing place to leave someone who just
# ran an installer.
#
# THIS SCRIPT RAISES NO UAC PROMPT. It is the silent installer -- the target of
# `irm ... | iex` and of any unattended tooling -- and a silent installer that
# stops to ask a question is a script that hangs on a machine with nobody
# watching. install.cmd is the interactive front door: it asks once, up front,
# and runs this with the rights already in hand.
#
# Elevated or not, AlloyFS itself installs per-user and needs nothing. Only the
# WinFsp driver does, and without those rights this reports it rather than
# asking for them.

function Test-WinFsp {
  (Test-Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp') -or (Test-Path 'HKLM:\SOFTWARE\WinFsp')
}

function Test-Admin {
  $id = [Security.Principal.WindowsIdentity]::GetCurrent()
  (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(
    [Security.Principal.WindowsBuiltinRole]::Administrator)
}

function Install-WinFsp {
  $wfRepo = 'winfsp/winfsp'
  $wfTag  = $env:WINFSP_VERSION

  Write-Host ''
  Write-Host 'WinFsp is not installed. It is the filesystem driver AlloyFS mounts through.' -ForegroundColor Cyan

  try {
    $url = if ($wfTag) { "https://api.github.com/repos/$wfRepo/releases/tags/$wfTag" }
           else        { "https://api.github.com/repos/$wfRepo/releases/latest" }
    $wfRel = Invoke-RestMethod -Uri $url -Headers $headers
  } catch {
    Dim "  Could not reach the WinFsp release API: $($_.Exception.Message)"
    Dim '  Install it by hand from https://winfsp.dev and re-run nothing -- alloyfs is already installed.'
    return
  }

  # The asset carries a build number the tag does not ("v2.1" ->
  # winfsp-2.1.25156.msi), so it is matched rather than constructed. The
  # -notlike excludes winfsp-tests-*.msi, which is a different package.
  $msiAsset = $wfRel.assets |
    Where-Object { $_.name -like 'winfsp-*.msi' -and $_.name -notlike 'winfsp-tests-*' } |
    Select-Object -First 1
  if (-not $msiAsset) {
    Dim "  Release $($wfRel.tag_name) has no installer asset. Install from https://winfsp.dev"
    return
  }

  $wfTmp = Join-Path ([IO.Path]::GetTempPath()) ("winfsp-" + [Guid]::NewGuid())
  New-Item -ItemType Directory -Path $wfTmp | Out-Null
  $msi = Join-Path $wfTmp $msiAsset.name
  try {
    Write-Host "  Downloading $($msiAsset.name)..."
    Invoke-WebRequest -Uri $msiAsset.browser_download_url -OutFile $msi
  } catch {
    Dim "  Download failed: $($_.Exception.Message)"
    Remove-Item $wfTmp -Recurse -Force -ErrorAction SilentlyContinue
    return
  }

  # Checked before running it, because this installs a KERNEL DRIVER. A
  # signature check is the right test rather than a pinned hash: WinFsp
  # publishes a new build under the same tag scheme regularly, and a hash
  # baked in here would either go stale or quietly stop being verified.
  $sig = Get-AuthenticodeSignature $msi
  if ($sig.Status -ne 'Valid') {
    Dim "  Refusing to run it: the installer's signature is '$($sig.Status)', not Valid."
    Dim '  Install by hand from https://winfsp.dev'
    Remove-Item $wfTmp -Recurse -Force -ErrorAction SilentlyContinue
    return
  }
  Dim "  Signed by: $($sig.SignerCertificate.Subject -replace '^CN=([^,]+).*','$1')"

  # /qn is silent, and no -Verb RunAs: this script raises no UAC dialog. It is
  # reached by `irm … | iex` and by unattended tooling, and a silent installer
  # that stops to ask a question is not silent — it is a script that hangs on a
  # machine with nobody watching. install.cmd is the interactive front door and
  # elevates before any of this runs, so by the time msiexec is reached the
  # rights are already in hand.
  #
  # No INSTALLLEVEL, unlike .github/workflows/publish.yml, which passes 1000 to
  # pull in the WinFsp SDK. That is a BUILD dependency — the headers a release
  # build compiles against — and installing it here would put a development kit
  # on the machine of someone who only wants to mount a drive. The default
  # level installs the driver and runtime, which is all a mount needs.
  Write-Host '  Installing...'
  $msiArgs = @("/i", "`"$msi`"", "/qn", "/norestart")
  try {
    $p = Start-Process msiexec -ArgumentList $msiArgs -Wait -PassThru
  } catch {
    Dim "  msiexec could not be started: $($_.Exception.Message)"
    Remove-Item $wfTmp -Recurse -Force -ErrorAction SilentlyContinue
    return
  }
  Remove-Item $wfTmp -Recurse -Force -ErrorAction SilentlyContinue

  switch ($p.ExitCode) {
    0 {
      if (Test-WinFsp) { Write-Host '  WinFsp installed.' -ForegroundColor Green }
      else { Dim '  msiexec reported success but WinFsp is still not registered.' }
    }
    3010 {
      Write-Host '  WinFsp installed -- REBOOT REQUIRED before mounting will work.' -ForegroundColor Yellow
    }
    1602 { Dim '  Installation was cancelled. Install later from https://winfsp.dev' }
    default { Dim "  msiexec failed with exit code $($p.ExitCode). See https://winfsp.dev" }
  }
}

Write-Host ''
if (Test-WinFsp) {
  Dim 'WinFsp is installed.'
} elseif ($env:ALLOYFS_SKIP_WINFSP) {
  Dim 'Note: WinFsp was not found and ALLOYFS_SKIP_WINFSP is set, so mounting will not work yet.'
  Dim '      Install it from https://winfsp.dev'
} elseif (Test-Admin) {
  Install-WinFsp
} else {
  # Not elevated, and this script does not ask. Said plainly, with the two ways
  # out, because "mounting will not work" is a worse thing to discover at the
  # first mount than to be told here.
  Write-Host ''
  Write-Host 'WinFsp is not installed, so mounting will not work yet.' -ForegroundColor Yellow
  Dim '  It is a kernel driver and installing it needs administrator rights,'
  Dim '  which this installer does not ask for. Either:'
  Dim ''
  Dim '    curl -fsSL https://alloy.okyle.dev/install.cmd -o install.cmd && install.cmd'
  Dim ''
  Dim '  which asks once and does the rest, or install it yourself from'
  Dim '  https://winfsp.dev'
}

Dim 'Config lives in %USERPROFILE%\.alloyfs -- separate from the binary, so'
Dim 'reinstalling or removing AlloyFS never touches your overlay or sync data.'
Write-Host ''
Write-Host 'Next:  alloyfs --help' -ForegroundColor Cyan
Dim '       https://alloy.okyle.dev/#/getting-started/first-mount'
