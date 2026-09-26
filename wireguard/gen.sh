#!/usr/bin/env bash
# Генерирует ключи и конфиги WireGuard для схемы «телефон -> дыры -> этот ПК» в
# wireguard/out/ (каталог в git не попадает: там приватные ключи).
#
#   ./gen.sh            # первый раз
#   ./gen.sh --force    # заново (старые ключи будут перезаписаны)
#   WG_OUT_DIR=out/win ./gen.sh   # отдельный набор (второй ПК, тесты); каталог должен быть в .gitignore
#
# Переменные (необязательно): WG_PORT (51820), WG_MTU (1360), WG_DNS (1.1.1.1),
# WG_SUBNET_PREFIX (10.77.0), WG_OUT_IFACE (интерфейс наружу, по умолчанию — как
# у маршрута по умолчанию).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
OUT="${WG_OUT_DIR:-out}"

command -v wg >/dev/null || { echo "нет утилиты wg: sudo apt install wireguard-tools" >&2; exit 1; }
if [[ -e $OUT/server.key && "${1:-}" != "--force" ]]; then
  echo "$OUT/ уже есть. Чтобы перегенерировать ключи: ./gen.sh --force" >&2
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
mkdir -p "$OUT"
wg genkey | tee $OUT/server.key | wg pubkey > $OUT/server.pub
wg genkey | tee $OUT/client.key | wg pubkey > $OUT/client.pub

# GUID'ы для прокси-службы (дыры): свой и телефона. Их знают оба конца заранее.
uuid() { cat /proc/sys/kernel/random/uuid; }
cat > $OUT/guids.env <<GUIDS
PC_ID=$(uuid)
PHONE_ID=$(uuid)
GUIDS

# Имя файла = имя интерфейса (wg-quick), поэтому wghp.
cat > $OUT/wghp.conf <<CONF
[Interface]
Address = $PREFIX.1/24
ListenPort = $PORT
PrivateKey = $(cat $OUT/server.key)
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
PublicKey = $(cat $OUT/client.pub)
AllowedIPs = $PREFIX.2/32
CONF

# Endpoint здесь — заглушка: приложение подменяет его на локальный мост.
cat > $OUT/client.conf <<CONF
[Interface]
PrivateKey = $(cat $OUT/client.key)
Address = $PREFIX.2/32
DNS = $DNS
MTU = $MTU

[Peer]
PublicKey = $(cat $OUT/server.pub)
AllowedIPs = 0.0.0.0/0
Endpoint = 127.0.0.1:51821
PersistentKeepalive = 25
CONF

# Юнит systemd для прокси-службы (сервер дыр -> WireGuard) с путями этой машины.
cat > $OUT/homeproxy-server.service <<UNIT
[Unit]
Description=HomeProxy server (дыры -> WireGuard)
After=network-online.target wg-quick@wghp.service
Wants=network-online.target wg-quick@wghp.service

[Service]
User=$USER
WorkingDirectory=$REPO/hp-server
EnvironmentFile=$REPO/hp-server/.env
Environment=RUST_LOG=server=info,connection=info
ExecStart=$REPO/target/release/hp-server
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

echo "готово, файлы в wireguard/$OUT/:"
ls -1 "$OUT" | sed 's/^/  /'
echo "публичный ключ сервера: $(cat $OUT/server.pub)"
