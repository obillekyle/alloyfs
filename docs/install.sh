#!/usr/bin/env sh
# AlloyFS installer.
#
#   curl -fsSL https://alloy.okyle.dev/install.sh | sh
#
# Environment:
#   ALLOYFS_VERSION   install this tag instead of the latest (e.g. v0.1.1)
#   ALLOYFS_INSTALL   install here instead of the default (~/.local/bin, or
#                     /usr/local/bin when running as root)
#   GITHUB_TOKEN      optional; raises the GitHub API rate limit
#
# POSIX sh on purpose: this runs before anything is installed, on whatever
# shell the machine happens to have.
set -eu

REPO="obillekyle/alloyfs"

# Where the binary lands, in three cases rather than one.
#
# `$HOME/.local/bin` alone got this wrong for the most common first command
# anyone runs. Under `sudo sh install.sh` — which is what someone types when
# they want alloyfs available to the whole machine, and what the FUSE note at
# the end of this script encourages — sudo sets HOME to /root, so the binary
# went to /root/.local/bin: a directory on nobody's PATH, unreadable by the
# user who ran the command, and reported as a success.
#
# Running as root therefore means /usr/local/bin, the system location that is
# already on every PATH. An explicit ALLOYFS_INSTALL still wins over both.
if [ -n "${ALLOYFS_INSTALL:-}" ]; then
  INSTALL_DIR="$ALLOYFS_INSTALL"
elif [ "$(id -u)" = 0 ]; then
  INSTALL_DIR=/usr/local/bin
else
  INSTALL_DIR="$HOME/.local/bin"
fi

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

# Whichever of two tags is newer by semver precedence.
#
# GitHub's `releases/latest` EXCLUDES prereleases, and this project's current
# line is published entirely as prereleases (1.0.0-alpha.N). So the lookup
# below answered v0.7.0 — a build from before the 1.0 line, which cannot speak
# the current wire protocol — and `alloyfs update` runs this very script, so on
# a machine already on an alpha the wrong answer was a DOWNGRADE rather than a
# missed upgrade.
#
# Numeric fields first, then a release outranks its own prereleases, then
# prerelease identifiers compare numerically when both are numeric.
newer_of() {
  a="$1"; b="$2"
  a_core=${a#v}; a_pre=''; case "$a_core" in *-*) a_pre=${a_core#*-}; a_core=${a_core%%-*};; esac
  b_core=${b#v}; b_pre=''; case "$b_core" in *-*) b_pre=${b_core#*-}; b_core=${b_core%%-*};; esac
  i=1
  while [ "$i" -le 3 ]; do
    x=$(printf '%s' "$a_core" | cut -d. -f"$i"); y=$(printf '%s' "$b_core" | cut -d. -f"$i")
    x=${x:-0}; y=${y:-0}
    case "$x$y" in *[!0-9]*) x=0; y=0 ;; esac
    if [ "$x" -gt "$y" ]; then printf '%s' "$a"; return; fi
    if [ "$x" -lt "$y" ]; then printf '%s' "$b"; return; fi
    i=$((i + 1))
  done
  if [ -z "$a_pre" ]; then printf '%s' "$a"; return; fi
  if [ -z "$b_pre" ]; then printf '%s' "$b"; return; fi
  # Both prereleases: identifier by identifier, left to right — NOT the
  # trailing one alone. Comparing only the last field says alpha.93 beats
  # beta.0, because it sees 93 against 0 and never looks at the channel. That
  # breaks the one upgrade that matters at a channel change: everyone on the
  # alpha line would sit there forever while beta shipped.
  i=1
  while :; do
    x=$(printf '%s' "$a_pre" | cut -d. -f"$i")
    y=$(printf '%s' "$b_pre" | cut -d. -f"$i")
    # Ran out of identifiers on both: equal, so neither is newer.
    if [ -z "$x" ] && [ -z "$y" ]; then printf '%s' "$b"; return; fi
    # Per semver, a larger set of identifiers wins when all before it match.
    if [ -z "$x" ]; then printf '%s' "$b"; return; fi
    if [ -z "$y" ]; then printf '%s' "$a"; return; fi
    if [ "$x" != "$y" ]; then
      case "$x$y" in
        # Either side non-numeric: compare as text. This also gives the
        # semver rule that a numeric identifier ranks below an alphanumeric
        # one, since digits sort before letters.
        *[!0-9]*) if [ "$x" \> "$y" ]; then printf '%s' "$a"; else printf '%s' "$b"; fi ;;
        *) if [ "$x" -gt "$y" ]; then printf '%s' "$a"; else printf '%s' "$b"; fi ;;
      esac
      return
    fi
    i=$((i + 1))
  done
}

tag_from() {
  api "https://api.github.com/repos/$REPO/$1" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -1
}

version="${ALLOYFS_VERSION:-}"
if [ -z "$version" ]; then
  bold "Looking up the latest release..."
  # Both, because they answer different questions: releases/latest is the
  # newest STABLE, the first page of releases is the newest thing published at
  # all. Whichever is genuinely newer wins.
  stable=$(tag_from "releases/latest") || true
  newest=$(tag_from "releases?per_page=1") || true
  if [ -n "$stable" ] && [ -n "$newest" ]; then
    version=$(newer_of "$newest" "$stable")
  else
    version="${stable:-$newest}"
  fi
  case "$version" in
    *-*) dim "The newest release is a prerelease ($version); installing it." ;;
  esac
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
