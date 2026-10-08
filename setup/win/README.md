# Установка с Windows (`setup\win\*.bat`)

Аналоги `setup/*.sh` для машины с Windows 10+: настроить Linux-сервер по ssh и поставить
`vps-client` на этот же ПК. Нужны `ssh` (OpenSSH из Windows, алиасы в `%USERPROFILE%\.ssh\config`)
и PowerShell 5.1. Сообщения в `.bat` — по-английски и ASCII: cmd с UTF-8 ломает метки `goto`/`call`.

| Скрипт | Что делает |
|---|---|
| `vps-server.bat <сервер>` | как `vps-server.sh`: `/opt/hp-vps` (бинарник, `vps.env`, `clients.txt`) и служба `hp-vps-server` |
| `vps-client.bat <сервер> [имя]` | этот ПК как клиент: GUID, строка в `clients.txt` сервера, сборка/копия `vps-client.exe`, `wintun.dll`, хуки, задача планировщика «home-proxy vps-client» (SYSTEM, при загрузке), проверка 10/10 дыр |
| `vps-client-remove.bat` | убрать клиента с этого ПК (сервер не трогает) |
| `vps-prepare.bat <сервер>` | фаервол/NAT сервера: запускает `setup/vps-prepare.sh` через bash из Git for Windows |
| `vps-client-router-vps-server.bat <сервер>` | сервер + этот ПК |

`vps-client.bat` запускать из консоли администратора. Состояние (GUID) — в `setup\state\`, как у
`.sh` (не коммитится). Если состояния сервера на этом ПК нет, адрес, порт и GUID сервера читаются
(только чтение) из `/opt/hp-vps/vps.env` на нём.

Windows не соберёт Linux-бинарь `vps-server`: соберите его на Linux/WSL
(`cargo build --release -p vps-server`) и укажите `VPS_SERVER_BIN=путь`.

## Осторожно с работающим сервером

- `vps-server.bat` **не придумывает новый GUID** серверу, у которого он уже есть: берёт его из
  `vps.env` (или отказывается, если он расходится с `setup\state`). Новый GUID потерял бы всех
  клиентов.
- Если на сервере уже есть `vps.env`, перед изменениями спрашивает подтверждение (`YES=1` — без
  вопроса). Служба перезапускается только если бинарник или `vps.env` отличаются.
- `DRY_RUN=1` — печатает команды ssh и план для этого ПК, ничего не меняет (для `vps-client.bat`
  нужен готовый `setup\state\<сервер>.server.env`: сервер не опрашивается).

Удалённые части — в `remote\*.sh` (LF, их читает `sh -s` на сервере).
