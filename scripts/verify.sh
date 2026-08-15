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

# Say what this run could not reach. A bare "verify OK" on Windows reads as
# "everything is checked" when ~870 lines were never compiled — and that gap is
# how a broken errno table once reached CI green-locally.
case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN*)
  cat <<'EOF'

NOT covered by this run (Linux-only, empty on Windows):
  crates/alloyfs-mount-fuse/src/lib.rs   the whole FUSE backend
  crates/alloyfs-mount-kernel/src/dev.rs /dev/alloyfs + mount(2)
  crates/alloyfs-mount-kernel/src/notify.rs
Everything else in mount-kernel (abi.rs, server.rs) IS covered — it is
platform-independent on purpose. To check the rest before pushing:
  bash scripts/verify-remote.sh
EOF
  ;;
esac
