#!/usr/bin/env bash
# Релей vps-relay для vps-server: машина с белым IP (x86_64, Debian/Ubuntu с systemd). Для клиентов
# релей — такой же сервер: часть их дыр идёт через него, остальные — напрямую на сервер. К серверу
# релей приходит сам, по своему GUID; сервер раз в минуту сообщает клиентам подключённые релеи.
#
#   setup/vps-relay.sh <ssh-алиас релея> <ssh-алиас сервера>
#
# Что делает: собирает vps-relay, кладёт в /opt/hp-relay (бинарник, relay.env) и ставит службу
# hp-vps-relay; открывает порты в ufw, если он активен (иначе — предупреждение, какие открыть);
# добавляет GUID релея в relays.txt сервера и перезапускает службу сервера (только если релей новый:
# relays.txt читается при запуске, у клиентов сервера — несколько секунд без дыр). Повторный запуск
# безопасен: GUID релея сохраняется (setup/state/<релей>.relay.env), служба релея перезапускается,
# только если бинарник или настройки поменялись. GUID и адрес сервера берутся из его vps.env.
#
# Переменные (необязательно): SERVER_DIR (/opt/hp-vps) и SERVER_UNIT (hp-vps-server) — экземпляр
# сервера; RELAY_PORT (как VPS_BOOTSTRAP_PORT сервера) и RELAY_SLOTS (как VPS_PORTS сервера) — порты
# релея; RELAY_PUBLIC_IP — белый адрес релея для клиентов (по умолчанию — адрес машины, а если он
# серый — тот, с которого релей приходит к серверу).
set -euo pipefail
. "$(dirname "$0")/common.sh"

RELAY="${1:?использование: $0 <ssh-алиас релея> <ssh-алиас сервера>}"
SRV="${2:?использование: $0 <ssh-алиас релея> <ssh-алиас сервера>}"
SERVER_DIR="${SERVER_DIR:-/opt/hp-vps}"
SERVER_UNIT="${SERVER_UNIT:-hp-vps-server}"
REMOTE="$ROOT/setup/win/remote"

step "Инструменты"
require_build_tools
echo "ok"

step "Сервер $SRV ($SERVER_DIR)"
srv_env="$(ssh_to "$SRV" "cat $SERVER_DIR/vps.env")" || die "нет $SERVER_DIR/vps.env на $SRV: сначала setup/vps-server.sh $SRV"
server_guid="$(sed -n 's/^MY_ID=//p' <<<"$srv_env" | tr -d ' \r')"
server_ip="$(sed -n 's/^VPS_PUBLIC_IP=//p' <<<"$srv_env" | tr -d ' \r')"
server_port="$(sed -n 's/^VPS_BOOTSTRAP_PORT=//p' <<<"$srv_env" | tr -d ' \r')"
server_slots="$(sed -n 's/^VPS_PORTS=//p' <<<"$srv_env" | tr -d ' \r')"
server_port="${server_port:-40000}"
server_slots="${server_slots:-40001-49999}"
[[ -n "$server_guid" && -n "$server_ip" ]] || die "в $SERVER_DIR/vps.env на $SRV нет MY_ID или VPS_PUBLIC_IP"
RELAY_PORT="${RELAY_PORT:-$server_port}"
RELAY_SLOTS="${RELAY_SLOTS:-$server_slots}"
echo "сервер $server_ip:$server_port (${server_guid%%-*}), служба $SERVER_UNIT"

step "Релей $RELAY: проверка"
relay_arch="$(ssh_to "$RELAY" 'uname -m')" || die "нет доступа по ssh к $RELAY"
[[ "$relay_arch" == "x86_64" ]] || die "релей $RELAY — $relay_arch; скрипт собирает только x86_64"
relay_glibc="$(ssh_to "$RELAY" 'ldd --version | head -1 | grep -oE "[0-9]+\.[0-9]+$"')"
relay_ip="$(ssh_to "$RELAY" "ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p'")"
if [[ -z "${RELAY_PUBLIC_IP+x}" ]]; then
  # Серый адрес на интерфейсе — клиентам его не отдаём: сервер возьмёт адрес, с которого пришёл релей
  # (порты тогда должны быть проброшены на релей).
  if [[ "$relay_ip" =~ ^(10\.|192\.168\.|172\.(1[6-9]|2[0-9]|3[01])\.|100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.) ]]; then
    warn "у релея серый адрес $relay_ip: клиентам — адрес, с которого он придёт к серверу; порты $RELAY_PORT и $RELAY_SLOTS должны быть проброшены на него"
    RELAY_PUBLIC_IP=""
  else
    RELAY_PUBLIC_IP="$relay_ip"
  fi
fi
echo "x86_64, glibc $relay_glibc, адрес для клиентов ${RELAY_PUBLIC_IP:-как видит сервер}, порты $RELAY_PORT, $RELAY_SLOTS"

step "GUID релея"
RELAY_GUID="$(ensure_guid "$STATE/$RELAY.relay.env" RELAY_GUID)"
echo "релей ${RELAY_GUID%%-*}, состояние: $STATE/$RELAY.relay.env (не коммитится)"

step "Сборка vps-relay (x86_64)"
(cd "$ROOT" && cargo build --release -p vps-relay)
relay_bin="$ROOT/target/release/vps-relay"
check_glibc "$relay_bin" "$relay_glibc"

step "Релей $RELAY: файлы и служба"
local_md5="$(md5sum "$relay_bin" | cut -d' ' -f1)"
remote_md5="$(ssh_to "$RELAY" 'md5sum /opt/hp-relay/vps-relay 2>/dev/null | cut -d" " -f1' || true)"
if [[ "$local_md5" != "$remote_md5" ]]; then
  ssh_to "$RELAY" 'mkdir -p /opt/hp-relay && chmod 700 /opt/hp-relay && cat > /opt/hp-relay/vps-relay.upload' < "$relay_bin"
fi
ssh_to "$RELAY" env RELAY_GUID="$RELAY_GUID" SERVER_GUID="$server_guid" SERVER_ADDR="$server_ip:$server_port" \
  RELAY_PUBLIC_IP="$RELAY_PUBLIC_IP" RELAY_PORT="$RELAY_PORT" RELAY_SLOTS="$RELAY_SLOTS" sh -s < "$REMOTE/relay-install.sh"

step "Сервер $SRV: релей в relays.txt"
ssh_to "$SRV" env SERVER_DIR="$SERVER_DIR" SERVER_UNIT="$SERVER_UNIT" RELAY_GUID="$RELAY_GUID" sh -s < "$REMOTE/server-add-relay.sh"

step "Проверка: релей на связи с сервером"
name="${RELAY_GUID%%-*}"
# Сервер пишет о релее при смене состояния: смотрим последнюю такую строку с запуска службы.
since="$(ssh_to "$SRV" "systemctl show -p ActiveEnterTimestamp --value $SERVER_UNIT")"
for _ in $(seq 1 15); do
  last="$(ssh_to "$SRV" "journalctl -u $SERVER_UNIT -o cat --since '$since' --no-pager | grep -E 'релей $name: (на связи|нет пакетов)' | tail -1" || true)"
  if [[ "$last" == *"на связи"* ]]; then
    ssh_to "$SRV" "journalctl -u $SERVER_UNIT -o cat --since '$since' --no-pager | grep 'релей $name: порт знакомства' | tail -1" || true
    step "Готово"
    echo "релей $RELAY ($name) на связи с $SRV; клиенты узнают о нём в течение минуты и поведут через него долю новых дыр (VPS_RELAY_SHARE, по умолчанию 50%)"
    exit 0
  fi
  sleep 2
done
die "релей $name не на связи с сервером за 30 с: journalctl -u hp-vps-relay на $RELAY, journalctl -u $SERVER_UNIT на $SRV; порты релея открыты?"
