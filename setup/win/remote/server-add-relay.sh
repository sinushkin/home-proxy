#!/bin/sh
# Server part of setup/vps-relay.sh and setup\win\vps-relay.bat: adds the relay GUID to relays.txt.
# Input (environment): SERVER_DIR (/opt/hp-vps) SERVER_UNIT (hp-vps-server) RELAY_GUID.
# relays.txt is read only at start: the service is restarted (a few seconds without holes for its
# clients) only if the GUID is new. A relay whose name (first GUID group) is a client name is refused.
set -eu
F="$SERVER_DIR/relays.txt"
NAME="$(echo "$RELAY_GUID" | cut -d- -f1)"
if grep -qi "^$NAME-" "$SERVER_DIR/clients.txt" 2>/dev/null; then
  echo "ERROR: $NAME is already a client name in $SERVER_DIR/clients.txt" >&2
  exit 1
fi
touch "$F" && chmod 600 "$F"
if grep -qix "$RELAY_GUID" "$F"; then
  echo "relays.txt: relay $NAME already there"
  exit 0
fi
echo "$RELAY_GUID" >> "$F"
echo "relays.txt: relay $NAME added, restarting $SERVER_UNIT"
systemctl restart "$SERVER_UNIT"
sleep 3
if [ "$(systemctl is-active "$SERVER_UNIT" || true)" != "active" ]; then
  echo "ERROR: $SERVER_UNIT did not start: journalctl -u $SERVER_UNIT" >&2
  exit 1
fi
