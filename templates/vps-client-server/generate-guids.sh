#!/bin/bash
# Генерация GUID для сервера и клиента

set -e

echo "Генерирую GUID для VPS-сервера и VPS-клиента..."
echo

SERVER_GUID=$(uuidgen)
CLIENT_GUID=$(uuidgen)

echo "GUID сервера (MY_ID):"
echo "  $SERVER_GUID"
echo

echo "GUID клиента (PEER_ID):"
echo "  $CLIENT_GUID"
echo

echo "Сохраняю в переменные окружения (добавьте в .bashrc или .bash_profile):"
echo
echo "  export VPS_SERVER_GUID='$SERVER_GUID'"
echo "  export VPS_CLIENT_GUID='$CLIENT_GUID'"
echo

echo "Или скопируйте прямо в конфиг:"
echo
echo "vps-server.env:"
echo "  MY_ID=$SERVER_GUID"
echo "  PEER_ID=$CLIENT_GUID"
echo

echo "Команда запуска клиента:"
echo "  vps-client <server_ip:port> $CLIENT_GUID $SERVER_GUID"
echo
