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
# nextest when it is installed, plain `cargo test` when it is not. The
# difference that matters is not speed: nextest runs each test in its own
# process, so it can enforce the per-test timeout in .config/nextest.toml —
# a deadlocked mount callback gets NAMED instead of hanging the run — and it
# reports a test that failed then passed as FLAKY rather than silently green.
# Chasing "the create-burst test is sometimes red" is what that is for.
#
# Kept optional on purpose: a fresh clone must be able to run this gate with
# nothing but a Rust toolchain.
if command -v cargo-nextest >/dev/null 2>&1; then
  # Doubled event-settle deadlines, for the same reason CI triples them.
  # nextest runs each test in its own process and starts one per logical CPU;
  # on a 2-core/4-thread laptop that is four concurrent processes, each with
  # a tokio runtime, an agent and an event pump. `cargo test` put a whole
  # binary's tests in one process and oversubscribed far less, so adopting
  # nextest tightened every settle deadline without changing any of them.
  # Measured: local_changes_pushed_live blew its 15 s ceiling once in a full
  # 382-test run, passed the retry in 2.5 s, then passed 20 of 20 on its own.
  # This raises ceilings and slows nothing down — a test that genuinely hangs
  # still fails, at 30 s instead of 15.
  run env ALLOYFS_TEST_DEADLINE_MULT=2 cargo nextest run --workspace
  # nextest deliberately does not run doctests; `cargo test --doc` is the
  # documented way to keep covering them, and this workspace has them.
  run cargo test --workspace --doc
else
  echo
  echo "==> cargo test --workspace (install cargo-nextest for per-test timeouts and flake reports)"
  cargo test --workspace # unit + loopback battery + golden wire test
fi
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
