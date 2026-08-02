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

# Docs must be built for a LINUX target, not just the host. On a macOS host the `device` and `iso`
# modules are cfg'd out entirely, so building docs there checks barely half the crate and happily
# passes on a broken intra-doc link in the transport. Ask for the Linux target explicitly, and fall
# back to the host only when it is not installed.
echo "== docs =="
if have_target x86_64-unknown-linux-gnu; then
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --features uac-host/uac2 \
    --target x86_64-unknown-linux-gnu
else
  echo "   (x86_64-unknown-linux-gnu not installed; docs cover only the host's cfg)"
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --features uac-host/uac2
fi

echo "all green"
