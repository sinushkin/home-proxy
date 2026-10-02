#!/bin/bash
# Запуск vps-client напрямую (для тестирования)
# Использование: sudo ./run-client.sh <server_ip:port> <my_guid> <server_guid>
#            или sudo ./run-client.sh (возьмёт из переменных окружения)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Проверяем, что бинарник собран
if [ ! -f "$SCRIPT_DIR/../../target/release/vps-client" ]; then
    echo "Ошибка: vps-client не собран"
    echo "Запустите: cd $SCRIPT_DIR/../.. && cargo build --release -p vps-client"
    exit 1
fi

# Проверяем права
if [ "$EUID" -ne 0 ]; then
    echo "Ошибка: требуются права root (для TUN)"
    echo "Запустите: sudo $0 $@"
    exit 1
fi

# Берём аргументы из командной строки или переменных окружения
SERVER_ADDR="${1:-${VPS_SERVER_ADDR:-203.0.113.10:40000}}"
MY_GUID="${2:-${VPS_CLIENT_GUID}}"
SERVER_GUID="${3:-${VPS_SERVER_GUID}}"

# Проверяем, что все аргументы присутствуют
if [ -z "$MY_GUID" ] || [ -z "$SERVER_GUID" ]; then
    echo "Использование: sudo ./run-client.sh <server_ip:port> <my_guid> <server_guid>"
    echo
    echo "Или установите переменные окружения:"
    echo "  export VPS_SERVER_ADDR=203.0.113.10:40000"
    echo "  export VPS_CLIENT_GUID=550e8400-e29b-41d4-a716-446655440001"
    echo "  export VPS_SERVER_GUID=550e8400-e29b-41d4-a716-446655440000"
    echo "  sudo ./run-client.sh"
    exit 1
fi

echo "VPS-клиент:"
echo "  Сервер: $SERVER_ADDR"
echo "  Мой GUID: $MY_GUID"
echo "  GUID сервера: $SERVER_GUID"
echo "Для остановки нажмите Ctrl+C"
echo

# Берём переменные окружения для TUN (если они заданы)
export TUN_NAME="${TUN_NAME:-hp0}"
export TUN_MTU="${TUN_MTU:-1400}"
# export RUST_LOG="${RUST_LOG:-info}"

exec "$SCRIPT_DIR/../../target/release/vps-client" "$SERVER_ADDR" "$MY_GUID" "$SERVER_GUID"
