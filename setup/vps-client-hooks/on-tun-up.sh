#!/bin/sh
# Хук vps-client: туннель поднят, маршруты уже стоят. Переменные: DNS (через запятую),
# TUN_DEV, TUN_ADDR, VPS_IP, UPLINK_DEV, ... (см. hp_tun::routes::hook_env).
# Настраивает DNS по окружению: OpenWrt (dnsmasq, runtime-файл в /tmp), Debian/Ubuntu (systemd-resolved).
[ -n "$DNS" ] || { echo "on-tun-up: DNS пуст, ничего не делаю"; exit 0; }

if command -v uci >/dev/null 2>&1 && uci -q show dhcp >/dev/null 2>&1; then
	# Только runtime: /tmp/dnsmasq.d подключён через conf-dir; в /etc/config не пишем.
	mkdir -p /tmp/dnsmasq.d
	{
		echo "no-resolv"
		for s in $(echo "$DNS" | tr "," " "); do echo "server=$s"; done
	} > /tmp/dnsmasq.d/hp-tun.conf.new && mv /tmp/dnsmasq.d/hp-tun.conf.new /tmp/dnsmasq.d/hp-tun.conf
	/etc/init.d/dnsmasq restart >/dev/null 2>&1
	echo "on-tun-up: OpenWrt, runtime dnsmasq -> $DNS"
elif command -v resolvectl >/dev/null 2>&1; then
	resolvectl dns "$TUN_DEV" $TUN_DNS
	resolvectl domain "$TUN_DEV" "~."
	echo "on-tun-up: systemd-resolved, $TUN_DEV -> $TUN_DNS"
else
	echo "on-tun-up: окружение без uci и resolvectl, DNS не настроен" >&2
fi
# Обвязка openwrt-k-router (если установлена): UDP напрямую, затем раздельное туннелирование.
[ -x /etc/vps-client/udp-direct/up.sh ] && /etc/vps-client/udp-direct/up.sh
[ -x /etc/vps-client/direct/up.sh ] && /etc/vps-client/direct/up.sh
exit 0
