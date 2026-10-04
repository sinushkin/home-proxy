#!/usr/bin/env bash
# Деплой обвязки openwrt-k-router на роутер: udp-direct и direct (скрипты) в /etc/vps-client/,
# список раздельного туннелирования (локальный файл, если задан) — в /etc/vps-client/direct-list.txt.
# Хуки vps-client (setup/vps-client-hooks) уже вызывают эти скрипты, если они есть. Сами туннель,
# маршруты и правила не трогает: они применяются при следующем подъёме туннеля (restart службы).
# Требует уже установленного vps-client (setup/vps-client.sh <роутер> <сервер>).
#
#   openwrt-k-router/deploy.sh <ssh-алиас роутера>
#   DIRECT_LIST_FILE=/путь/к/my-direct.txt openwrt-k-router/deploy.sh jump17wan
#   DRY_RUN=1 openwrt-k-router/deploy.sh jump17wan      # только напечатать действия
set -euo pipefail
. "$(dirname "$0")/../setup/common.sh"

RTR="${1:?использование: $0 <ssh-алиас роутера>}"
KIT="$(dirname "$0")"
DRY_RUN="${DRY_RUN:-0}"
DIRECT_LIST_FILE="${DIRECT_LIST_FILE:-}"

run() {
  if [[ "$DRY_RUN" == "1" ]]; then echo "[$RTR] $*"; else ssh_to "$RTR" "$@"; fi
}
push() { # push <локальный файл> <путь на роутере> <права>
  local src="$1" dst="$2" mode="$3"
  if [[ "$DRY_RUN" == "1" ]]; then echo "[$RTR] $src -> $dst ($mode)"; return 0; fi
  ssh_to "$RTR" "cat > $dst.new && chmod $mode $dst.new && mv $dst.new $dst" < "$src"
}

step "Проверка роутера $RTR"
if [[ "$DRY_RUN" != "1" ]]; then
  ssh_to "$RTR" 'test -x /usr/bin/vps-client' || die "на $RTR нет vps-client: сначала setup/vps-client.sh"
  ssh_to "$RTR" 'nft list table inet fw4 >/dev/null' || die "на $RTR нет nft/fw4"
  [[ -n "$DIRECT_LIST_FILE" ]] || ssh_to "$RTR" 'test -f /etc/vps-client/direct-list.txt' || warn "нет списка раздельного туннелирования: задайте DIRECT_LIST_FILE или положите /etc/vps-client/direct-list.txt вручную"
fi
echo "ok (или DRY_RUN)"

step "Скрипты UDP напрямую (udp-direct)"
run 'mkdir -p /etc/vps-client/udp-direct /etc/vps-client/direct'
push "$KIT/udp-direct/up.sh" /etc/vps-client/udp-direct/up.sh 755
push "$KIT/udp-direct/down.sh" /etc/vps-client/udp-direct/down.sh 755
if [[ -f "$KIT/udp-direct/env" ]]; then
  push "$KIT/udp-direct/env" /etc/vps-client/udp-direct/env 600
fi

step "Раздельное туннелирование (direct)"
push "$KIT/direct/up.sh" /etc/vps-client/direct/up.sh 755
push "$KIT/direct/down.sh" /etc/vps-client/direct/down.sh 755
if [[ -n "$DIRECT_LIST_FILE" ]]; then
  [[ -f "$DIRECT_LIST_FILE" ]] || die "нет файла $DIRECT_LIST_FILE"
  push "$DIRECT_LIST_FILE" /etc/vps-client/direct-list.txt 644
  echo "список: $(grep -cvE '^#|^[[:space:]]*$' "$DIRECT_LIST_FILE") записей"
fi

step "Проверка синтаксиса на роутере"
run 'for f in /etc/vps-client/udp-direct/up.sh /etc/vps-client/udp-direct/down.sh /etc/vps-client/direct/up.sh /etc/vps-client/direct/down.sh; do sh -n $f || exit 1; done && echo синтаксис ok'

step "Готово"
echo "файлы на $RTR обновлены. Применятся при следующем подъёме туннеля:"
echo "  ssh $RTR '/etc/init.d/vps-client restart'   (перезапуск рвёт сессии — делайте в удобный момент)"
