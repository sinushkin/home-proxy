#!/usr/bin/env bash
# Настройка сервера для vps-server (запускать один раз на сервере с белым IP, и снова — безопасно):
#   - ip_forward=1 (постоянно, /etc/sysctl.d/90-hp-vps.conf);
#   - NAT: MASQUERADE для подсети туннеля на внешний интерфейс сервера;
#   - пересылка из TUN наружу (iptables FORWARD, а при активном ufw — ufw route);
#   - UDP-порты знакомства и слотов в ufw;
#   - systemd unit hp-vps-nat, который восстанавливает NAT и FORWARD после перезагрузки.
#
#   setup/vps-setup.sh <ssh-алиас сервера>
#   DRY_RUN=1 setup/vps-setup.sh ihor      # только напечатать команды, ничего не менять
#
# Переменные (необязательно): TUN_NAME=hp-vps, TUN_SUBNET=10.94.0.0/24, BOOTSTRAP_PORT=40600,
# SLOT_PORTS=40601-40699. Должны совпадать с vps-client-router-vps-server.sh.
set -euo pipefail

SRV="${1:?использование: $0 <ssh-алиас сервера>}"
TUN_NAME="${TUN_NAME:-hp-vps}"
TUN_SUBNET="${TUN_SUBNET:-10.94.0.0/24}"
BOOTSTRAP_PORT="${BOOTSTRAP_PORT:-40600}"
SLOT_PORTS="${SLOT_PORTS:-40601-40699}"
DRY_RUN="${DRY_RUN:-0}"

SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR)
step() { echo; echo "== $*"; }
die() { echo "ОШИБКА: $*" >&2; exit 1; }

# Выполнить команду на сервере (или напечатать, в режиме DRY_RUN).
run() {
  if [[ "$DRY_RUN" == "1" ]]; then
    echo "[$SRV] $*"
  else
    ssh "${SSH_OPTS[@]}" "$SRV" "$@"
  fi
}

step "Сервер $SRV: что уже есть"
if [[ "$DRY_RUN" != "1" ]]; then
  ssh "${SSH_OPTS[@]}" "$SRV" 'uname -m' >/dev/null || die "нет доступа по ssh к $SRV"
  OUT_IF="$(ssh "${SSH_OPTS[@]}" "$SRV" "ip -4 route get 1.1.1.1 | sed -n 's/.* dev \([^ ]*\).*/\1/p'")"
  [[ -n "$OUT_IF" ]] || die "не определился внешний интерфейс сервера"
  UFW_ACTIVE="$(ssh "${SSH_OPTS[@]}" "$SRV" "ufw status 2>/dev/null | grep -c 'Status: active' || true")"
else
  OUT_IF="<внешний интерфейс>"
  UFW_ACTIVE="0"
fi
echo "внешний интерфейс: $OUT_IF, ufw активен: $UFW_ACTIVE"

step "Пересылка пакетов (ip_forward)"
run "sysctl -w net.ipv4.ip_forward=1 >/dev/null"
run "printf 'net.ipv4.ip_forward = 1\n' > /etc/sysctl.d/90-hp-vps.conf"

step "NAT и пересылка из туннеля (iptables)"
# Правила проверяем через -C и добавляем только если их нет: повторный запуск ничего не дублирует.
NAT_RULE="-s $TUN_SUBNET -o $OUT_IF -j MASQUERADE"
run "iptables -t nat -C POSTROUTING $NAT_RULE 2>/dev/null || iptables -t nat -A POSTROUTING $NAT_RULE"
run "iptables -C FORWARD -i $TUN_NAME -j ACCEPT 2>/dev/null || iptables -I FORWARD -i $TUN_NAME -j ACCEPT"
run "iptables -C FORWARD -o $TUN_NAME -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || iptables -I FORWARD -o $TUN_NAME -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT"

step "Фаервол ufw: порты знакомства и слотов, пересылка из туннеля"
if [[ "$UFW_ACTIVE" == "1" || "$DRY_RUN" == "1" ]]; then
  run "ufw allow ${BOOTSTRAP_PORT}/udp comment 'hp-vps знакомство'"
  run "ufw allow ${SLOT_PORTS/-/:}/udp comment 'hp-vps слоты'"
  run "ufw route allow in on $TUN_NAME out on $OUT_IF comment 'hp-vps пересылка'"
else
  echo "ufw не активен — правила портов пропускаем (порты открыты по умолчанию)"
fi

step "Восстановление правил после перезагрузки (unit hp-vps-nat)"
UNIT="[Unit]
Description=home-proxy vps: NAT и пересылка для подсети туннеля
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'sysctl -w net.ipv4.ip_forward=1 >/dev/null; iptables -t nat -C POSTROUTING $NAT_RULE 2>/dev/null || iptables -t nat -A POSTROUTING $NAT_RULE; iptables -C FORWARD -i $TUN_NAME -j ACCEPT 2>/dev/null || iptables -I FORWARD -i $TUN_NAME -j ACCEPT; iptables -C FORWARD -o $TUN_NAME -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || iptables -I FORWARD -o $TUN_NAME -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT'

[Install]
WantedBy=multi-user.target"
if [[ "$DRY_RUN" == "1" ]]; then
  echo "[$SRV] запишем /etc/systemd/system/hp-vps-nat.service:"
  echo "$UNIT" | sed 's/^/    /'
  echo "[$SRV] systemctl daemon-reload && systemctl enable hp-vps-nat"
else
  printf '%s\n' "$UNIT" | ssh "${SSH_OPTS[@]}" "$SRV" 'cat > /etc/systemd/system/hp-vps-nat.service'
  ssh "${SSH_OPTS[@]}" "$SRV" 'systemctl daemon-reload && systemctl enable hp-vps-nat >/dev/null'
fi

step "Проверка"
if [[ "$DRY_RUN" == "1" ]]; then
  echo "(в режиме DRY_RUN проверка не выполняется)"
else
  ssh "${SSH_OPTS[@]}" "$SRV" "iptables -t nat -S POSTROUTING | grep -F -- '$NAT_RULE' && echo 'NAT: есть' || echo 'NAT: НЕТ'; sysctl -n net.ipv4.ip_forward | sed 's/^/ip_forward=/'"
fi
echo
echo "Готово. Проверка из туннеля: с клиента ping 10.94.0.1, а с сервера — пинг клиента в $TUN_SUBNET."
