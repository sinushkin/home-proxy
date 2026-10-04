#!/bin/sh
# UDP с LAN напрямую через WAN (торренты, игры), остальной трафик — в туннель vps-client.
# Вызывается из хука туннеля (см. README) или вручную. Не сбрасывает правила fw4: свои правила —
# в отдельной таблице inet hp_udp и в таблице маршрутизации DIRECT_TABLE. Повторный запуск не дублирует.
PATH=/usr/sbin:/usr/bin:/sbin:/bin
DIR="$(cd "$(dirname "$0")" && pwd)"
[ -f "$DIR/env" ] && . "$DIR/env"
UDP_DIRECT_PORTS="${UDP_DIRECT_PORTS:-1025-65535}"
DIRECT_MARK="${DIRECT_MARK:-0x2}"
DIRECT_TABLE="${DIRECT_TABLE:-200}"
LAN_IF="${LAN_IF:-br-lan}"
WAN_IF="${WAN_IF:-$(uci -q get network.wan.device)}"

WAN_GW="$(ip -4 route show default dev "$WAN_IF" | sed -n 's/^default via \([0-9.]*\).*/\1/p' | head -1)"
[ -n "$WAN_GW" ] || { echo "udp-direct: нет шлюза WAN ($WAN_IF)" >&2; exit 1; }

# Таблица маршрутов для помеченного трафика: default — через WAN, LAN — как есть.
ip route replace default via "$WAN_GW" dev "$WAN_IF" table "$DIRECT_TABLE"
for net in $(ip -4 route show dev "$LAN_IF" scope link | awk '{print $1}'); do
	ip route replace "$net" dev "$LAN_IF" table "$DIRECT_TABLE"
done
ip rule show | grep -q "fwmark $DIRECT_MARK lookup $DIRECT_TABLE" || ip rule add fwmark "$DIRECT_MARK" lookup "$DIRECT_TABLE" priority 1000

# Метка на UDP из LAN в диапазоне портов (prerouting до выбора маршрута).
if ! nft list table inet hp_udp >/dev/null 2>&1; then
	nft -f - <<NFT
table inet hp_udp {
	chain prerouting {
		type filter hook prerouting priority -150; policy accept;
		iifname "$LAN_IF" meta l4proto udp udp dport $UDP_DIRECT_PORTS meta mark set $DIRECT_MARK
	}
}
NFT
fi
echo "udp-direct: UDP $UDP_DIRECT_PORTS из $LAN_IF -> $WAN_IF (шлюз $WAN_GW), остальное — в туннель"
