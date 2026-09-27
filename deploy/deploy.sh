#!/usr/bin/env bash
# Build 67ft for whatever the Pi actually is, ship it, and leave it running and
# enabled at boot.
#
#   ./deploy/deploy.sh pi@raspberrypi.local
#   ./deploy/deploy.sh pi@192.168.1.4 --native    # compile on the Pi instead
set -euo pipefail

HOST="${1:?usage: deploy.sh <user@host> [--native]}"
MODE="${2:-cross}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"

# The Pi 3B is a 64-bit board that most people run a 32-bit OS on, so the
# hardware says nothing useful here. Ask the kernel which userland it booted.
ARCH="$(ssh "$HOST" uname -m)"
case "$ARCH" in
  aarch64|arm64) TARGET=aarch64-unknown-linux-musl ;;
  armv7l)        TARGET=armv7-unknown-linux-musleabihf ;;
  armv6l)        TARGET=arm-unknown-linux-musleabihf ;;   # Pi 1 / Zero
  x86_64)        TARGET=x86_64-unknown-linux-musl ;;
  *) echo "unrecognised architecture: $ARCH" >&2; exit 1 ;;
esac
echo "==> $HOST is $ARCH, building $TARGET"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

if [ "$MODE" = "--native" ]; then
  # lto and codegen-units=1 are what make the shipped binary small, and also
  # what makes it need more RAM to link than a 1GB Pi reliably has. Trade the
  # size back for a build that finishes.
  echo "==> compiling on the Pi (expect 20-40 minutes on a 3B)"
  ssh "$HOST" 'command -v cargo >/dev/null' \
    || { echo "no cargo on $HOST: install rustup there first" >&2; exit 1; }
  command -v rsync >/dev/null && ssh "$HOST" 'command -v rsync >/dev/null' \
    || { echo "--native needs rsync on both ends" >&2; exit 1; }
  ssh "$HOST" 'mkdir -p ~/67ft-build'
  rsync -a --delete --exclude target --exclude .git "$REPO"/ "$HOST:67ft-build/"
  ssh "$HOST" 'cd ~/67ft-build && CARGO_PROFILE_RELEASE_LTO=false \
      CARGO_PROFILE_RELEASE_CODEGEN_UNITS=4 cargo build --release'
  ssh "$HOST" 'cp ~/67ft-build/target/release/67ft /tmp/67ft-staged'
else
  BIN="$REPO/$("$REPO/deploy/build.sh" "$TARGET")"
  [ -f "$BIN" ] || { echo "build produced no binary at $BIN" >&2; exit 1; }
  echo "==> built $(du -h "$BIN" | cut -f1) binary"
  cp "$BIN" "$STAGE/67ft"
fi

cp "$REPO/deploy/67ft.service" "$REPO/deploy/67ft.conf" "$REPO/deploy/install.sh" "$STAGE/"

echo "==> copying to $HOST"
ssh "$HOST" 'rm -rf ~/.67ft-install && mkdir -p ~/.67ft-install'
scp -q "$STAGE"/* "$HOST:.67ft-install/"
if [ "$MODE" = "--native" ]; then
  ssh "$HOST" 'mv /tmp/67ft-staged ~/.67ft-install/67ft'
fi

echo "==> installing (sudo on the Pi)"
ssh -t "$HOST" 'sudo bash ~/.67ft-install/install.sh'

# From the Pi, not from the shipped default: a config already on the box is
# kept, so on a re-deploy those two can disagree.
PORT="$(ssh "$HOST" "sed -n 's/^PORT=//p' /etc/67ft.conf | head -1")"
echo
echo "Done. http://${HOST#*@}:${PORT:-8080}"
