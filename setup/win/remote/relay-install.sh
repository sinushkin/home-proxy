#!/bin/sh
# Relay part of setup/vps-relay.sh and setup\win\vps-relay.bat: installs vps-relay on this host.
# Input (environment): RELAY_GUID SERVER_GUID SERVER_ADDR (ip:port of the server bootstrap port)
# RELAY_PUBLIC_IP (may be empty: the server then uses the address the relay comes from) RELAY_PORT
# RELAY_SLOTS. If /opt/hp-relay/vps-relay.upload exists, it replaces the binary. The service is
# restarted only if the binary or relay.env actually changed. Firewall: opened only via an active
# ufw; otherwise a warning with the ports to open.
set -eu
D=/opt/hp-relay
mkdir -p "$D" && chmod 700 "$D"

BINARY_CHANGED=0
if [ -f "$D/vps-relay.upload" ]; then
  chmod 755 "$D/vps-relay.upload"
  if ldd "$D/vps-relay.upload" 2>&1 | grep -q 'not found'; then
    rm -f "$D/vps-relay.upload"
    echo "ERROR: the uploaded vps-relay needs libraries/glibc this host does not have" >&2
    exit 1
  fi
  mv -f "$D/vps-relay.upload" "$D/vps-relay"
  BINARY_CHANGED=1
  echo "vps-relay: updated"
else
  echo "vps-relay: unchanged"
fi

umask 077
{
  echo "VPS_SERVER=$SERVER_ADDR"
  echo "MY_ID=$RELAY_GUID"
  echo "SERVER_ID=$SERVER_GUID"
  [ -n "$RELAY_PUBLIC_IP" ] && echo "RELAY_PUBLIC_IP=$RELAY_PUBLIC_IP"
  echo "RELAY_BOOTSTRAP_PORT=$RELAY_PORT"
  echo "RELAY_PORTS=$RELAY_SLOTS"
  echo "RUST_LOG=info"
} > "$D/relay.env.new"
CONFIG_CHANGED=0
if ! cmp -s "$D/relay.env.new" "$D/relay.env" 2>/dev/null; then
  mv -f "$D/relay.env.new" "$D/relay.env"
  CONFIG_CHANGED=1
  echo "relay.env: updated"
else
  rm -f "$D/relay.env.new"
  echo "relay.env: unchanged"
fi

umask 022
cat > /etc/systemd/system/hp-vps-relay.service.new <<'UNIT'
[Unit]
Description=home-proxy vps-relay (релей vps-server)
After=network-online.target
Wants=network-online.target

[Service]
WorkingDirectory=/opt/hp-relay
EnvironmentFile=/opt/hp-relay/relay.env
ExecStart=/opt/hp-relay/vps-relay
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
if ! cmp -s /etc/systemd/system/hp-vps-relay.service.new /etc/systemd/system/hp-vps-relay.service 2>/dev/null; then
  mv -f /etc/systemd/system/hp-vps-relay.service.new /etc/systemd/system/hp-vps-relay.service
  systemctl daemon-reload
  echo "systemd unit: updated"
else
  rm -f /etc/systemd/system/hp-vps-relay.service.new
fi
systemctl enable hp-vps-relay >/dev/null 2>&1

# Firewall: clients come to RELAY_PORT and RELAY_SLOTS over UDP.
SLOTS_UFW="$(echo "$RELAY_SLOTS" | tr - :)"
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q '^Status: active'; then
  ufw allow "$RELAY_PORT/udp" comment 'home-proxy vps-relay' >/dev/null
  ufw allow "$SLOTS_UFW/udp" comment 'home-proxy vps-relay' >/dev/null
  echo "ufw: $RELAY_PORT/udp and $RELAY_SLOTS/udp allowed"
elif iptables -S INPUT 2>/dev/null | grep -q '^-P INPUT DROP' && ! iptables-save 2>/dev/null | grep -qE -- "--dports? ([0-9,:]*[,:])?$RELAY_PORT([,: ]|$)"; then
  echo "WARNING: INPUT policy is DROP and ufw is not active - open UDP $RELAY_PORT and $RELAY_SLOTS yourself" >&2
  echo "  e.g. iptables -I INPUT -p udp -m multiport --dports $RELAY_PORT,$SLOTS_UFW -j ACCEPT (and make it persistent)" >&2
fi

if [ "$(systemctl is-active hp-vps-relay || true)" != "active" ]; then
  systemctl start hp-vps-relay
elif [ "$BINARY_CHANGED" = 1 ] || [ "$CONFIG_CHANGED" = 1 ]; then
  systemctl restart hp-vps-relay
  echo "service restarted (binary or config changed)"
fi
sleep 3
if [ "$(systemctl is-active hp-vps-relay || true)" != "active" ]; then
  echo "ERROR: hp-vps-relay did not start: journalctl -u hp-vps-relay" >&2
  exit 1
fi
echo "hp-vps-relay: active, autostart enabled"
