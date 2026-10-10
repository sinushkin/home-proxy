#!/usr/bin/env bash
# Клиент vps-client для машины клиента. Архитектура определяется сама:
#   mips    — роутер OpenWrt (mipsel, soft-float): /usr/bin/vps-client, procd-служба, зона firewall wan;
#   x86_64  — Debian/Ubuntu с systemd: /usr/local/bin/vps-client, служба hp-vps-client (нужен sudo).
# Оба варианта: GUID клиента (setup/state), строка в clients.txt сервера (сервер подхватит за ~2 с
# без перезапуска), хуки DNS по окружению (setup/vps-client-hooks), проверка дыр (не меньше 4 в работе) и маршрутов.
# Сервер должен быть уже настроен: setup/vps-server.sh <сервер>.
#
#   setup/vps-client.sh <ssh-алиас клиента> [ssh-алиас сервера]     (сервер по умолчанию — ihor)
#
# Переменные (необязательно): CLIENT_TUN (hp0), SERVER_PORT (40600), TOOLCHAIN_DIR (для OpenWrt).
set -euo pipefail
. "$(dirname "$0")/common.sh"

CLI="${1:?использование: $0 <ssh-алиас клиента> [ssh-алиас сервера]}"
SRV="${2:-${VPS_SERVER_ALIAS:-ihor}}"
CLIENT_TUN="${CLIENT_TUN:-hp0}"
SERVER_PORT="${SERVER_PORT:-40600}"

SERVER_STATE="$STATE/$SRV.server.env"
[[ -f "$SERVER_STATE" ]] || die "нет $SERVER_STATE: сначала setup/vps-server.sh $SRV"
SERVER_GUID="$(sed -n 's/^SERVER_GUID=//p' "$SERVER_STATE")"
SERVER_IP="$(sed -n 's/^SERVER_IP=//p' "$SERVER_STATE")"
[[ -n "$SERVER_GUID" ]] || die "в $SERVER_STATE нет SERVER_GUID: запустите setup/vps-server.sh $SRV"
if [[ -z "$SERVER_IP" ]]; then
  # Старое состояние без IP: берём адрес у сервера и дописываем в файл.
  SERVER_IP="$(ssh_to "$SRV" "ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p'")" || true
  [[ -n "$SERVER_IP" ]] || die "не удалось узнать публичный IP сервера $SRV"
  printf 'SERVER_IP=%s\n' "$SERVER_IP" >> "$SERVER_STATE"
  echo "IP сервера $SERVER_IP записан в $SERVER_STATE"
fi

step "Клиент $CLI: проверка"
arch="$(ssh_to "$CLI" 'uname -m')" || die "нет доступа по ssh к $CLI"
CLIENT_GUID="$(ensure_guid "$STATE/$SRV.$CLI.client.env" CLIENT_GUID)"
echo "архитектура $arch, GUID клиента в $STATE/$SRV.$CLI.client.env"

step "Сервер $SRV: клиент в clients.txt"
ssh_to "$SRV" "touch /opt/hp-vps/clients.txt && chmod 600 /opt/hp-vps/clients.txt && (grep -qx '$CLIENT_GUID' /opt/hp-vps/clients.txt || echo '$CLIENT_GUID' >> /opt/hp-vps/clients.txt)" \
  || die "не удалось записать клиента в /opt/hp-vps/clients.txt на $SRV (сервер настроен? setup/vps-server.sh $SRV)"
echo "строка есть в clients.txt; сервер перечитает файл за ~2 с, перезапуск не нужен"

# Набор дыр динамический (4..10, дыры стареют и заменяются): «поднялся» — в работе не меньше 4.
holes_ok() { [[ "$1" =~ дыры\ ([0-9]+)/ ]] && (( BASH_REMATCH[1] >= 4 )); }

# Конфиг клиента (одинаковый для обоих вариантов): shell-совместимый key=value.
CONF_TEXT="VPS_SERVER=$SERVER_IP:$SERVER_PORT
VPS_MY_ID=$CLIENT_GUID
VPS_PEER_ID=$SERVER_GUID
TUN_NAME=$CLIENT_TUN
RUST_LOG=info
ON_TUN_UP=/etc/vps-client/on-tun-up.sh
ON_TUN_DOWN=/etc/vps-client/on-tun-down.sh
${CONTROL_ADDR:+CONTROL_ADDR=$CONTROL_ADDR
}"
# CONTROL_ADDR (необязательно, при запуске скрипта): протокол управления для hpctl/трея, например
# 192.168.1.1:47001 (адрес LAN роутера) или 127.0.0.1:47001; строка подключения — vps-client --connection-string.

install_openwrt() {
  require_build_tools
  require_openwrt_toolchain
  step "Сборка vps-client (mipsel, OpenWrt)"
  (cd "$ROOT" && TOOLCHAIN_DIR="$TOOLCHAIN_DIR" PACKAGES=vps-client ./OpenWRT/build.sh >/dev/null)
  local bin="$ROOT/target/openwrt/mipsel-unknown-linux-musl/release/vps-client"
  [[ -x "$bin" ]] || die "сборка vps-client не дала $bin"
  objdump -T "$bin" | grep -q '__atomic_.*_8' && die "в vps-client 64-битные атомики: на MIPS32 не запустится"

  step "Роутер $CLI: TUN, файлы, служба"
  if ! ssh_to "$CLI" 'test -c /dev/net/tun'; then
    echo "нет /dev/net/tun — ставим kmod-tun"
    ssh_to "$CLI" 'if command -v apk >/dev/null; then apk update >/dev/null && apk add kmod-tun >/dev/null; else opkg update >/dev/null && opkg install kmod-tun >/dev/null; fi'
    ssh_to "$CLI" 'test -c /dev/net/tun' || die "после установки kmod-tun нет /dev/net/tun"
  fi
  # Старый ручной процесс (если был) держит TUN — останавливаем.
  ssh_to "$CLI" 'if [ -x /etc/init.d/vps-client ]; then /etc/init.d/vps-client stop 2>/dev/null; fi; for p in $(pidof vps-client 2>/dev/null); do kill $p; done; sleep 2; true'

  ssh_to "$CLI" 'cat > /usr/bin/vps-client.new && chmod 755 /usr/bin/vps-client.new && mv /usr/bin/vps-client.new /usr/bin/vps-client' < "$bin"
  ssh_to "$CLI" 'mkdir -p /etc/vps-client'
  printf '%s' "$CONF_TEXT" | ssh_to "$CLI" 'cat > /etc/vps-client/vps-client.conf.new && chmod 600 /etc/vps-client/vps-client.conf.new && mv /etc/vps-client/vps-client.conf.new /etc/vps-client/vps-client.conf'

  ssh_to "$CLI" 'cat > /etc/init.d/vps-client.new && chmod 755 /etc/init.d/vps-client.new && mv /etc/init.d/vps-client.new /etc/init.d/vps-client' <<'INIT'
#!/bin/sh /etc/rc.common
# vps-client (home-proxy): клиент VPS-сервера с белым IP. Настройки — /etc/vps-client/vps-client.conf.
# Маршруты туннеля ставит сам vps-client; DNS — хук /etc/vps-client/on-tun-up.sh.
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
	procd_set_param env TUN_NAME="$TUN_NAME" RUST_LOG="$RUST_LOG" ON_TUN_UP="$ON_TUN_UP" ON_TUN_DOWN="$ON_TUN_DOWN" CONTROL_ADDR="$CONTROL_ADDR" CONTROL_KEY_FILE="$CONTROL_KEY_FILE"
	procd_set_param respawn 3600 5 0
	procd_set_param stdout 1
	procd_set_param stderr 1
	procd_close_instance
}
INIT

  # TUN клиента — в зону firewall wan: иначе трафик от сервера до роутера отбивается (reject).
  ssh_to "$CLI" "zone=\$(uci show firewall | sed -n \"s/^firewall\\.\\(@zone\\[[0-9]*\\]\\)\\.name='wan'\$/\\1/p\"); [ -n \"\$zone\" ] || { echo 'нет зоны wan' >&2; exit 1; }; if uci show firewall | grep -q \"'$CLIENT_TUN'\"; then echo 'зона wan: $CLIENT_TUN уже есть'; else uci add_list firewall.\$zone.device=$CLIENT_TUN && uci commit firewall && /etc/init.d/firewall reload && echo 'зона wan: добавлен $CLIENT_TUN'; fi" \
    || die "не удалось добавить $CLIENT_TUN в зону wan роутера"
  ssh_to "$CLI" "nft list table inet fw4 | grep -q '\"$CLIENT_TUN\"'" || die "$CLIENT_TUN нет в правилах fw4: пересылка из LAN в туннель не заработает"

  for hook in on-tun-up.sh on-tun-down.sh; do
    ssh_to "$CLI" "cat > /etc/vps-client/$hook.new && chmod 755 /etc/vps-client/$hook.new && mv /etc/vps-client/$hook.new /etc/vps-client/$hook" < "$ROOT/setup/vps-client-hooks/$hook"
  done
  ssh_to "$CLI" '/etc/init.d/vps-client enable && /etc/init.d/vps-client restart'
  echo "служба vps-client: включена (автозапуск), перезапущена"

  step "Проверка (до 90 с)"
  local holes=""
  for _ in $(seq 1 18); do
    sleep 5
    holes="$(ssh_to "$CLI" 'logread | grep "дыры" | tail -n 1' 2>/dev/null || true)"
    holes_ok "$holes" && break
  done
  if holes_ok "$holes"; then echo "$CLI: ${holes#*дыры }"; else warn "$CLI: набор дыр не поднялся (нужно не меньше 4). Последняя строка: ${holes:-нет логов}"; fi
  local route=""
  for _ in $(seq 1 12); do
    route="$(ssh_to "$CLI" "ip route show 0.0.0.0/1 | head -1" 2>/dev/null || true)"
    [[ "$route" == *"dev $CLIENT_TUN"* ]] && break
    sleep 5
  done
  if [[ "$route" == *"dev $CLIENT_TUN"* ]]; then echo "весь трафик в $CLIENT_TUN (0.0.0.0/1), default аплинка не тронут"; else warn "0.0.0.0/1 не ушёл в $CLIENT_TUN: ${route:-пусто}"; fi
}

install_linux() {
  require_build_tools
  step "Сборка vps-client (x86_64)"
  (cd "$ROOT" && cargo build --release -p vps-client)
  local bin="$ROOT/target/release/vps-client"
  local cli_glibc
  cli_glibc="$(ssh_to "$CLI" 'ldd --version | head -1 | grep -oE "[0-9]+\.[0-9]+$"')"
  check_glibc "$bin" "$cli_glibc"

  step "Клиент $CLI: установка (нужен sudo без пароля)"
  local sudo_cmd=""
  if [[ "$(ssh_to "$CLI" 'id -u')" != "0" ]]; then
    sudo_cmd="sudo -n"
    ssh_to "$CLI" 'sudo -n true' || die "на $CLI нет sudo без пароля (sudo -n)"
  fi
  ssh_to "$CLI" "$sudo_cmd sh -c 'mkdir -p /etc/vps-client && cat > /usr/local/bin/vps-client.new && chmod 755 /usr/local/bin/vps-client.new && mv /usr/local/bin/vps-client.new /usr/local/bin/vps-client'" < "$bin"
  printf '%s' "$CONF_TEXT" | ssh_to "$CLI" "$sudo_cmd sh -c 'cat > /etc/vps-client/vps-client.conf && chmod 600 /etc/vps-client/vps-client.conf'"
  for hook in on-tun-up.sh on-tun-down.sh; do
    ssh_to "$CLI" "$sudo_cmd sh -c 'cat > /etc/vps-client/$hook && chmod 755 /etc/vps-client/$hook'" < "$ROOT/setup/vps-client-hooks/$hook"
  done
  ssh_to "$CLI" "$sudo_cmd sh -c 'cat > /etc/systemd/system/hp-vps-client.service'" <<'UNIT'
[Unit]
Description=home-proxy vps-client (клиент vps-server с белым IP)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/bin/sh -c 'set -a; . /etc/vps-client/vps-client.conf; set +a; exec /usr/local/bin/vps-client "$VPS_SERVER" "$VPS_MY_ID" "$VPS_PEER_ID"'
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
  ssh_to "$CLI" "$sudo_cmd systemctl daemon-reload && $sudo_cmd systemctl enable hp-vps-client >/dev/null && $sudo_cmd systemctl restart hp-vps-client"
  echo "служба hp-vps-client: включена (автозапуск), перезапущена"

  step "Проверка (до 90 с)"
  local holes=""
  for _ in $(seq 1 18); do
    sleep 5
    holes="$(ssh_to "$CLI" "$sudo_cmd journalctl -u hp-vps-client --no-pager -n 20 | grep -o 'дыры [0-9/]*' | tail -n 1" 2>/dev/null || true)"
    holes_ok "$holes" && break
  done
  if holes_ok "$holes"; then echo "$CLI: $holes"; else warn "$CLI: набор дыр не поднялся (нужно не меньше 4). Последняя строка: ${holes:-нет логов}"; fi
  warn "маршруты туннеля перехватывают весь трафик клиента (кроме адреса сервера). Управление $CLI по сети, отличной от его аплинка, может оборваться — см. PLAN, раздел TODO."
}

case "$arch" in
  mips) install_openwrt ;;
  x86_64) install_linux ;;
  *) die "архитектура $arch не поддерживается (нужна mips — OpenWrt или x86_64 — Linux)" ;;
esac

step "Готово"
echo "клиент $CLI: vps-client (TUN $CLIENT_TUN) → сервер $SRV ($SERVER_IP:$SERVER_PORT)"
