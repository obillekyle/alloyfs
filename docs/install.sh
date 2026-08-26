#!/usr/bin/env sh
# AlloyFS installer.
#
#   curl -fsSL https://alloy.okyle.dev/install.sh | sh
#
# Environment:
#   ALLOYFS_VERSION   install this tag instead of resolving one (e.g. v0.1.1)
#   ALLOYFS_CHANNEL   alpha | beta | rc | stable | latest. With none of these,
#                     the channel the INSTALLED binary is on is the one that
#                     is followed, and a fresh machine gets the newest release
#                     there is.
#   ALLOYFS_INSTALL   install here instead of the default (~/.local/bin, or
#                     /usr/local/bin when running as root)
#   GITHUB_TOKEN      optional; raises the GitHub API rate limit
#
# POSIX sh on purpose: this runs before anything is installed, on whatever
# shell the machine happens to have.
set -eu

REPO="AlloyFS/alloyfs"

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
# Needed because GitHub's `releases/latest` EXCLUDES prereleases. On a project
# whose current line is 1.0.0-alpha.N it answers with the last STABLE — here
# v0.7.0, which predates the 1.0 line entirely and cannot speak the current
# wire protocol. So `curl … | sh`, the install command in the docs, handed
# every new user a build from before the rewrite and called it a success.
#
# Same rule `alloyfs update` applies in Rust (`is_newer`, commands/update.rs):
# numeric fields first, then a release outranks its own prereleases, then
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
  # Equal cores: a release beats its own prereleases.
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

# Every tag on a release page, newest first.
#
# `tr ',' '\n'` first because the API answers one enormous line and `sed`
# works a line at a time: without the split, the greedy `.*` matches the LAST
# tag_name in the whole document and every other release is invisible.
tags_from() {
  api "https://api.github.com/repos/$REPO/$1" \
    | tr ',' '\n' \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

# When one tag was published. Fetched per tag rather than paired up from the
# list, because pairing means trusting that no release BODY contains the text
# `"published_at"` — and bodies are free-form markdown. One tag, one object,
# no ambiguity. Only called when version order cannot decide, which is rare.
published_of() {
  api "https://api.github.com/repos/$REPO/releases/tags/$1" \
    | tr ',' '\n' \
    | sed -n 's/.*"published_at"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -1
}

# Which rung of the stability ladder a tag sits on: 0 alpha, 1 beta, 2 rc/pre,
# 3 stable. Empty for a prerelease naming something else — there is no rung to
# promote onto, and guessing which one it resembles would be worse than
# ignoring it.
#
# rc and pre are deliberately one rung: cutver's config maps both onto the
# same channel, so splitting them here would invent a distinction the releases
# themselves do not make.
rung_of() {
  case "${1#v}" in
    *-alpha|*-alpha.*) printf '0' ;;
    *-beta|*-beta.*) printf '1' ;;
    *-rc|*-rc.*|*-pre|*-pre.*|*-prerelease|*-prerelease.*) printf '2' ;;
    *-*) printf '' ;;
    *) printf '3' ;;
  esac
}

rung_name() {
  case "$1" in
    0) printf 'alpha' ;;
    1) printf 'beta' ;;
    2) printf 'rc' ;;
    3) printf 'stable' ;;
  esac
}

# Is $1 >= $2 comparing CORES only, ignoring prerelease identifiers?
#
# This is what "stable has caught up" means, and it needs its own comparison
# because semver precedence cannot express it: 1.0.0 outranks every 1.0.0-*,
# but so does it outrank nothing at all — while 0.8.1 against 1.0.0-alpha.94
# must NOT count as caught up. Cores answer that; whole-version order does not.
core_at_least() {
  a=${1#v}; a=${a%%-*}
  b=${2#v}; b=${b%%-*}
  i=1
  while [ "$i" -le 3 ]; do
    x=$(printf '%s' "$a" | cut -d. -f"$i"); y=$(printf '%s' "$b" | cut -d. -f"$i")
    x=${x:-0}; y=${y:-0}
    case "$x$y" in *[!0-9]*) x=0; y=0 ;; esac
    if [ "$x" -gt "$y" ]; then return 0; fi
    if [ "$x" -lt "$y" ]; then return 1; fi
    i=$((i + 1))
  done
  return 0 # equal cores: stable has arrived at the line you were following
}

# The version already on this machine, if any — the thing that decides which
# channel is being followed. An install with nothing to upgrade has no channel,
# which is a different case and handled as one.
installed_version() {
  for c in "$INSTALL_DIR/alloyfs" alloyfs; do
    p=$(command -v "$c" 2>/dev/null) || continue
    v=$("$p" --version 2>/dev/null | awk 'NR==1 {print $2}') || continue
    if [ -n "$v" ]; then printf 'v%s' "${v#v}"; return; fi
  done
}

version="${ALLOYFS_VERSION:-}"
if [ -z "$version" ]; then
  bold "Looking up the latest release..."

  # The newest tag on each rung, from one request. The list arrives
  # newest-first, so the first tag seen for a rung is that rung's latest.
  alpha_tag=''; beta_tag=''; rc_tag=''; stable_tag=''
  for t in $(tags_from "releases?per_page=100"); do
    case "$(rung_of "$t")" in
      0) [ -z "$alpha_tag" ] && alpha_tag=$t ;;
      1) [ -z "$beta_tag" ] && beta_tag=$t ;;
      2) [ -z "$rc_tag" ] && rc_tag=$t ;;
      3) [ -z "$stable_tag" ] && stable_tag=$t ;;
    esac
  done

  # Which channel is being followed. An explicit ALLOYFS_CHANNEL wins; failing
  # that, the installed binary's own version says it. Choosing the channel by
  # what is already here is the whole point: installing "the newest thing
  # published" regardless of channel is what put a STABLE machine onto an
  # alpha, which is a far worse surprise than a missed upgrade.
  asked=''
  mine_rung="${ALLOYFS_CHANNEL:-}"
  case "$mine_rung" in
    alpha) mine_rung=0; asked=1 ;;
    beta) mine_rung=1; asked=1 ;;
    rc|pre) mine_rung=2; asked=1 ;;
    stable) mine_rung=3; asked=1 ;;
    # Deliberately off the ladder: whatever is newest, whatever rung it is
    # on. The escape hatch for someone who wants the bleeding edge without
    # first installing something on that channel.
    latest) mine_rung=9 ;;
    '') current=$(installed_version); [ -n "$current" ] && mine_rung=$(rung_of "$current") ;;
    *) die "unknown ALLOYFS_CHANNEL '$mine_rung'. Use alpha, beta, rc, stable or latest." ;;
  esac

  case "$mine_rung" in
    0) mine=$alpha_tag ;;
    1) mine=$beta_tag ;;
    2) mine=$rc_tag ;;
    3) mine=$stable_tag ;;
    *) mine='' ;;
  esac

  if [ -z "$mine" ] && [ -n "$asked" ]; then
    # A channel was NAMED and has nothing on it. Falling through to "newest
    # overall" here would answer an explicit `ALLOYFS_CHANNEL=beta` with an
    # alpha — the opposite of what was asked for, reported as success.
    die "nothing is published on the $(rung_name "$mine_rung") channel.

       Published channels right now:$(
      for r in 0 1 2 3; do
        case "$r" in
          0) t=$alpha_tag ;; 1) t=$beta_tag ;; 2) t=$rc_tag ;; 3) t=$stable_tag ;;
        esac
        [ -n "$t" ] && printf '\n         %-7s %s' "$(rung_name "$r")" "$t"
      done
    )"
  fi

  if [ -z "$mine" ]; then
    # Nothing installed and no channel named: there is no channel to follow,
    # so take the newest thing there is.
    version=''
    for t in $stable_tag $rc_tag $beta_tag $alpha_tag; do
      if [ -z "$version" ]; then version=$t; else version=$(newer_of "$t" "$version"); fi
    done
    case "$version" in
      *-*) dim "The newest release is a prerelease ($version); installing it." ;;
    esac
  else
    # Climb. Each rung above the installed one is judged against what is
    # installed, and later rungs overwrite earlier ones — so the result is the
    # MOST stable rung that has caught up. Nothing here can move down a rung:
    # a stable machine is never pulled onto a prerelease.
    version=$mine
    downgrade=''
    for rung in 1 2 3; do
      [ "$rung" -le "${mine_rung:-0}" ] && continue
      case "$rung" in
        1) cand=$beta_tag ;;
        2) cand=$rc_tag ;;
        3) cand=$stable_tag ;;
      esac
      [ -z "$cand" ] && continue
      if [ "$rung" -eq 3 ]; then
        if core_at_least "$cand" "$mine"; then version=$cand; downgrade=''; fi
      elif [ "$cand" != "$mine" ] && [ "$(newer_of "$cand" "$mine")" = "$cand" ]; then
        version=$cand
        downgrade=''
      else
        # Version order says this rung is not ahead. The only way it can still
        # win is by having been published more recently — and since it is not
        # version-newer, taking it means going BACKWARDS in version. That is
        # the one case worth naming out loud rather than performing quietly.
        cand_at=$(published_of "$cand") || true
        mine_at=$(published_of "$mine") || true
        if [ -n "$cand_at" ] && [ -n "$mine_at" ] && [ "$cand_at" \> "$mine_at" ]; then
          version=$cand
          downgrade=1
        fi
      fi
    done

    if [ "$version" != "$mine" ]; then
      to=$(rung_name "$(rung_of "$version")")
      from=$(rung_name "${mine_rung:-0}")
      if [ -n "$downgrade" ]; then
        bold "$to is the channel to be on, but $version is OLDER than $mine."
        dim "Installing it anyway — this is a downgrade, not an upgrade."
      else
        dim "$to has caught up; moving off $from ($mine -> $version)."
      fi
    fi
  fi
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
