#!/usr/bin/env bash
# Интеграционная проверка setup/*.sh на чистых системах в Docker (systemd + sshd, у серверов — только
# root/пароль, как у провайдера VPS). Для каждой пары сервер × клиент:
#   ssh-key.sh → vps-prepare.sh → vps-server.sh → vps-client.sh (+ повторный запуск, проверки).
#
#   tests/run.sh [-p проект] [-s серверы] [-c клиенты] [--keep] [--down] [--no-build]
#
#   -p проект     имя compose-проекта (по умолчанию hp-setup-tests; PROJECT=…)
#   -s серверы    через запятую: debian12,debian13,ubuntu2404,ubuntu2204 (по умолчанию все)
#   -c клиенты    через запятую: debian,manjaro (по умолчанию все)
#   --keep        не останавливать контейнеры после прогона (быстрее следующий запуск)
#   --down        только остановить и удалить контейнеры проекта (с томом кэша apt — ещё и --purge)
#   --no-build    не пересобирать образы
# Кэш пакетов apt: локальный apt-cacher-ng (том apt-cache, переживает прогоны). Свой прокси:
#   APT_PROXY=http://192.168.3.2:3142 tests/run.sh      (локальный тогда не поднимается)
# Windows в Docker не запустить — см. tests/README.md.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
OUT="$HERE/out"
PROJECT="${PROJECT:-hp-setup-tests}"
SERVERS="debian12,debian13,ubuntu2404,ubuntu2204"
CLIENTS="debian,manjaro"
KEEP=0 DOWN=0 PURGE=0 BUILD=1
ROOT_PASSWORD="${ROOT_PASSWORD:-rootpass}"

while (($#)); do
  case "$1" in
    -p) PROJECT="$2"; shift 2 ;;
    -s) SERVERS="$2"; shift 2 ;;
    -c) CLIENTS="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --down) DOWN=1; shift ;;
    --purge) PURGE=1; shift ;;
    --no-build) BUILD=0; shift ;;
    -h|--help) sed -n 2,17p "$0"; exit 0 ;;
    *) echo "неизвестный аргумент: $1" >&2; exit 2 ;;
  esac
done

declare -A IP=([debian12]=172.29.77.11 [debian13]=172.29.77.12 [ubuntu2404]=172.29.77.13 [ubuntu2204]=172.29.77.14
               [debian]=172.29.77.21 [manjaro]=172.29.77.22)

dc() { docker compose -p "$PROJECT" -f "$HERE/docker-compose.yml" "$@"; }
log() { echo; echo "### $*"; }

cleanup_state() { rm -f "$ROOT"/setup/state/*172.29.77.* 2>/dev/null || true; }

if ((DOWN)); then
  if ((PURGE)); then dc --profile proxy down -v --remove-orphans; else dc --profile proxy down --remove-orphans; fi
  cleanup_state; exit 0
fi

command -v docker >/dev/null || { echo "нет docker" >&2; exit 1; }
command -v sshpass >/dev/null || { echo "нет sshpass (пакет sshpass): он вводит пароль root, как человек у нового VPS" >&2; exit 1; }

# --- ssh: свой ключ и конфиг; обёртка ssh в PATH, чтобы setup/*.sh не трогали ~/.ssh ---
mkdir -p "$OUT/bin" "$OUT/ssh"; chmod 700 "$OUT/ssh"
[[ -f "$OUT/ssh/id_ed25519" ]] || ssh-keygen -q -t ed25519 -N '' -f "$OUT/ssh/id_ed25519"
cat > "$OUT/ssh/config" <<CFG
Host 172.29.77.*
  IdentityFile $OUT/ssh/id_ed25519
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR
CFG
printf '#!/bin/sh\nexec /usr/bin/ssh -F "%s" "$@"\n' "$OUT/ssh/config" > "$OUT/bin/ssh"; chmod +x "$OUT/bin/ssh"
export PATH="$OUT/bin:$PATH" SSHPASS="$ROOT_PASSWORD" KEY="$OUT/ssh/id_ed25519.pub"

# --- прокси пакетов ---
if [[ -z "${APT_PROXY:-}" ]]; then
  log "Локальный кэш пакетов (apt-cacher-ng)"
  dc --profile proxy up -d proxy
  export APT_PROXY="http://host.docker.internal:3142"
fi

# --- контейнеры ---
svcs=()
for s in ${SERVERS//,/ }; do svcs+=("srv-$s"); done
for c in ${CLIENTS//,/ }; do svcs+=("cli-$c"); done
cleanup_state
log "Образы и контейнеры: ${svcs[*]}"
((BUILD)) && dc build "${svcs[@]}"
dc up -d --force-recreate "${svcs[@]}"
trap '((KEEP)) || { dc --profile proxy stop >/dev/null 2>&1; dc --profile proxy down --remove-orphans >/dev/null 2>&1 || true; }; cleanup_state' EXIT

wait_ssh() {
  for _ in $(seq 1 60); do (exec 3<>/dev/tcp/$1/22) 2>/dev/null && return 0; sleep 1; done
  echo "ssh на $1 не поднялся" >&2; return 1
}

PASS=0 FAIL=0 RESULTS=()
check() { # check <метка> <команда…>
  local label="$1"; shift
  if "$@" >/dev/null 2>&1; then PASS=$((PASS+1)); RESULTS+=("ok    $label"); else FAIL=$((FAIL+1)); RESULTS+=("FAIL  $label"); fi
}
rsh() { ssh -o BatchMode=yes -o ConnectTimeout=10 "$@"; }

log "Доступ по паролю → по ключу (setup/ssh-key.sh)"
for n in ${SERVERS//,/ }; do wait_ssh "${IP[$n]}"; "$ROOT/setup/ssh-key.sh" "root@${IP[$n]}"; done
for n in ${CLIENTS//,/ }; do wait_ssh "${IP[$n]}"; "$ROOT/setup/ssh-key.sh" "testuser@${IP[$n]}"; done

for s in ${SERVERS//,/ }; do
  SRV="root@${IP[$s]}"
  log "Сервер $s: vps-prepare + vps-server"
  if "$ROOT/setup/vps-prepare.sh" "$SRV" && "$ROOT/setup/vps-server.sh" "$SRV"; then :; fi
  check "$s: служба hp-vps-server активна" rsh "$SRV" 'systemctl is-active --quiet hp-vps-server'
  check "$s: ip_forward=1" rsh "$SRV" '[ "$(sysctl -n net.ipv4.ip_forward)" = 1 ]'
  check "$s: NAT MASQUERADE на месте" rsh "$SRV" 'iptables -t nat -S POSTROUTING | grep -q MASQUERADE'
  out="$("$ROOT/setup/vps-server.sh" "$SRV" 2>&1 || true)"
  check "$s: повторный vps-server без изменений" grep -q 'vps-server: без изменений' <<<"$out"
  check "$s: после повтора служба активна" rsh "$SRV" 'systemctl is-active --quiet hp-vps-server'

  for c in ${CLIENTS//,/ }; do
    CLI="testuser@${IP[$c]}"
    log "Пара сервер $s × клиент $c: vps-client"
    "$ROOT/setup/vps-client.sh" "$CLI" "$SRV" || true
    guid="$(sed -n 's/^CLIENT_GUID=//p' "$ROOT/setup/state/$SRV.$CLI.client.env" 2>/dev/null || true)"
    check "$s×$c: GUID клиента в clients.txt сервера" rsh "$SRV" "grep -qx '$guid' /opt/hp-vps/clients.txt"
    check "$s×$c: служба hp-vps-client активна" rsh "$CLI" 'sudo -n systemctl is-active --quiet hp-vps-client'
    check "$s×$c: набор дыр ≥ 4" rsh "$CLI" "sudo -n journalctl -u hp-vps-client --no-pager -n 30 | grep -oE 'дыры [0-9]+/' | tail -1 | grep -oE '[0-9]+' | awk '{exit !(\$1>=4)}'"
    check "$s×$c: пинг сервера через туннель (10.94.0.1)" rsh "$CLI" 'ping -c 3 -W 2 10.94.0.1'
    check "$s×$c: маршрут по умолчанию уходит в hp0" rsh "$CLI" 'ip route get 8.8.8.8 | grep -q "dev hp0"'
    check "$s×$c: curl example.com через туннель" rsh "$CLI" 'curl -fsS --max-time 20 -o /dev/null http://example.com'
    rsh "$CLI" 'sudo -n systemctl stop hp-vps-client' >/dev/null 2>&1 || true   # клиент отпускаем до следующей пары
  done
done

log "Итог"
printf '%s\n' "${RESULTS[@]}"
echo "успешно: $PASS, провалено: $FAIL"
((FAIL == 0))
