#!/bin/sh
# Диагностика «после отключения питания hp-router не подключился, помогла только перезагрузка» (OpenWrt, ash).
# Запускать НА РОУТЕРЕ, КОГДА ПРОБЛЕМА ЕСТЬ, ДО перезагрузки: после неё logread и состояние пропадают.
# Ничего не перезапускает и не меняет; вывод — на экран и в /tmp/debug-startup.txt (ОЗУ, не флеш).
#
#   scp OpenWRT/debug-startup-on-power-on.sh <роутер>:/tmp/ && ssh <роутер> sh /tmp/debug-startup-on-power-on.sh
#
# Что собирает: часы и NTP, аплинк (адрес, маршруты, Wi-Fi), начало лога каждой службы hp-router*
# (когда стартовала и что писала сразу), сокеты и conntrack по серверам, правила nft,
# и 10 с tcpdump (если установлен) — уходят ли UDP-запросы на сервер и приходят ли ответы.
OUT=/tmp/debug-startup.txt
SERVICES="hp-router hp-router2"
DUMP_SECS=10

# ash без process substitution: перезапускаем себя с выводом через tee
if [ -z "${HP_DEBUG_INNER:-}" ]; then
  HP_DEBUG_INNER=1 sh "$0" "$@" 2>&1 | tee "$OUT"
  exit 0
fi

section() { echo; echo "===== $* ====="; }

section "время"
date '+%Y-%m-%d %H:%M:%S %Z'
up=$(cut -d. -f1 /proc/uptime)
echo "uptime: $((up / 60)) мин; загружен $(date -d "@$(( $(date +%s) - up ))" '+%Y-%m-%d %H:%M:%S' 2>/dev/null)"
if pidof ntpd >/dev/null 2>&1; then echo "ntpd работает"; else echo "ntpd НЕ работает"; fi
ubus call system info 2>/dev/null | head -5
echo "logread начинается с: $(logread | head -1 | cut -c1-24)"

section "аплинк: адреса и маршруты"
ip -br addr
ip route
ip rule
for dev in $(ls /sys/class/net | grep -E '^(wan|phy.*sta|wlan.*)'); do
  echo "-- $dev: $(cat /sys/class/net/$dev/operstate 2>/dev/null)"
done
iwinfo 2>/dev/null | grep -E 'ESSID|Signal|Access Point|Mode' | head -12
ubus call network.interface.wan status 2>/dev/null | grep -E '"up"|"uptime"|"address"|"nexthop"' | head -8

section "сервисы hp-router*: когда стартовали и первые строки лога"
for svc in $SERVICES; do
  echo "--- $svc"
  conf="/etc/$svc/router.env"
  [ -f "$conf" ] || { echo "нет $conf"; continue; }
  server=$(sed -n 's/^VPS_SERVER=//p' "$conf" | tr -d ' ' | head -1)
  echo "VPS_SERVER=$server"
  pid=$(pgrep -f "/usr/bin/$svc --config" | head -1)
  if [ -n "$pid" ]; then
    echo "pid $pid, работает $(( $(cut -d. -f1 /proc/uptime) - $(awk '{print int($22/100)}' /proc/$pid/stat) )) с (после загрузки)"
  else
    echo "процесса нет"
  fi
  echo "первые 25 строк лога службы:"
  logread | grep -E "$svc" | head -25 | cut -c1-220
  echo "статистика исходов (последние строки с «не ответил», «открыта», «состояние»):"
  logread | grep -E "$svc" | grep -E 'не ответил|дыра открыта|состояние соединения|WARN|ERROR' | tail -8 | cut -c1-220
done

section "сокеты UDP и серверы"
netstat -uanp 2>/dev/null | grep -E 'hp-router' | head -30
for svc in $SERVICES; do
  server=$(sed -n 's/^VPS_SERVER=//p' "/etc/$svc/router.env" 2>/dev/null | tr -d ' ' | head -1)
  host="${server%%:*}"
  [ -n "$host" ] || continue
  echo "-- ping $host"; ping -c 2 -W 2 "$host" 2>&1 | tail -3
  echo "-- маршрут: $(ip route get "$host" 2>&1 | head -1)"
  echo "-- conntrack $host:"
  grep -F "$host" /proc/net/nf_conntrack 2>/dev/null | head -10
done
echo "conntrack всего: $(cat /proc/sys/net/netfilter/nf_conntrack_count 2>/dev/null) из $(cat /proc/sys/net/netfilter/nf_conntrack_max 2>/dev/null)"

section "nft (правила, которые могут трогать UDP)"
nft list ruleset 2>/dev/null | grep -nE 'notrack|udp|drop|reject' | head -30

section "dmesg: хвост (Wi-Fi, ethernet, память)"
dmesg | tail -30 | cut -c1-200

section "tcpdump ${DUMP_SECS} с: UDP на серверы hp-router* (уходят ли запросы, приходят ли ответы)"
if command -v tcpdump >/dev/null 2>&1; then
  filter=""
  for svc in $SERVICES; do
    server=$(sed -n 's/^VPS_SERVER=//p' "/etc/$svc/router.env" 2>/dev/null | tr -d ' ' | head -1)
    host="${server%%:*}"
    [ -n "$host" ] && filter="${filter:+$filter or }host $host"
  done
  if [ -n "$filter" ]; then
    echo "фильтр: udp and ($filter)"
    tcpdump -ni any -c 60 "udp and ($filter)" 2>&1 &
    tpid=$!
    sleep "$DUMP_SECS"; kill "$tpid" 2>/dev/null
    wait "$tpid" 2>/dev/null
  fi
else
  echo "tcpdump не установлен (opkg update && opkg install tcpdump — только если очень нужно; флеш мал)"
  echo "счётчики интерфейса wan до/после ${DUMP_SECS} с:"
  grep -E 'wan' /proc/net/dev
  sleep "$DUMP_SECS"
  grep -E 'wan' /proc/net/dev
fi

echo
echo "Сохранено в $OUT — скопируйте до перезагрузки: scp <роутер>:$OUT ."
