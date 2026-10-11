#!/bin/sh
# Хук vps-client: туннель снимается (маршруты уже сняты). Убираем runtime-DNS, dnsmasq берёт resolv от WAN.
if command -v uci >/dev/null 2>&1 && uci -q show dhcp >/dev/null 2>&1; then
	rm -f /tmp/dnsmasq.d/hp-tun.conf
	/etc/init.d/dnsmasq restart >/dev/null 2>&1
	echo "on-tun-down: OpenWrt, runtime-DNS снят, dnsmasq берёт resolv от WAN"
elif command -v resolvectl >/dev/null 2>&1; then
	resolvectl revert "$TUN_DEV" 2>/dev/null
	echo "on-tun-down: systemd-resolved, $TUN_DEV сброшен"
fi
# Обвязка openwrt-k-router (если установлена): сначала раздельное туннелирование, потом UDP.
[ -x /etc/vps-client/direct/down.sh ] && /etc/vps-client/direct/down.sh
[ -x /etc/vps-client/udp-direct/down.sh ] && /etc/vps-client/udp-direct/down.sh
exit 0
