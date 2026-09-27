#!/usr/bin/env bash
# Runs on the Pi, as root. deploy.sh copies this next to the binary and the
# unit files and calls it; it is also fine to run by hand after scp-ing those
# three files into one directory.
set -euo pipefail

SRC="$(cd "$(dirname "$0")" && pwd)"

[ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }

# An edited config on the box wins over the one being shipped, so the port to
# test is that one — testing the shipped default would check a port the service
# is not going to bind.
CONF=/etc/67ft.conf
[ -f "$CONF" ] || CONF="$SRC/67ft.conf"
PORT_WANTED="$(sed -n 's/^PORT=//p' "$CONF" | head -1)"
PORT_WANTED="${PORT_WANTED:-8080}"

# Stop first: on a re-deploy the port is held by the old 67ft, and freeing it
# here is cheaper than trying to tell it apart from a real conflict.
systemctl stop 67ft 2>/dev/null || true

# Pi-hole v5 puts lighttpd on 80; v6 puts FTL's own server on 80 and 443. Some
# setups move the admin page to 8080, which is exactly where this wants to be,
# so look before binding rather than after.
if command -v ss >/dev/null; then
  CLASH="$(ss -lntpH "sport = :$PORT_WANTED" 2>/dev/null || true)"
  if [ -n "$CLASH" ]; then
    echo "!! port $PORT_WANTED is already taken:" >&2
    echo "$CLASH" >&2
    echo "!! set a different PORT in /etc/67ft.conf, then re-run" >&2
    exit 1
  fi
fi

install -m 0755 "$SRC/67ft" /usr/local/bin/67ft
install -m 0644 "$SRC/67ft.service" /etc/systemd/system/67ft.service

# Never clobber a config that has been edited on the box.
if [ -f /etc/67ft.conf ]; then
  echo "keeping existing /etc/67ft.conf"
else
  install -m 0644 "$SRC/67ft.conf" /etc/67ft.conf
fi

systemctl daemon-reload
systemctl enable 67ft
systemctl restart 67ft

# systemd calls the unit started the moment it forks, which says nothing about
# whether the listener came up. Ask the thing itself.
PORT="$(sed -n 's/^PORT=//p' /etc/67ft.conf | head -1)"; PORT="${PORT:-8080}"
for _ in $(seq 1 20); do
  if curl -fsS --max-time 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
    echo "67ft is up on port $PORT, and enabled at boot"
    systemctl --no-pager --lines=0 status 67ft | head -3
    exit 0
  fi
  sleep 0.5
done

echo "!! service did not answer /health within 10s" >&2
journalctl -u 67ft --no-pager --lines=30 >&2
exit 1
