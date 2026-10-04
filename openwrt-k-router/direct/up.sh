#!/bin/sh
# Адреса из списка (по умолчанию /etc/vps-client/direct-list.txt; свой на каждом роутере).
# Нужен udp-direct/up.sh (таблица 200 и правило fwmark). Набор пересоздаётся при каждом запуске.
PATH=/usr/sbin:/usr/bin:/sbin:/bin
DIR="$(cd "$(dirname "$0")" && pwd)"
[ -f "$DIR/../udp-direct/env" ] && . "$DIR/../udp-direct/env"
DIRECT_MARK="${DIRECT_MARK:-0x2}"
LAN_IF="${LAN_IF:-br-lan}"
LIST="${DIRECT_LIST:-/etc/vps-client/direct-list.txt}"
[ -f "$LIST" ] || { echo "direct: нет $LIST" >&2; exit 1; }

ips="$(grep -vE '^#|^[[:space:]]*$|/' "$LIST" | paste -sd, -)"
nets="$(grep -vE '^#|^[[:space:]]*$' "$LIST" | grep '/' | paste -sd, -)"

ips_block=""; [ -n "$ips" ] && ips_block="elements = { $ips }"
nets_block=""; [ -n "$nets" ] && nets_block="elements = { $nets }"

nft delete table inet hp_direct 2>/dev/null
{
	echo "table inet hp_direct {"
	echo "	set direct_ips { type ipv4_addr; $ips_block }"
	echo "	set direct_nets { type ipv4_addr; flags interval; $nets_block }"
	echo "	chain prerouting {"
	echo "		type filter hook prerouting priority -140; policy accept;"
	echo "		iifname \"$LAN_IF\" ip daddr 192.168.0.0/16 return"
	echo "		ip daddr @direct_ips meta mark set $DIRECT_MARK"
	echo "		ip daddr @direct_nets meta mark set $DIRECT_MARK"
	echo "	}"
	echo "	chain output {"
	echo "		type route hook output priority -140; policy accept;"
	echo "		ip daddr @direct_ips meta mark set $DIRECT_MARK"
	echo "		ip daddr @direct_nets meta mark set $DIRECT_MARK"
	echo "	}"
	echo "}"
} | nft -f - || { echo "direct: nft не принял набор" >&2; exit 1; }
echo "direct: адресов $(echo "$ips" | tr ',' '\n' | grep -c .), сетей $(echo "$nets" | tr ',' '\n' | grep -c .)"
