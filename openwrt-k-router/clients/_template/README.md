# Шаблон клиента роутера

Что нужно на каждый роутер (в старом проекте — `clients/104`, OpenVPN; теперь — `vps-client`):

| Что | Где | Из чего |
|---|---|---|
| Туннель | `/usr/bin/vps-client`, `/etc/vps-client/` | `setup/vps-client.sh <роутер> <сервер>` |
| DNS от сервера | `/etc/vps-client/on-tun-up.sh` | `setup/vps-client-hooks/` |
| UDP напрямую | `/etc/vps-client/udp-direct-up.sh` (копия `openwrt-k-router/udp-direct/up.sh`) | см. ../README.md |
| Настройки клиента | `env` рядом с `udp-direct` | `env.example` здесь |

Порядок: 1) `setup/vps-server.sh <сервер>` (один раз); 2) `setup/vps-client.sh <роутер> <сервер>`
(добавляет клиента в `clients.txt`, сервер подхватит без перезапуска); 3) хук `udp-direct` (см. ../README.md).

Не переносить из старого проекта: `passwords.txt`, ключи (`*.key`, `id_*`), `.ovpn`, бэкапы
роутера (`backup-OpenWrt-*.tar.gz`, там пароли и ключи), `up.sh`/`down.sh` под OpenVPN (`tun1`).
