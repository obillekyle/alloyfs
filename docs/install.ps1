# AlloyFS installer.
#
#   irm alloy.okyle.dev/install.ps1 | iex
#
# Environment:
#   $env:ALLOYFS_VERSION   install this tag instead of the latest (e.g. v0.1.1)
#   $env:ALLOYFS_INSTALL   install here instead of %LOCALAPPDATA%\Programs\alloyfs
#   $env:GITHUB_TOKEN      optional; raises the GitHub API rate limit

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

# --- what it needs to actually mount ---------------------------------------

Write-Host ''
if (-not (Test-Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp') -and
    -not (Test-Path 'HKLM:\SOFTWARE\WinFsp')) {
  Dim 'Note: WinFsp was not found, so mounting will not work yet.'
  Dim '      Install it from https://winfsp.dev'
}

Dim 'Config lives in %USERPROFILE%\.alloyfs -- separate from the binary, so'
Dim 'reinstalling or removing AlloyFS never touches your overlay or sync data.'
Write-Host ''
Write-Host 'Next:  alloyfs --help' -ForegroundColor Cyan
Dim '       https://alloy.okyle.dev/#/getting-started/first-mount'
