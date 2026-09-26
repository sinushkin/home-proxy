#!/usr/bin/env bash
# Собирает образ дистрибутива для WSL2 (Alpine + hp-server на TUN) в wsl/out/homeproxy-wsl.tar.gz.
# Нужен docker. Импорт на Windows: wsl/install.ps1 (wsl --import).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
mkdir -p wsl/out
docker build -f wsl/Dockerfile -t homeproxy-wsl .
id=$(docker create homeproxy-wsl)
trap 'docker rm -f "$id" >/dev/null' EXIT
docker export "$id" | gzip -9 > wsl/out/homeproxy-wsl.tar.gz
ls -la wsl/out/homeproxy-wsl.tar.gz
