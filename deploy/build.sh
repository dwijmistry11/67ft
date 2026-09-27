#!/usr/bin/env bash
# Cross-compile 67ft for a Raspberry Pi. Prints the path to the binary.
#
#   ./deploy/build.sh aarch64-unknown-linux-musl
#
# musl rather than gnu, so the result is a single static binary that does not
# care which Raspberry Pi OS release is on the card.
set -euo pipefail

TARGET="${1:?usage: build.sh <rust-target-triple>}"
cd "$(dirname "$0")/.."

rustup target add "$TARGET" >/dev/null 2>&1 || true

if command -v cargo-zigbuild >/dev/null; then
  echo "building $TARGET with cargo-zigbuild" >&2
  cargo zigbuild --release --target "$TARGET"
elif command -v cross >/dev/null && docker info >/dev/null 2>&1; then
  echo "building $TARGET with cross" >&2
  cross build --release --target "$TARGET"
else
  cat >&2 <<'MSG'
No cross-compiler found. Either will do:

  cargo install cargo-zigbuild && brew install zig   # lighter, no Docker
  cargo install cross                                # needs Docker running

Or build on the Pi itself with: ./deploy/deploy.sh <host> --native
MSG
  exit 1
fi

echo "target/$TARGET/release/67ft"
