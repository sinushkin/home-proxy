#!/usr/bin/env bash
# Запуск router: настройки из .env рядом со скриптом (образец — .env.example).
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

for name in STUN_ADDR MQTT_ADDR MQTT_CA SERVER_MY_ID SERVER_PEER_ID PHONE_1_MY_ID PHONE_1_PEER_ID; do
  [[ -n "${!name:-}" ]] || { echo "задайте $name в .env" >&2; exit 1; }
done

# Логи только наших крейтов. LOG_LEVEL: info | debug | trace (по умолчанию info,
# у роутера debug слишком шумный). Явный RUST_LOG в .env главнее.
LOG_LEVEL="${LOG_LEVEL:-info}"
export RUST_LOG="${RUST_LOG:-router=${LOG_LEVEL},connection=${LOG_LEVEL}}"

echo "router: сервер=$SERVER_PEER_ID RUST_LOG=$RUST_LOG (телефоны — PHONE_<n>_* из .env)" >&2

# В чекауте пересобираем через cargo; на хосте без cargo — готовый ./router.
if [[ -f ../Cargo.toml ]] && command -v cargo >/dev/null 2>&1; then
  exec cargo run --release -q -p router
elif [[ -x ./router ]]; then
  exec ./router
else
  echo "не нашёл ни cargo с исходниками, ни бинарника ./router" >&2
  exit 1
fi
