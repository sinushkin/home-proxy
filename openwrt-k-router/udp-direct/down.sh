#!/bin/sh
# Снимает то, что ставит up.sh. Маршруты туннеля (/1) и default не трогает.
PATH=/usr/sbin:/usr/bin:/sbin:/bin
DIR="$(cd "$(dirname "$0")" && pwd)"
[ -f "$DIR/env" ] && . "$DIR/env"
DIRECT_MARK="${DIRECT_MARK:-0x2}"
DIRECT_TABLE="${DIRECT_TABLE:-200}"
while ip rule del fwmark "$DIRECT_MARK" lookup "$DIRECT_TABLE" 2>/dev/null; do :; done
ip route flush table "$DIRECT_TABLE" 2>/dev/null
nft delete table inet hp_udp 2>/dev/null
echo "udp-direct: снято"
