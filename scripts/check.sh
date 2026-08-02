#!/usr/bin/env bash
# Everything CI runs, in one command, so "green locally" and "green in CI" mean the same thing.
#
# The target list is the point: the transfer machinery is gated to Linux and Android, so a
# host-only check compiles almost none of it. `cargo check` needs no NDK — it never links — which
# is why a macOS or Linux developer can still cover all four targets before pushing.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGETS=(
  x86_64-unknown-linux-gnu
  aarch64-linux-android
  armv7-linux-androideabi
  x86_64-linux-android
)
FEATURES=("" "--features uac-host/uac2")

have_target() { rustup target list --installed 2>/dev/null | grep -qx "$1"; }

echo "== fmt =="
cargo fmt --all -- --check

echo "== tier-0 tests (host) =="
cargo test --workspace
cargo test --workspace --features uac-host/uac2

echo "== clippy (host) =="
for f in "${FEATURES[@]}"; do
  # shellcheck disable=SC2086
  cargo clippy --workspace --all-targets $f -- -D warnings
done

for t in "${TARGETS[@]}"; do
  if ! have_target "$t"; then
    echo "== skipping $t (not installed: rustup target add $t) =="
    continue
  fi
  echo "== clippy $t =="
  for f in "${FEATURES[@]}"; do
    # shellcheck disable=SC2086
    cargo clippy --workspace --all-targets --target "$t" $f -- -D warnings
  done
done

echo "== docs =="
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --features uac-host/uac2

echo "all green"
