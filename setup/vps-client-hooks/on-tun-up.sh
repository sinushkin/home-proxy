#!/bin/sh
# Хук vps-client: туннель поднят, маршруты уже стоят. Переменные: DNS (через запятую),
# TUN_DEV, TUN_ADDR, VPS_IP, UPLINK_DEV, ... (см. hp_tun::routes::hook_env).
# Настраивает DNS по окружению: OpenWrt (dnsmasq, uci), Debian/Ubuntu (systemd-resolved).
[ -n "$DNS" ] || { echo "on-tun-up: DNS пуст, ничего не делаю"; exit 0; }

if command -v uci >/dev/null 2>&1 && uci -q show dhcp >/dev/null 2>&1; then
	uci -q delete dhcp.@dnsmasq[0].server
	for s in $(echo "$DNS" | tr ',' ' '); do uci add_list dhcp.@dnsmasq[0].server="$s"; done
	uci set dhcp.@dnsmasq[0].noresolv='1'
	uci commit dhcp
	/etc/init.d/dnsmasq restart >/dev/null 2>&1
	echo "on-tun-up: OpenWrt, dnsmasq -> $DNS"
elif command -v resolvectl >/dev/null 2>&1; then
	resolvectl dns "$TUN_DEV" $TUN_DNS
	resolvectl domain "$TUN_DEV" '~.'
	echo "on-tun-up: systemd-resolved, $TUN_DEV -> $TUN_DNS"
else
	echo "on-tun-up: окружение без uci и resolvectl, DNS не настроен" >&2
fi
exit 0
