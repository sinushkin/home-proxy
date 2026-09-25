#!/usr/bin/env bash
# Запуск server: настройки из .env рядом со скриптом (образец — .env.example).
# Относительные пути в .env считаются от каталога скрипта.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

if [[ ! -f .env ]]; then
  echo "нет файла .env рядом с run.sh — скопируйте .env.example в .env и заполните" >&2
  exit 1
fi
set -a
# shellcheck disable=SC1091
source .env
set +a

for name in STUN_ADDR MQTT_ADDR MQTT_CA MY_ID PEER_ID; do
  [[ -n "${!name:-}" ]] || { echo "задайте $name в .env" >&2; exit 1; }
done

# Логи только наших крейтов. LOG_LEVEL: info | debug | trace (по умолчанию info).
# Явный RUST_LOG в .env главнее.
LOG_LEVEL="${LOG_LEVEL:-info}"
export RUST_LOG="${RUST_LOG:-server=${LOG_LEVEL},connection=${LOG_LEVEL}}"

echo "server: я=$MY_ID роутер=$PEER_ID WireGuard=${WG_ADDR:-127.0.0.1:51820} RUST_LOG=$RUST_LOG" >&2

# В чекауте пересобираем через cargo; на хосте без cargo — готовый ./server.
if [[ -f ../Cargo.toml ]] && command -v cargo >/dev/null 2>&1; then
  exec cargo run --release -q -p server
elif [[ -x ./server ]]; then
  exec ./server
else
  echo "не нашёл ни cargo с исходниками, ни бинарника ./server" >&2
  exit 1
fi
