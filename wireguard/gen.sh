#!/usr/bin/env bash
# Генерирует ключи и конфиги WireGuard для схемы «телефон -> дыры -> этот ПК» в
# wireguard/out/ (каталог в git не попадает: там приватные ключи).
#
#   ./gen.sh            # первый раз
#   ./gen.sh --force    # заново (старые ключи будут перезаписаны)
#
# Переменные (необязательно): WG_PORT (51820), WG_MTU (1360), WG_DNS (1.1.1.1),
# WG_SUBNET_PREFIX (10.77.0), WG_OUT_IFACE (интерфейс наружу, по умолчанию — как
# у маршрута по умолчанию).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

command -v wg >/dev/null || { echo "нет утилиты wg: sudo apt install wireguard-tools" >&2; exit 1; }
if [[ -e out/server.key && "${1:-}" != "--force" ]]; then
  echo "out/ уже есть. Чтобы перегенерировать ключи: ./gen.sh --force" >&2
  exit 1
fi

PORT="${WG_PORT:-51820}"
MTU="${WG_MTU:-1360}"
DNS="${WG_DNS:-1.1.1.1}"
PREFIX="${WG_SUBNET_PREFIX:-10.77.0}"
IFACE_OUT="${WG_OUT_IFACE:-$(ip route show default | awk '/default/ {print $5; exit}')}"
[[ -n "$IFACE_OUT" ]] || { echo "не нашёл внешний интерфейс, задайте WG_OUT_IFACE" >&2; exit 1; }
REPO="$(cd .. && pwd)"

umask 077
mkdir -p out
wg genkey | tee out/server.key | wg pubkey > out/server.pub
wg genkey | tee out/client.key | wg pubkey > out/client.pub

# GUID'ы для прокси-службы (дыры): свой и телефона. Их знают оба конца заранее.
uuid() { cat /proc/sys/kernel/random/uuid; }
cat > out/guids.env <<GUIDS
PC_ID=$(uuid)
PHONE_ID=$(uuid)
GUIDS

# Имя файла = имя интерфейса (wg-quick), поэтому wghp.
cat > out/wghp.conf <<CONF
[Interface]
Address = $PREFIX.1/24
ListenPort = $PORT
PrivateKey = $(cat out/server.key)
MTU = $MTU
# Выход трафика телефона наружу (NAT) и закрытие порта WireGuard для всех, кроме
# локального моста (прокси-служба обращается к нему по 127.0.0.1).
PostUp = sysctl -q net.ipv4.ip_forward=1
PostUp = iptables -I FORWARD 1 -i %i -j ACCEPT
PostUp = iptables -I FORWARD 1 -o %i -j ACCEPT
PostUp = iptables -t nat -A POSTROUTING -s $PREFIX.0/24 -o $IFACE_OUT -j MASQUERADE
PostUp = iptables -I INPUT 1 ! -i lo -p udp --dport $PORT -j DROP
PostDown = iptables -D FORWARD -i %i -j ACCEPT
PostDown = iptables -D FORWARD -o %i -j ACCEPT
PostDown = iptables -t nat -D POSTROUTING -s $PREFIX.0/24 -o $IFACE_OUT -j MASQUERADE
PostDown = iptables -D INPUT ! -i lo -p udp --dport $PORT -j DROP

[Peer]
# телефон
PublicKey = $(cat out/client.pub)
AllowedIPs = $PREFIX.2/32
CONF

# Endpoint здесь — заглушка: приложение подменяет его на локальный мост.
cat > out/client.conf <<CONF
[Interface]
PrivateKey = $(cat out/client.key)
Address = $PREFIX.2/32
DNS = $DNS
MTU = $MTU

[Peer]
PublicKey = $(cat out/server.pub)
AllowedIPs = 0.0.0.0/0
Endpoint = 127.0.0.1:51821
PersistentKeepalive = 25
CONF

# Юнит systemd для прокси-службы (сервер дыр -> WireGuard) с путями этой машины.
cat > out/homeproxy-server.service <<UNIT
[Unit]
Description=HomeProxy server (дыры -> WireGuard)
After=network-online.target wg-quick@wghp.service
Wants=network-online.target wg-quick@wghp.service

[Service]
User=$USER
WorkingDirectory=$REPO/hp-backend/server
EnvironmentFile=$REPO/hp-backend/server/.env
Environment=RUST_LOG=server=info,connection=info
ExecStart=$REPO/hp-backend/target/release/server
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

echo "готово, файлы в wireguard/out/:"
ls -1 out | sed 's/^/  /'
echo "публичный ключ сервера: $(cat out/server.pub)"
