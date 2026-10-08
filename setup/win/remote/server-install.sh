#!/bin/sh
# Server part of setup\win\vps-server.bat: the same result as the tail of setup/vps-server.sh.
# Input (environment): SERVER_GUID SRV_IP SERVER_PORT SERVER_SLOTS SERVER_TUN SERVER_ADDR.
# If /opt/hp-vps/vps-server.upload exists, it replaces the binary. The service is restarted only
# if the binary or vps.env actually changed.
set -eu
D=/opt/hp-vps
mkdir -p "$D" && chmod 700 "$D"

BINARY_CHANGED=0
if [ -f "$D/vps-server.upload" ]; then
  chmod 755 "$D/vps-server.upload"
  if ldd "$D/vps-server.upload" 2>&1 | grep -q 'not found'; then
    rm -f "$D/vps-server.upload"
    echo "ERROR: the uploaded vps-server needs libraries/glibc this server does not have" >&2
    exit 1
  fi
  mv -f "$D/vps-server.upload" "$D/vps-server"
  BINARY_CHANGED=1
  echo "vps-server: updated"
else
  echo "vps-server: unchanged"
fi

umask 077
cat > "$D/vps.env.new" <<ENV
MY_ID=$SERVER_GUID
VPS_PUBLIC_IP=$SRV_IP
VPS_BOOTSTRAP_PORT=$SERVER_PORT
VPS_PORTS=$SERVER_SLOTS
TUN_NAME=$SERVER_TUN
TUN_ADDR=$SERVER_ADDR
CONTROL_ADDR=off
CLIENTS_FILE=$D/clients.txt
RUST_LOG=info
ENV
CONFIG_CHANGED=0
if ! cmp -s "$D/vps.env.new" "$D/vps.env" 2>/dev/null; then
  mv -f "$D/vps.env.new" "$D/vps.env"
  CONFIG_CHANGED=1
  echo "vps.env: updated"
else
  rm -f "$D/vps.env.new"
  echo "vps.env: unchanged"
fi

# clients.txt: created, never modified here (it holds the GUIDs of all clients).
touch "$D/clients.txt" && chmod 600 "$D/clients.txt"

umask 022
cat > /etc/systemd/system/hp-vps-server.service.new <<'UNIT'
[Unit]
Description=home-proxy vps-server (клиенты с белым IP сервера)
After=network-online.target
Wants=network-online.target

[Service]
WorkingDirectory=/opt/hp-vps
ExecStart=/opt/hp-vps/vps-server --config /opt/hp-vps/vps.env
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
if ! cmp -s /etc/systemd/system/hp-vps-server.service.new /etc/systemd/system/hp-vps-server.service 2>/dev/null; then
  mv -f /etc/systemd/system/hp-vps-server.service.new /etc/systemd/system/hp-vps-server.service
  systemctl daemon-reload
  echo "systemd unit: updated"
else
  rm -f /etc/systemd/system/hp-vps-server.service.new
fi
systemctl enable hp-vps-server >/dev/null 2>&1

if [ "$(systemctl is-active hp-vps-server || true)" != "active" ]; then
  systemctl start hp-vps-server
elif [ "$BINARY_CHANGED" = 1 ] || [ "$CONFIG_CHANGED" = 1 ]; then
  systemctl restart hp-vps-server
  echo "service restarted (binary or config changed)"
fi
sleep 3
if [ "$(systemctl is-active hp-vps-server || true)" != "active" ]; then
  echo "ERROR: hp-vps-server did not start: journalctl -u hp-vps-server" >&2
  exit 1
fi
echo "hp-vps-server: active, autostart enabled"
