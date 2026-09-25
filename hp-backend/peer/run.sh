#!/usr/bin/env bash
# Запуск peer: настройки берёт из .env рядом со скриптом (образец — .env.example).
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

: "${STUN_ADDR:?задайте STUN_ADDR в .env}"
: "${MQTT_ADDR:?задайте MQTT_ADDR в .env}"
: "${MQTT_CA:?задайте MQTT_CA в .env}"
: "${MY_PEER_ID:?задайте MY_PEER_ID в .env}"
: "${PEER_ID:?задайте PEER_ID в .env}"

# Логи только наших крейтов, чтобы не тонуть в отладке rustls/rumqttc.
# LOG_LEVEL: info | debug (по умолчанию debug). Явный RUST_LOG в .env главнее.
LOG_LEVEL="${LOG_LEVEL:-debug}"
export RUST_LOG="${RUST_LOG:-peer=${LOG_LEVEL},connection=${LOG_LEVEL}}"

echo "peer: я=$MY_PEER_ID ищу=$PEER_ID RUST_LOG=$RUST_LOG" >&2
echo "набирайте текст и нажимайте Enter; Ctrl+C — выход" >&2

# В чекауте пересобираем (cargo сам пропустит, если ничего не менялось); на
# хосте без cargo (там лежит готовый бинарник рядом со скриптом) берём его.
if [[ -f ../Cargo.toml ]] && command -v cargo >/dev/null 2>&1; then
  exec cargo run --release -q -p peer -- "$STUN_ADDR" "$MQTT_ADDR" "$MQTT_CA" "$MY_PEER_ID" "$PEER_ID"
elif [[ -x ./peer ]]; then
  exec ./peer "$STUN_ADDR" "$MQTT_ADDR" "$MQTT_CA" "$MY_PEER_ID" "$PEER_ID"
else
  echo "не нашёл ни cargo с исходниками, ни бинарника ./peer" >&2
  exit 1
fi
