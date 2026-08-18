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
# vendor/ has to travel too: winfsp-sys is a path dependency of the workspace,
# so cargo cannot even load the manifest without it — Windows-only in effect,
# mandatory for resolution everywhere.
tar czf - Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml crates scripts kernel vendor |
# The shipped directories go first: tar unpacks OVER a tree and never deletes,
# so a renamed file lingers there and fails the build with an error that has
# nothing to do with the change under test.
  ssh "$host" "mkdir -p $dest && cd $dest && rm -rf crates kernel vendor && tar xzf -"

# The SAME gate as scripts/verify.sh, not a weaker one.
#
# This ran only `cargo test` until CI failed on a commit it had passed: the
# dead-code errors that broke the Linux build come from `clippy -D warnings`,
# and `cargo test` reports them as warnings it prints and ignores. A remote
# check that is easier to satisfy than CI is worse than no remote check,
# because it is believed.
echo "==> fmt + clippy + test on $host"
ssh "$host" "cd $dest && . ~/.cargo/env 2>/dev/null; \
  cargo fmt --all --check && \
  cargo clippy --workspace --all-targets -- -D warnings && \
  cargo test --workspace"

echo
echo "remote verify OK ($host)"
