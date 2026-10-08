#!/bin/sh
# Read-only probe of the server for setup\win\vps-server.bat. Prints KEY=VALUE lines, changes nothing.
echo "ARCH=$(uname -m)"
echo "GLIBC=$(ldd --version 2>&1 | head -1 | grep -oE '[0-9]+\.[0-9]+$')"
echo "IP=$(ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p')"
if [ -f /opt/hp-vps/vps.env ]; then
  echo "EXISTING_ID=$(sed -n 's/^MY_ID=//p' /opt/hp-vps/vps.env)"
fi
echo "ACTIVE=$(systemctl is-active hp-vps-server 2>/dev/null)"
exit 0
