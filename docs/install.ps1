# AlloyFS installer.
#
#   irm alloy.okyle.dev/install.ps1 | iex
#
# Environment:
#   $env:ALLOYFS_VERSION      install this tag instead of the latest (e.g. v0.1.1)
#   $env:ALLOYFS_INSTALL      install here instead of %LOCALAPPDATA%\Programs\alloyfs
#   $env:GITHUB_TOKEN         optional; raises the GitHub API rate limit
#   $env:ALLOYFS_SKIP_WINFSP  do not offer to install the WinFsp driver
#   $env:ALLOYFS_NO_ELEVATE   never raise a UAC prompt; skip the driver instead
#   $env:WINFSP_VERSION       install this WinFsp tag instead of the latest
#
# AlloyFS itself installs PER-USER and needs no rights at all — that part
# prompts for nothing and is safe to run unattended.
#
# WinFsp is a kernel driver and does need rights. If this is not already
# elevated, one UAC prompt is raised for the DRIVER ALONE. A denied or
# impossible prompt is not a failure: the script says what is missing and
# carries on, leaving a working per-user alloyfs behind. Set
# ALLOYFS_NO_ELEVATE=1 to skip asking at all.
#
# install.cmd remains the front door that elevates once up front and then
# does everything without a second prompt.

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
  $body = (Invoke-WebRequest -Uri $sumUrl -UseBasicParsing).Content
  # GitHub serves .sha256 as application/octet-stream, and Windows PowerShell
  # 5.1 hands back a Byte[] for any content-type it does not read as text.
  # Calling .Trim() on that throws, the catch below swallows it, and the
  # installer reports "publishes no checksum" for a release that published
  # one -- so verification silently never ran on the shell this script
  # targets. Decode first; PowerShell 7 already gives a string.
  if ($body -is [byte[]]) { $body = [System.Text.Encoding]::ASCII.GetString($body) }
  $want = $body.Trim()
} catch {
  # Only a genuine 404 means "this release predates checksums" -- that is the
  # case worth continuing for, since refusing it would break rolling back to
  # an old release. ANY other failure (network, TLS, a decode that threw) must
  # be fatal: a verification step that quietly does not run is worse than no
  # verification step, because the output says it was considered.
  $code = $null
  try { $code = [int]$_.Exception.Response.StatusCode } catch { }
  if ($code -eq 404) {
    Write-Host "note: $version publishes no checksum; skipping verification" -ForegroundColor DarkGray
  } else {
    Die "could not fetch the checksum for ${asset}: $($_.Exception.Message)"
  }
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

  # /qn keeps msiexec silent. Whether it is ELEVATED depends on what we
  # already have:
  #
  # - Already admin (install.cmd's path, or an elevated shell): run it
  #   directly, exactly as before. No dialog, nothing to approve.
  # - Not admin: ask for the rights with -Verb RunAs. This raises one UAC
  #   prompt, for the kernel driver only — never for AlloyFS itself, which is
  #   a per-user install and needs no rights at all.
  #
  # The script used to skip the driver entirely when unelevated, which left
  # `irm … | iex` installing an alloyfs that could not mount anything and
  # saying so in a line most people scrolled past.
  #
  # Unattended safety is kept by what happens when the prompt cannot be
  # answered: a denied or impossible elevation throws, and the catch below
  # lands on exactly the old behaviour — say what is missing, carry on, leave
  # a working per-user alloyfs behind. Set ALLOYFS_NO_ELEVATE=1 to refuse
  # outright and go straight there.
  Write-Host '  Installing...'
  $msiArgs = @("/i", "`"$msi`"", "/qn", "/norestart")
  $elevate = -not (Test-Admin) -and -not $env:ALLOYFS_NO_ELEVATE
  if ($elevate) {
    Write-Host '  This needs administrator rights — approve the prompt to install the driver.' -ForegroundColor Cyan
  }
  try {
    $p = if ($elevate) {
      Start-Process msiexec -ArgumentList $msiArgs -Verb RunAs -Wait -PassThru
    } else {
      Start-Process msiexec -ArgumentList $msiArgs -Wait -PassThru
    }
  } catch {
    # The usual cause is a declined UAC prompt, or no interactive desktop to
    # show one on. Neither is an install failure: alloyfs is already in place.
    Dim "  The driver was not installed: $($_.Exception.Message)"
    Dim '  Install WinFsp from https://winfsp.dev, or re-run this from an elevated shell.'
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
} elseif ($env:ALLOYFS_NO_ELEVATE -and -not (Test-Admin)) {
  # Elevation refused by configuration. Say what is missing and how to fix it,
  # rather than leaving it to be discovered at the first mount.
  Write-Host ''
  Write-Host 'WinFsp is not installed, so mounting will not work yet.' -ForegroundColor Yellow
  Dim '  It is a kernel driver and installing it needs administrator rights,'
  Dim '  and ALLOYFS_NO_ELEVATE is set, so this installer did not ask. Either:'
  Dim ''
  Dim '    re-run this from an elevated shell,'
  Dim ''
  Dim '  or install it yourself from https://winfsp.dev'
} else {
  # Elevated already, or willing to ask for it — Install-WinFsp decides which
  # and raises at most one prompt, for the driver alone.
  Install-WinFsp
}

Dim 'Config lives in %USERPROFILE%\.alloyfs -- separate from the binary, so'
Dim 'reinstalling or removing AlloyFS never touches your overlay or sync data.'
Write-Host ''
Write-Host 'Next:  alloyfs --help' -ForegroundColor Cyan
Dim '       https://alloy.okyle.dev/#/getting-started/first-mount'
