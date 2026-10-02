#!/bin/bash
# Запуск vps-server напрямую (для тестирования)
# Использование: ./run-server.sh [--config path/to/config.env]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG_FILE="${1:-.}/vps-server.env"

# Проверяем, что config задан или существует
if [ "$1" = "--config" ] && [ -n "$2" ]; then
    CONFIG_FILE="$2"
fi

# Проверяем, что бинарник собран
if [ ! -f "$SCRIPT_DIR/../../target/release/vps-server" ]; then
    echo "Ошибка: vps-server не собран"
    echo "Запустите: cd $SCRIPT_DIR/../.. && cargo build --release -p vps-server"
    exit 1
fi

# Проверяем, что конфиг существует
if [ ! -f "$CONFIG_FILE" ]; then
    echo "Ошибка: конфиг не найден: $CONFIG_FILE"
    echo "Копируем из примера..."
    if [ -f "$SCRIPT_DIR/vps-server.env.example" ]; then
        cp "$SCRIPT_DIR/vps-server.env.example" "$CONFIG_FILE"
        echo "Создан файл $CONFIG_FILE"
        echo "Отредактируйте его и запустите снова:"
        echo "  nano $CONFIG_FILE"
        exit 1
    fi
fi

echo "VPS-сервер: конфиг $CONFIG_FILE"
echo "Для остановки нажмите Ctrl+C"
echo

exec "$SCRIPT_DIR/../../target/release/vps-server" --config "$CONFIG_FILE"
