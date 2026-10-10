#!/usr/bin/env bash
# Первый шаг для свежего VPS: провайдер выдал только root и пароль — кладём на машину наш ssh-ключ,
# дальше остальные скрипты (vps-prepare/vps-server/vps-client) ходят по ключу (BatchMode).
# Ключ берётся из KEY (по умолчанию ~/.ssh/id_ed25519.pub, при отсутствии создаётся). Пароль — из
# переменной SSHPASS (нужен sshpass) или вводится вручную. Повторный запуск безопасен.
#
#   setup/ssh-key.sh root@203.0.113.20
#   SSHPASS='пароль' setup/ssh-key.sh root@203.0.113.20
#
# После этого адрес можно использовать вместо ssh-алиаса: setup/vps-server.sh root@203.0.113.20.
set -euo pipefail
. "$(dirname "$0")/common.sh"

TARGET="${1:?использование: $0 <user@хост | ssh-алиас>}"
PUB="${KEY:-$HOME/.ssh/id_ed25519.pub}"

command -v ssh-copy-id >/dev/null || die "нет ssh-copy-id: пакет openssh-client"
if [[ ! -f "$PUB" ]]; then
  step "Ключа нет — создаю ${PUB%.pub}"
  mkdir -p "$(dirname "$PUB")"
  ssh-keygen -q -t ed25519 -N '' -f "${PUB%.pub}"
fi

if ssh_to "$TARGET" true 2>/dev/null; then
  echo "$TARGET: вход по ключу уже работает"
  exit 0
fi

step "Копирую ключ на $TARGET"
if [[ -n "${SSHPASS:-}" ]]; then
  command -v sshpass >/dev/null || die "SSHPASS задан, но нет sshpass: пакет sshpass"
  sshpass -e ssh-copy-id -i "$PUB" -o PubkeyAuthentication=no -o ConnectTimeout=10 "$TARGET"
else
  ssh-copy-id -i "$PUB" -o ConnectTimeout=10 "$TARGET"
fi
ssh_to "$TARGET" true || die "ключ скопирован, но вход по ключу не работает (проверьте PermitRootLogin и IdentityFile)"
echo "$TARGET: вход по ключу работает"
