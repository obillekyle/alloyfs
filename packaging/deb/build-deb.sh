#!/usr/bin/env bash
# Build a Debian package around an already-built alloyfs binary.
#
#   packaging/deb/build-deb.sh <binary> <arch>
#   packaging/deb/build-deb.sh target/x86_64-unknown-linux-musl/release/alloyfs amd64
#   packaging/deb/build-deb.sh target/aarch64-unknown-linux-musl/release/alloyfs arm64
#
# The binary is expected to be a MUSL STATIC build, and that expectation is
# what keeps this script small: a static binary has no glibc floor, so ONE
# .deb per architecture serves every Debian and Ubuntu release, and Depends
# stays empty instead of growing a libc version matrix. (fuse3 is a
# Recommends: only mounting needs it; serving and sync do not.)
#
# Like install.sh, this deliberately does not run cargo — build first, as
# yourself. It also strips CRLF from everything it stages, for the same
# reason install.sh does: half of this repo lives on Windows.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BINARY="${1:?usage: build-deb.sh <binary> <amd64|arm64>}"
ARCH="${2:?usage: build-deb.sh <binary> <amd64|arm64>}"

[ -f "$BINARY" ] || { echo "no such binary: $BINARY" >&2; exit 1; }
case "$ARCH" in amd64 | arm64) ;; *) echo "arch must be amd64 or arm64" >&2; exit 1 ;; esac

# The workspace version is the package version. Debian versions cannot carry
# a bare `-alpha.30` (a dash separates the Debian revision), so the semver
# pre-release dash becomes a tilde — which also sorts BEFORE the final
# release in dpkg's ordering, exactly what a pre-release should do.
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
[ -n "$VERSION" ] || { echo "could not read workspace version" >&2; exit 1; }
DEB_VERSION="${VERSION/-/\~}"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

install -D -m 0755 "$BINARY" "$STAGE/usr/bin/alloyfs"
# Same two rewrites install.sh performs: no CRLF into /etc-adjacent places,
# and ExecStart must match where THIS package puts the binary.
mkdir -p "$STAGE/usr/lib/systemd/system"
sed -e 's/\r$//' \
	-e 's|^ExecStart=/usr/local/bin/alloyfs|ExecStart=/usr/bin/alloyfs|' \
	"$ROOT/scripts/alloyfs.service" >"$STAGE/usr/lib/systemd/system/alloyfs@.service"
chmod 0644 "$STAGE/usr/lib/systemd/system/alloyfs@.service"

mkdir -p "$STAGE/DEBIAN"
sed 's/\r$//' >"$STAGE/DEBIAN/control" <<EOF
Package: alloyfs
Version: $DEB_VERSION
Architecture: $ARCH
Maintainer: Kyle <obillekyle@gmail.com>
Section: utils
Priority: optional
Homepage: https://github.com/obillekyle/alloyfs
Recommends: fuse3
Description: virtual drive service - mount remote folders as real drives
 AlloyFS exports folders from one machine and mounts them as ordinary
 drives on another (FUSE on Linux, WinFsp on Windows), with live change
 events, local caching, and byte-range locks over the wire.
 .
 This package installs the agent/client binary (static, no libc
 dependency) and the systemd template unit alloyfs@<user>. Mounting on
 Linux additionally needs fuse3; serving needs nothing else.
EOF

# No postinst daemon-reload on purpose: dpkg triggers handle unit reloads on
# any modern Debian/Ubuntu, and a maintainer script that shells out is a
# lintian finding waiting to happen.

OUT="$ROOT/target/deb"
mkdir -p "$OUT"
DEB="$OUT/alloyfs_${DEB_VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$DEB" >/dev/null
echo "$DEB"
dpkg-deb --info "$DEB" | sed -n '2,8p'
