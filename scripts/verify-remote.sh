#!/usr/bin/env bash
# The half of the workspace a Windows run cannot compile.
#
#   bash scripts/verify-remote.sh            # host from $ALLOYFS_REMOTE, else "azure"
#   ALLOYFS_REMOTE=other bash scripts/verify-remote.sh
#
# alloyfs-mount-fuse is `#![cfg(unix)]` and mount-kernel's dev.rs/notify.rs are
# Linux-gated, so on Windows they build to nothing and their tests do not run.
# CI covers them on every push, but that answer arrives minutes after the
# mistake; this one arrives before the commit.
#
# Ships a tarball rather than pushing a branch because the destination is an
# rsync target with no .git — which also means uncommitted work is testable,
# the point of running it at all.
set -euo pipefail
cd "$(dirname "$0")/.."

host="${ALLOYFS_REMOTE:-azure}"
dest="${ALLOYFS_REMOTE_DIR:-~/alloyfs}"

echo "==> shipping the workspace to $host:$dest"
# kernel/ rides along so the module sources stay in step with the daemon that
# speaks to them; the ABI is shared and drifts silently otherwise.
tar czf - Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml crates scripts kernel |
  ssh "$host" "mkdir -p $dest && cd $dest && tar xzf -"

echo "==> cargo test --workspace on $host"
ssh "$host" "cd $dest && . ~/.cargo/env 2>/dev/null; cargo test --workspace"

echo
echo "remote verify OK ($host)"
