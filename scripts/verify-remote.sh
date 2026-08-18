#!/usr/bin/env bash
# The half of the workspace a Windows run cannot compile.
#
#   bash scripts/verify-remote.sh              # WSL if present, else azure
#   ALLOYFS_REMOTE=wsl   bash scripts/verify-remote.sh
#   ALLOYFS_REMOTE=azure bash scripts/verify-remote.sh
#   ALLOYFS_REMOTE=other-host bash scripts/verify-remote.sh
#
# alloyfs-mount-fuse is `#![cfg(unix)]` and mount-kernel's dev.rs/notify.rs are
# Linux-gated, so on Windows they build to nothing and their tests do not run.
# CI covers them on every push, but that answer arrives minutes after the
# mistake; this one arrives before the commit.
#
# Ships a tarball rather than pushing a branch because the destination has no
# .git — which also means uncommitted work is testable, the point of running it
# at all.
set -euo pipefail
cd "$(dirname "$0")/.."

# WSL by default when it has a distro, because it is the same check an order of
# magnitude faster: the source arrives in seconds instead of crossing a network,
# and it has more cores and more memory than the remote box. `ALLOYFS_REMOTE`
# still names a host for the times the answer needs to come from somewhere that
# is not this machine.
default_host=azure
if command -v wsl.exe >/dev/null 2>&1 && wsl.exe --list --quiet 2>/dev/null | tr -d '\0\r' | grep -q .; then
  default_host=wsl
fi
host="${ALLOYFS_REMOTE:-$default_host}"
dest="${ALLOYFS_REMOTE_DIR:-~/alloyfs}"

# One indirection so the rest of the script does not care which it is. WSL is
# not reached over ssh, and `bash -lc` is what puts ~/.cargo/bin on PATH there.
run_there() {
  if [ "$host" = wsl ]; then
    wsl.exe -e bash -lc "$1"
  else
    ssh "$host" "$1"
  fi
}

echo "==> shipping the workspace to $host:$dest"
# kernel/ rides along so the module sources stay in step with the daemon that
# speaks to them; the ABI is shared and drifts silently otherwise.
# vendor/ has to travel too: winfsp-sys is a path dependency of the workspace,
# so cargo cannot even load the manifest without it — Windows-only in effect,
# mandatory for resolution everywhere.
#
# The shipped directories go first: tar unpacks OVER a tree and never deletes,
# so a renamed file lingers there and fails the build with an error that has
# nothing to do with the change under test.
tar czf - Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml crates scripts kernel vendor |
  run_there "mkdir -p $dest && cd $dest && rm -rf crates kernel vendor && tar xzf -"

# The SAME gate as scripts/verify.sh, not a weaker one.
#
# This ran only `cargo test` until CI failed on a commit it had passed: the
# dead-code errors that broke the Linux build come from `clippy -D warnings`,
# and `cargo test` reports them as warnings it prints and ignores. A check that
# is easier to satisfy than CI is worse than no check, because it is believed.
echo "==> fmt + clippy + test on $host"
run_there "cd $dest && . ~/.cargo/env 2>/dev/null; \
  cargo fmt --all --check && \
  cargo clippy --workspace --all-targets -- -D warnings && \
  cargo test --workspace"

echo
echo "verify OK ($host)"
