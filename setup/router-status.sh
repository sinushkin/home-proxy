#!/bin/sh
# Отчёт «что с home-proxy на роутере» (OpenWrt, ash): часы, службы hp-router*, туннели, маршруты,
# связь с сервером и сводка из logread. Смотреть, когда всё сломалось и непонятно, что было.
#
#   ~/status.sh [-n N]    N — сколько последних строк лога каждой службы показать (по умолчанию 8)
#
# Установка: scp setup/router-status.sh <роутер>:status.sh && ssh <роутер> chmod +x status.sh
# Службы — /etc/init.d/hp-router, hp-router2 (настройки /etc/<служба>/router.env).
# logread живёт в ОЗУ и невелик: после перезагрузки логов до неё нет — смотрите, с какого времени он есть.
LINES=8
if [ "${1:-}" = "-n" ]; then LINES="${2:?число строк}"; shift 2; fi

now=$(date +%s)
up=$(cut -d. -f1 /proc/uptime)
echo "=== $(cat /proc/sys/kernel/hostname): отчёт $(date '+%Y-%m-%d %H:%M:%S %Z') ==="
echo "загружен $((up / 60)) мин назад ($(date -d "@$((now - up))" '+%Y-%m-%d %H:%M:%S' 2>/dev/null))"
if pidof ntpd >/dev/null 2>&1; then echo "ntpd работает"; else echo "ntpd НЕ работает — часы могут врать"; fi
first=$(logread | head -1 | cut -c1-24)
echo "logread с: ${first:-пусто} (раньше логов нет — буфер в ОЗУ)"

report() {
  svc="$1"; pat="$2"
  conf="/etc/$svc/router.env"
  echo
  echo "--- $svc ---"
  [ -f "$conf" ] || { echo "нет $conf"; return; }
  get() { sed -n "s/^$1=//p" "$conf" | tr -d ' ' | head -1; }
  server="$(get VPS_SERVER)"; tun="$(get TUN_NAME)"; tun="${tun:-hp0}"
  pid=$(pgrep -f "/usr/bin/$svc --config" | head -1)
  if [ -z "$pid" ]; then
    echo "процесс: НЕ запущен (автозапуск: $(ls /etc/rc.d/S??$svc >/dev/null 2>&1 && echo включён || echo выключен))"
  else
    start=$(awk '{print $22}' "/proc/$pid/stat")
    age=$((up - start / 100))
    echo "процесс: pid $pid, работает $((age / 60)) мин $((age % 60)) с, стартовал через $((start / 100)) с после загрузки"
    [ $((start / 100)) -lt 120 ] && ! pidof ntpd >/dev/null 2>&1 && echo "  (стартовал рано, до NTP: время запуска клиента могло быть по старым часам)"
  fi
  echo "сервер: ${server:-?}, туннель: $tun"
  ip -br addr show "$tun" 2>/dev/null | sed 's/^/  /' || echo "  интерфейса $tun нет"
  if [ -n "$server" ]; then
    ip="${server%:*}"
    if ping -c1 -W2 "$ip" >/dev/null 2>&1; then echo "ping $ip: есть"; else echo "ping $ip: нет ответа (может быть закрыт ICMP)"; fi
    echo "маршрут до сервера: $(ip route get "$ip" 2>/dev/null | head -1)"
  fi

  logs=$(logread | grep -E "$pat")
  echo "строк в логе: $(echo "$logs" | grep -c .)"
  echo "последняя сводка:  $(echo "$logs" | grep -E 'VPS: дыры' | tail -1 | cut -c1-230)"
  echo "последнее состояние:  $(echo "$logs" | grep 'состояние соединения' | tail -1 | cut -c1-200)"
  stuck=$(echo "$logs" | grep -c 'сервер не ответил')
  [ "$stuck" -gt 0 ] && echo "ВНИМАНИЕ: «сервер не ответил» $stuck раз — сервер не отвечает на знакомство (сервер забыл/игнорирует клиента, закрыт порт или нет связи)"
  lost=$(echo "$logs" | grep -c 'дыра потеряна')
  [ "$lost" -gt 0 ] && echo "потеряно дыр (в буфере): $lost"
  echo "предупреждения и ошибки (WARN/ERROR), последние:"
  echo "$logs" | grep -E ' (WARN|ERROR) ' | tail -5 | cut -c1-230 | sed 's/^/  /'
  echo "последние $LINES строк (без статистики порядка пакетов):"
  echo "$logs" | grep -v 'порядок пакетов' | tail -"$LINES" | cut -c1-230 | sed 's/^/  /'
}

[ -x /etc/init.d/hp-router ] && report hp-router 'hp-router-run|hp-router\['
[ -x /etc/init.d/hp-router2 ] && report hp-router2 'hp-router2\['

echo
echo "--- маршруты и DNS ---"
ip route | grep -E 'hp[0-9]|^default' | sed 's/^/  /'
if [ -f /tmp/dnsmasq.d/hp-tun.conf ]; then echo "  runtime-DNS туннеля: включён ($(tr '\n' ' ' < /tmp/dnsmasq.d/hp-tun.conf | cut -c1-120))"; else echo "  runtime-DNS туннеля: нет (DNS провайдера)"; fi
echo
echo "--- память и нагрузка ---"
echo "  load: $(cut -d' ' -f1-3 /proc/loadavg), свободно ОЗУ: $(awk '/MemAvailable/ {print int($2/1024)" МБ"}' /proc/meminfo)"
