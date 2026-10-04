#!/usr/bin/env bash
# Обёртка: сервер + один клиент. Эквивалент двух вызовов:
#   setup/vps-server.sh <сервер>
#   setup/vps-client.sh <клиент> <сервер>
# Отдельные скрипты — для добавления клиентов к уже работающему серверу.
#
#   setup/vps-client-router-vps-server.sh <ssh-алиас сервера> <ssh-алиас клиента>
#   setup/vps-client-router-vps-server.sh ihor jump17wan
set -euo pipefail
SRV="${1:?использование: $0 <ssh-алиас сервера> <ssh-алиас клиента>}"
CLI="${2:?использование: $0 <ssh-алиас сервера> <ssh-алиас клиента>}"
DIR="$(dirname "$0")"
"$DIR/vps-server.sh" "$SRV"
"$DIR/vps-client.sh" "$CLI" "$SRV"
