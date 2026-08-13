#!/usr/bin/env bash
# One-shot local gate. Same invocation on both platforms:
#   Linux:   bash scripts/verify.sh
#   Windows: bash scripts/verify.sh          (Git Bash; gnullvm toolchain)
# --quick skips the release build.
set -euo pipefail
cd "$(dirname "$0")/.."
run() {
  echo
  echo "==> $*"
  "$@"
}
run cargo fmt --all --check
run cargo clippy --workspace --all-targets -- -D warnings
run cargo test --workspace # unit + loopback battery + golden wire test
[ "${1:-}" = "--quick" ] || run cargo build --workspace --release
echo
echo "verify OK"
