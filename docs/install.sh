#!/usr/bin/env sh
# AlloyFS installer.
#
#   curl -fsSL https://alloy.okyle.dev/install.sh | sh
#
# Environment:
#   ALLOYFS_VERSION   install this tag instead of the latest (e.g. v0.1.1)
#   ALLOYFS_INSTALL   install here instead of ~/.local/bin
#   GITHUB_TOKEN      optional; raises the GitHub API rate limit
#
# POSIX sh on purpose: this runs before anything is installed, on whatever
# shell the machine happens to have.
set -eu

REPO="obillekyle/alloyfs"
INSTALL_DIR="${ALLOYFS_INSTALL:-$HOME/.local/bin}"

red() { printf '\033[31m%s\033[0m\n' "$1" >&2; }
dim() { printf '\033[2m%s\033[0m\n' "$1"; }
bold() { printf '\033[1m%s\033[0m\n' "$1"; }

die() {
  red "error: $1"
  exit 1
}

# --- what are we running on -------------------------------------------------

os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Linux) ;;
  Darwin)
    die "macOS is not supported: AlloyFS mounts through FUSE on Linux and
       WinFsp on Windows, and neither applies here. The agent side would
       work, but there is no macOS build to install."
    ;;
  *) die "unsupported system: $os" ;;
esac

# Only x86_64 is published. Refusing loudly beats installing a binary that
# cannot run and failing with 'exec format error' later.
case "$arch" in
  x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
  aarch64 | arm64)
    die "no aarch64 build is published yet. Build from source:
       cargo build --release"
    ;;
  *) die "unsupported architecture: $arch" ;;
esac

# --- how do we fetch --------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$@"; }
  fetch_to() { curl -fsSL -o "$1" "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO- "$@"; }
  fetch_to() { wget -qO "$1" "$2"; }
else
  die "neither curl nor wget is available"
fi

auth_header=""
if [ -n "${GITHUB_TOKEN:-}" ]; then
  auth_header="Authorization: Bearer $GITHUB_TOKEN"
elif [ -n "${GH_TOKEN:-}" ]; then
  auth_header="Authorization: Bearer $GH_TOKEN"
fi

api() {
  if [ -n "$auth_header" ]; then
    fetch -H "Accept: application/vnd.github+json" -H "$auth_header" "$1"
  else
    fetch -H "Accept: application/vnd.github+json" "$1"
  fi
}

# --- which version ----------------------------------------------------------

version="${ALLOYFS_VERSION:-}"
if [ -z "$version" ]; then
  bold "Looking up the latest release..."
  version=$(api "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -1) || true
fi

if [ -z "$version" ]; then
  die "could not reach the GitHub release API.

       Usually a network problem or an unauthenticated rate limit. A token
       raises the limit:

         export GITHUB_TOKEN=ghp_...

       Or skip the lookup entirely by naming the version:

         ALLOYFS_VERSION=v0.1.1 curl -fsSL https://alloy.okyle.dev/install.sh | sh"
fi

asset="alloyfs-$target"
url="https://github.com/$REPO/releases/download/$version/$asset"

bold "Installing AlloyFS $version ($target)"

# --- download ---------------------------------------------------------------

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t alloyfs)
trap 'rm -rf "$tmp"' EXIT
out="$tmp/alloyfs"

if [ -n "$auth_header" ]; then
  # `Accept: application/octet-stream` on the API asset URL returns the bytes.
  api_url=$(api "https://api.github.com/repos/$REPO/releases/tags/$version" \
    | tr '{' '\n' | grep "\"name\": *\"$asset\"" | \
      sed -n 's/.*"url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
  [ -n "$api_url" ] || die "release $version has no asset named $asset"
  curl -fsSL -H "$auth_header" -H "Accept: application/octet-stream" \
    -o "$out" "$api_url" || die "download failed"
else
  fetch_to "$out" "$url" || die "download failed: $url"
fi

# Verify we got a binary and not an HTML error page. Without this the installer
# happily writes a 404 page to your PATH and names it alloyfs.
magic=$(dd if="$out" bs=4 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
case "$magic" in
  7f454c46) ;; # \x7fELF
  *) die "downloaded file is not a Linux executable (got magic '$magic').
       This usually means the URL returned an error page." ;;
esac

# Checksum, when the release publishes one. Releases from before this
# existed have no .sha256 asset, and refusing those would break rolling
# back to them — so a MISSING sum warns and continues, while a sum that is
# present and does not match is fatal. The magic-byte check above catches
# an error page; this catches a truncated download or a swapped asset.
sums=$(command -v sha256sum || command -v shasum || true)
if [ -n "$sums" ]; then
  want=$(fetch "$url.sha256" 2>/dev/null | tr -d ' \r\n' || true)
  if [ -n "$want" ]; then
    case "$sums" in
      *shasum) got=$("$sums" -a 256 "$out" | awk '{print $1}') ;;
      *)       got=$("$sums" "$out" | awk '{print $1}') ;;
    esac
    [ "$got" = "$want" ] || die "checksum mismatch for $asset
       expected $want
       got      $got
       Refusing to install. Try again; if it persists, the release asset may be corrupt."
    bold "Checksum verified."
  else
    printf 'note: %s publishes no checksum; skipping verification\n' "$version" >&2
  fi
else
  printf 'note: no sha256sum/shasum on PATH; skipping checksum verification\n' >&2
fi

# --- install ----------------------------------------------------------------

mkdir -p "$INSTALL_DIR"
mv "$out" "$INSTALL_DIR/alloyfs"
chmod +x "$INSTALL_DIR/alloyfs"

bold "Installed to $INSTALL_DIR/alloyfs"

# --- PATH -------------------------------------------------------------------

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    case "${SHELL:-}" in
      */zsh) profile="$HOME/.zshrc" ;;
      */fish) profile="$HOME/.config/fish/config.fish" ;;
      *) profile="$HOME/.bashrc" ;;
    esac
    printf '\n'
    dim "$INSTALL_DIR is not on your PATH. Add it with:"
    if [ "${profile##*/}" = "config.fish" ]; then
      printf '  fish_add_path %s\n' "$INSTALL_DIR"
    else
      printf '  echo '\''export PATH="%s:$PATH"'\'' >> %s\n' "$INSTALL_DIR" "$profile"
    fi
    ;;
esac

# --- what it needs to actually mount ---------------------------------------

printf '\n'
if [ ! -e /dev/fuse ]; then
  dim "Note: /dev/fuse is missing, so mounting will not work yet."
  dim "      sudo apt install fuse3     (or your distribution's equivalent)"
fi

dim "Config lives in ~/.alloyfs — separate from the binary, so reinstalling"
dim "or removing AlloyFS never touches your overlay or sync baselines."
printf '\n'
bold "Next:  alloyfs --help"
dim "       https://alloy.okyle.dev/#/getting-started/first-mount"
