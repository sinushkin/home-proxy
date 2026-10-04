#!/usr/bin/env bash
# Сервер для vps-client: машина с белым IP (x86_64, Debian/Ubuntu с systemd). Собирает vps-server,
# кладёт в /opt/hp-vps (бинарник, vps.env, clients.txt) и ставит службу hp-vps-server с автозапуском.
# Фаервол и iptables не трогает (их — vps-setup.sh). Клиенты — отдельно: vps-client.sh <клиент> <сервер>.
# Повторный запуск безопасен: GUID и clients.txt сохраняются, сервер перезапускается только если
# бинарник или конфиг поменялись.
#
#   setup/vps-server.sh <ssh-алиас сервера>
#
# Переменные (необязательно): SERVER_PORT (40600), SERVER_SLOTS (40601-40699), SERVER_TUN (hp-vps).
set -euo pipefail
. "$(dirname "$0")/common.sh"

SRV="${1:?использование: $0 <ssh-алиас сервера>}"
SERVER_PORT="${SERVER_PORT:-40600}"
SERVER_SLOTS="${SERVER_SLOTS:-40601-40699}"
SERVER_TUN="${SERVER_TUN:-hp-vps}"
SERVER_ADDR="10.94.0.1/24"

step "Инструменты"
require_build_tools
echo "ok"

step "Сервер $SRV: проверка"
srv_arch="$(ssh_to "$SRV" 'uname -m')" || die "нет доступа по ssh к $SRV"
[[ "$srv_arch" == "x86_64" ]] || die "сервер $SRV — $srv_arch; скрипт собирает только x86_64"
srv_glibc="$(ssh_to "$SRV" 'ldd --version | head -1 | grep -oE "[0-9]+\.[0-9]+$"')"
srv_ip="$(ssh_to "$SRV" "ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p'")"
[[ -n "$srv_ip" ]] || die "не определился публичный IP сервера"
echo "x86_64, glibc $srv_glibc, публичный IP $srv_ip"

step "GUID сервера"
SERVER_GUID="$(ensure_guid "$STATE/$SRV.server.env" SERVER_GUID)"
# IP пишем в состояние: vps-client.sh возьмёт его отсюда (и не будет спрашивать сервер заново).
(umask 077; printf 'SERVER_GUID=%s\nSERVER_IP=%s\n' "$SERVER_GUID" "$srv_ip" > "$STATE/$SRV.server.env")
echo "состояние сервера: $STATE/$SRV.server.env (не коммитится)"

step "Сборка vps-server (x86_64)"
(cd "$ROOT" && cargo build --release -p vps-server)
srv_bin="$ROOT/target/release/vps-server"
check_glibc "$srv_bin" "$srv_glibc"

step "Сервер $SRV: файлы и служба"
ssh_to "$SRV" 'mkdir -p /opt/hp-vps && chmod 700 /opt/hp-vps'

# Бинарник: ставим, только если он отличается (тогда и перезапуск).
local_md5="$(md5sum "$srv_bin" | cut -d' ' -f1)"
remote_md5="$(ssh_to "$SRV" 'md5sum /opt/hp-vps/vps-server 2>/dev/null | cut -d" " -f1' || true)"
binary_changed=0
if [[ "$local_md5" != "$remote_md5" ]]; then
  ssh_to "$SRV" 'cat > /opt/hp-vps/vps-server.new && chmod 755 /opt/hp-vps/vps-server.new && mv /opt/hp-vps/vps-server.new /opt/hp-vps/vps-server' < "$srv_bin"
  binary_changed=1
  echo "vps-server: обновлён"
else
  echo "vps-server: без изменений"
fi

env_text="MY_ID=$SERVER_GUID
VPS_PUBLIC_IP=$srv_ip
VPS_BOOTSTRAP_PORT=$SERVER_PORT
VPS_PORTS=$SERVER_SLOTS
TUN_NAME=$SERVER_TUN
TUN_ADDR=$SERVER_ADDR
CONTROL_ADDR=off
CLIENTS_FILE=/opt/hp-vps/clients.txt
RUST_LOG=info
"
remote_env_md5="$(printf '%s' "$env_text" | md5sum | cut -d' ' -f1)"
current_env_md5="$(ssh_to "$SRV" 'md5sum /opt/hp-vps/vps.env 2>/dev/null | cut -d" " -f1' || true)"
config_changed=0
if [[ "$remote_env_md5" != "$current_env_md5" ]]; then
  printf '%s' "$env_text" | ssh_to "$SRV" 'cat > /opt/hp-vps/vps.env.new && chmod 600 /opt/hp-vps/vps.env.new && mv /opt/hp-vps/vps.env.new /opt/hp-vps/vps.env'
  config_changed=1
fi

# clients.txt: создаём, но не трогаем содержимое (там GUID всех клиентов).
ssh_to "$SRV" 'touch /opt/hp-vps/clients.txt && chmod 600 /opt/hp-vps/clients.txt'

ssh_to "$SRV" 'cat > /etc/systemd/system/hp-vps-server.service' <<'UNIT'
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
UNIT
ssh_to "$SRV" 'systemctl daemon-reload && systemctl enable hp-vps-server >/dev/null'

active="$(ssh_to "$SRV" 'systemctl is-active hp-vps-server' || true)"
if [[ "$active" != "active" ]]; then
  ssh_to "$SRV" 'systemctl start hp-vps-server'
elif [[ $binary_changed == 1 || $config_changed == 1 ]]; then
  ssh_to "$SRV" 'systemctl restart hp-vps-server'
  echo "служба перезапущена (бинарник или конфиг изменились)"
fi
sleep 3
ssh_to "$SRV" 'systemctl is-active hp-vps-server' | grep -qx active || die "служба hp-vps-server не поднялась: journalctl -u hp-vps-server на $SRV"
echo "служба hp-vps-server: active, автозапуск включён"

step "Готово"
echo "сервер $SRV: $srv_ip:$SERVER_PORT, TUN $SERVER_TUN $SERVER_ADDR, порты слотов $SERVER_SLOTS"
echo "клиентов добавлять: setup/vps-client.sh <клиент> $SRV"
