#!/usr/bin/env bash
# Связка: vps-server на сервере с белым IP + vps-client на роутере OpenWrt (Xiaomi 4C и подобные,
# MIPS little-endian soft-float). Собирает, копирует, пишет конфиги с GUID и поднимает службы с
# автозапуском. Фаервол и iptables НЕ трогает: только проверяет и печатает, что сделать вручную.
#
#   setup/vps-client-router-vps-server.sh <ssh-алиас сервера> <ssh-алиас роутера>
#   setup/vps-client-router-vps-server.sh ihor jump17wan
#
# Переменные (необязательно):
#   TOOLCHAIN_DIR     каталог toolchain-mipsel_* из SDK OpenWrt (по умолчанию см. ниже)
#   SERVER_PORT       порт знакомства на сервере (40600)
#   SERVER_SLOTS      диапазон портов слотов на сервере (40601-40699)
#   SERVER_TUN        имя TUN на сервере (hp-vps), адрес 10.94.0.1/24
#   CLIENT_TUN        имя TUN на роутере (hpvps)
#
# GUID хранятся в setup/state/ (в .gitignore, права 600). Повторный запуск их переиспользует.
set -euo pipefail

SRV="${1:?использование: $0 <ssh-алиас сервера> <ssh-алиас роутера>}"
RTR="${2:?использование: $0 <ssh-алиас сервера> <ssh-алиас роутера>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STATE="$ROOT/setup/state"
SERVER_PORT="${SERVER_PORT:-40600}"
SERVER_SLOTS="${SERVER_SLOTS:-40601-40699}"
SERVER_TUN="${SERVER_TUN:-hp-vps}"
SERVER_ADDR="10.94.0.1/24"
CLIENT_TUN="${CLIENT_TUN:-hpvps}"
TOOLCHAIN_DIR="${TOOLCHAIN_DIR:-$HOME/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl}"

SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR)
ssh_srv() { ssh "${SSH_OPTS[@]}" "$SRV" "$@"; }
ssh_rtr() { ssh "${SSH_OPTS[@]}" -o ProxyJump=none "$RTR" "$@"; }
step() { echo; echo "== $*"; }
warn() { echo "ВНИМАНИЕ: $*" >&2; }
die() { echo "ОШИБКА: $*" >&2; exit 1; }

# ---------------------------------------------------------------- проверки окружения (до сборки)
step "Проверка тулчейна и инструментов"
[[ -x "$TOOLCHAIN_DIR/bin/mipsel-openwrt-linux-musl-gcc" ]] || die "тулчейн OpenWrt не найден: $TOOLCHAIN_DIR/bin/mipsel-openwrt-linux-musl-gcc.
  Скачайте SDK для ramips/mt76x8 с https://downloads.openwrt.org (версия не ниже прошивки роутера),
  распакуйте и задайте TOOLCHAIN_DIR=…/staging_dir/toolchain-mipsel_24kc_gcc-…_musl.
  В BUILD_OPENWRT.md — подробности."
command -v cargo >/dev/null || die "нет cargo: установите rustup (https://rustup.rs)"
rustup component list --installed 2>/dev/null | grep -q '^rust-src' || die "нет компонента rust-src: rustup component add rust-src"
command -v protoc >/dev/null || die "нет protoc: пакет protobuf-compiler"
command -v uuidgen >/dev/null || die "нет uuidgen: пакет uuid-runtime"
command -v objdump >/dev/null || die "нет objdump: пакет binutils"
echo "тулчейн: $TOOLCHAIN_DIR"

step "Проверка сервера ($SRV)"
srv_arch="$(ssh_srv 'uname -m')" || die "нет доступа по ssh к $SRV"
[[ "$srv_arch" == "x86_64" ]] || die "сервер $SRV — $srv_arch; скрипт собирает только x86_64"
srv_glibc="$(ssh_srv 'ldd --version | head -1 | grep -oE "[0-9]+\.[0-9]+$"')"
srv_ip="$(ssh_srv "ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p'")"
[[ -n "$srv_ip" ]] || die "не определился публичный IP сервера"
echo "архитектура x86_64, glibc $srv_glibc, публичный IP $srv_ip"

step "Проверка роутера ($RTR)"
rtr_arch="$(ssh_rtr 'uname -m')" || die "нет доступа по ssh к $RTR (через ProxyJump=none, напрямую по WAN)"
[[ "$rtr_arch" == "mips" ]] || die "роутер $RTR — $rtr_arch; скрипт собирает только mips (little-endian, soft-float)"
ssh_rtr 'test -f /lib/ld-musl-mipsel-sf.so.1' || die "на роутере нет загрузчика ld-musl-mipsel-sf.so.1: сборка не подойдёт, нужен SDK для mipsel_24kc"
echo "архитектура mips, загрузчик на месте"

# ---------------------------------------------------------------- GUID (один раз, в setup/state)
mkdir -p "$STATE"
chmod 700 "$STATE"
SERVER_STATE="$STATE/$SRV.server.env"
CLIENT_STATE="$STATE/$SRV.$RTR.client.env"
if [[ ! -f "$SERVER_STATE" ]]; then
  (umask 077; echo "SERVER_GUID=$(uuidgen)" > "$SERVER_STATE")
fi
if [[ ! -f "$CLIENT_STATE" ]]; then
  (umask 077; echo "CLIENT_GUID=$(uuidgen)" > "$CLIENT_STATE")
fi
# shellcheck disable=SC1090
source "$SERVER_STATE"; source "$CLIENT_STATE"
[[ -n "${SERVER_GUID:-}" && -n "${CLIENT_GUID:-}" ]] || die "битый файл GUID в $STATE"
echo "GUID сохранены в $STATE (не коммитятся)"

# ---------------------------------------------------------------- сборка
step "Сборка vps-server (x86_64)"
(cd "$ROOT" && cargo build --release -p vps-server)
srv_bin="$ROOT/target/release/vps-server"
need_glibc="$(objdump -T "$srv_bin" | grep -oE 'GLIBC_[0-9.]+' | sed 's/GLIBC_//' | sort -V | tail -1)"
newest="$(printf '%s\n%s\n' "$need_glibc" "$srv_glibc" | sort -V | tail -1)"
[[ "$newest" == "$srv_glibc" ]] || die "бинарник требует glibc $need_glibc, на сервере $srv_glibc. Соберите на машине с более старой glibc."

step "Сборка vps-client (mipsel, OpenWrt)"
(cd "$ROOT" && TOOLCHAIN_DIR="$TOOLCHAIN_DIR" PACKAGES=vps-client ./OpenWRT/build.sh >/dev/null)
rtr_bin="$ROOT/target/openwrt/mipsel-unknown-linux-musl/release/vps-client"
[[ -x "$rtr_bin" ]] || die "сборка vps-client не дала $rtr_bin"
if objdump -T "$rtr_bin" | grep -q '__atomic_.*_8'; then die "в vps-client 64-битные атомики: на MIPS32 не запустится"; fi

# ---------------------------------------------------------------- сервер
step "Сервер $SRV: файлы, клиенты, служба"
ssh_srv 'mkdir -p /opt/hp-vps && chmod 700 /opt/hp-vps'
ssh_srv 'cat > /opt/hp-vps/vps-server.new && chmod 755 /opt/hp-vps/vps-server.new && mv /opt/hp-vps/vps-server.new /opt/hp-vps/vps-server' < "$srv_bin"

ssh_srv "cat > /opt/hp-vps/vps.env.new && chmod 600 /opt/hp-vps/vps.env.new && mv /opt/hp-vps/vps.env.new /opt/hp-vps/vps.env" <<EOF
MY_ID=$SERVER_GUID
VPS_PUBLIC_IP=$srv_ip
VPS_BOOTSTRAP_PORT=$SERVER_PORT
VPS_PORTS=$SERVER_SLOTS
TUN_NAME=$SERVER_TUN
TUN_ADDR=$SERVER_ADDR
CONTROL_ADDR=off
CLIENTS_FILE=/opt/hp-vps/clients.txt
RUST_LOG=info
EOF

# Клиенты: одна строка на GUID; строку этого роутера добавляем, если её нет.
ssh_srv "touch /opt/hp-vps/clients.txt && chmod 600 /opt/hp-vps/clients.txt && (grep -qx '$CLIENT_GUID' /opt/hp-vps/clients.txt || echo '$CLIENT_GUID' >> /opt/hp-vps/clients.txt)"

ssh_srv "cat > /etc/systemd/system/hp-vps-server.service" <<'EOF'
[Unit]
Description=home-proxy vps-server (клиенты с белым IP сервера)
After=network-online.target
Wants=network-online.target

[Service]
WorkingDirectory=/opt/hp-vps
ExecStart=/opt/hp-vps/vps-server --config /opt/hp-vps/vps.env
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
ssh_srv 'systemctl daemon-reload && systemctl enable hp-vps-server >/dev/null && systemctl restart hp-vps-server'
sleep 3
ssh_srv 'systemctl is-active hp-vps-server' | grep -qx active || die "служба hp-vps-server не поднялась: journalctl -u hp-vps-server на $SRV"
echo "служба hp-vps-server: active, автозапуск включён"

# ---------------------------------------------------------------- роутер
step "Роутер $RTR: TUN, файлы, служба"
if ! ssh_rtr 'test -c /dev/net/tun'; then
  echo "нет /dev/net/tun — ставим kmod-tun"
  ssh_rtr 'if command -v apk >/dev/null; then apk update >/dev/null && apk add kmod-tun >/dev/null; else opkg update >/dev/null && opkg install kmod-tun >/dev/null; fi'
  ssh_rtr 'test -c /dev/net/tun' || die "после установки kmod-tun нет /dev/net/tun"
fi

# Остановить старые ручные процессы vps-client (если были), иначе TUN занят.
ssh_rtr 'if [ -x /etc/init.d/vps-client ]; then /etc/init.d/vps-client stop 2>/dev/null; fi; for p in $(pidof vps-client 2>/dev/null); do kill $p; done; sleep 2; true'

ssh_rtr 'cat > /usr/bin/vps-client.new && chmod 755 /usr/bin/vps-client.new && mv /usr/bin/vps-client.new /usr/bin/vps-client' < "$rtr_bin"

ssh_rtr 'mkdir -p /etc/vps-client'
ssh_rtr "cat > /etc/vps-client/vps-client.conf.new && chmod 600 /etc/vps-client/vps-client.conf.new && mv /etc/vps-client/vps-client.conf.new /etc/vps-client/vps-client.conf" <<EOF
VPS_SERVER=$srv_ip:$SERVER_PORT
VPS_MY_ID=$CLIENT_GUID
VPS_PEER_ID=$SERVER_GUID
TUN_NAME=$CLIENT_TUN
RUST_LOG=info
EOF

ssh_rtr "cat > /etc/init.d/vps-client.new && chmod 755 /etc/init.d/vps-client.new && mv /etc/init.d/vps-client.new /etc/init.d/vps-client" <<'EOF'
#!/bin/sh /etc/rc.common
# vps-client (home-proxy): клиент VPS-сервера с белым IP. Настройки — /etc/vps-client/vps-client.conf
START=96
STOP=10
USE_PROCD=1

PROG=/usr/bin/vps-client
CONF=/etc/vps-client/vps-client.conf

start_service() {
	[ -f "$CONF" ] || { echo "vps-client: нет $CONF" >&2; return 1; }
	. "$CONF"
	procd_open_instance
	procd_set_param command "$PROG" "$VPS_SERVER" "$VPS_MY_ID" "$VPS_PEER_ID"
	procd_set_param env TUN_NAME="$TUN_NAME" RUST_LOG="$RUST_LOG"
	procd_set_param respawn 3600 5 0
	procd_set_param stdout 1
	procd_set_param stderr 1
	procd_close_instance
}
EOF
# TUN клиента — в зону firewall wan: иначе трафик от сервера до роутера отбивается (reject).
ssh_rtr "zone=\$(uci show firewall | sed -n \"s/^firewall\\.\\(@zone\\[[0-9]*\\]\\)\\.name='wan'\$/\\1/p\"); [ -n \"\$zone\" ] || { echo 'нет зоны wan' >&2; exit 1; }; if uci show firewall | grep -q \"'$CLIENT_TUN'\"; then echo 'зона wan: $CLIENT_TUN уже есть'; else uci add_list firewall.\$zone.device=$CLIENT_TUN && uci commit firewall && /etc/init.d/firewall reload && echo 'зона wan: добавлен $CLIENT_TUN'; fi" || die "не удалось добавить $CLIENT_TUN в зону wan роутера"

ssh_rtr '/etc/init.d/vps-client enable && /etc/init.d/vps-client restart'
echo "служба vps-client: включена (автозапуск), перезапущена"

# ---------------------------------------------------------------- проверка результата
step "Проверка связки (до 90 с ожидания дыр)"
holes=""
for _ in $(seq 1 18); do
  sleep 5
  holes="$(ssh_rtr 'logread | grep "дыры" | tail -n 1' 2>/dev/null || true)"
  [[ "$holes" == *"дыры 10/10"* ]] && break
done
if [[ "$holes" == *"дыры 10/10"* ]]; then
  echo "роутер: 10/10 дыр"
else
  warn "роутер ещё не набрал 10/10 дыр. Последняя строка: ${holes:-нет логов}"
fi
ssh_srv 'journalctl -u hp-vps-server --no-pager -n 40 | grep -o "дыры [^,]*" | tail -n 1' || true

# ---------------------------------------------------------------- фаервол: только проверка
step "Фаервол сервера (скрипт его не меняет)"
if ! ssh_srv "ufw status 2>/dev/null | grep -qE '40000:40999/udp|$SERVER_PORT/udp'"; then
  warn "на сервере, похоже, не открыт UDP $SERVER_PORT и $SERVER_SLOTS (ufw). Откройте вручную, например:
    ssh $SRV 'ufw allow $SERVER_PORT/udp && ufw allow ${SERVER_SLOTS/-/:}/udp'"
fi

step "Готово"
echo "сервер $SRV: hp-vps-server ($srv_ip:$SERVER_PORT, TUN $SERVER_TUN $SERVER_ADDR)"
echo "роутер $RTR: vps-client (TUN $CLIENT_TUN), GUID клиента в clients.txt на сервере"
