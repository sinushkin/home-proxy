#!/bin/bash
# Отчёт по клиентам, дырам и релеям home-proxy на этом сервере — из памяти служб: hpctl status по
# протоколу управления на 127.0.0.1 (CONTROL_ADDR в настройках службы, ключ — control.key рядом).
#
#   ~/hp-status.sh [-e N] [служба ...]   по умолчанию — все службы vps-server/hp-server с --config
#                                          -e N — ещё N последних событий дыр каждого клиента
#
# Установка на сервер: scp setup/hp-status.sh <сервер>: (рядом с vps-server нужен hpctl —
# cargo build --release -p hp-control, target/release/hpctl → в каталог службы).
# Понятные имена клиентов — ~/hp-aliases.txt (строки «имя псевдоним»).
EVENTS=0
if [ "${1:-}" = "-e" ]; then EVENTS="${2:?число событий}"; shift 2; fi
SERVICES=("$@")
if [ ${#SERVICES[@]} -eq 0 ]; then
  # Службы с протоколом управления: vps-server или hp-server с --config.
  for unit in $(systemctl list-units --type=service --all --no-legend --plain 'hp-*' | awk '{print $1}'); do
    systemctl show "$unit" -p ExecStart --value | grep -qE '(vps-server|hp-server) .*--config' && SERVICES+=("$unit")
  done
fi

ALIASES=""
if [ -f ~/hp-aliases.txt ]; then
  while read -r name rest; do
    [ -n "$name" ] && [ -n "$rest" ] && ALIASES+="s/\\b$name\\b/$name ($rest)/;"
  done < ~/hp-aliases.txt
fi

echo "=== home-proxy на $(hostname -s): отчёт $(date "+%Y-%m-%d %H:%M:%S %Z") ==="
for unit in "${SERVICES[@]}"; do
  if ! systemctl cat "$unit" >/dev/null 2>&1; then echo; echo "[$unit] службы нет"; continue; fi
  active="$(systemctl is-active "$unit")"
  echo; echo "[$unit] $active, запущена: $(systemctl show "$unit" -p ActiveEnterTimestamp --value)"
  [ "$active" = active ] || continue
  conf="$(systemctl show "$unit" -p ExecStart --value | grep -o -- "--config [^ ;]*" | head -1 | cut -d" " -f2)"
  dir="$(dirname "$conf")"
  ctl="$(sed -n "s/^CONTROL_ADDR=//p" "$conf" | tr -d " ")"
  key="$(sed -n "s/^CONTROL_KEY_FILE=//p" "$conf" | tr -d " ")"; key="${key:-control.key}"
  case "$key" in /*) ;; *) key="$dir/$key" ;; esac
  if [ -z "$ctl" ] || [ "$ctl" = off ]; then
    echo "  управление выключено: задайте CONTROL_ADDR=127.0.0.1:<порт> в $conf и перезапустите службу"
    continue
  fi
  # Самый новый hpctl (умеет events): он работает и со службой прошлой версии.
  hpctl=""
  for h in "$dir/hpctl" /opt/hp-vps*/hpctl; do
    if [ -x "$h" ] && "$h" 2>&1 | grep -q events; then hpctl="$h"; break; fi
  done
  [ -n "$hpctl" ] || hpctl="$dir/hpctl"
  "$hpctl" --addr "$ctl" --key-file "$key" status | sed "$ALIASES" | sed "s/^/  /"
  if [ "$EVENTS" -gt 0 ]; then
    echo "  последние события дыр:"
    "$hpctl" --addr "$ctl" --key-file "$key" events "$EVENTS" | sed "$ALIASES" | sed "s/^/    /"
  fi
done
