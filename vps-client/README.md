# `vps-client` — клиент сервера с белым IP

Клиент [`vps-server`](../vps-server/README.md): адрес сервера известен заранее, ни STUN, ни
MQTT, ни пробива. Поднимает 10 дыр к серверу и мост для WireGuard на `127.0.0.1:<порт>`
(мост — `hp_client::bridge`, тот же, что у приложения на телефоне). В конфиге WireGuard
клиента `Endpoint = 127.0.0.1:<порт моста>`.

```bash
vps-client <ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера> [порт моста]
```

Порт знакомства по умолчанию 40000, порт моста — 51821. Окружение: `REORDER_WAIT_MS`
(начальное ожидание буфера порядка, 8; 0 — выключить), `DATA_HOLES` (0 — данные через все
живые дыры), `RUST_LOG`, `LOG_TARGET=syslog` (на OpenWrt логи читаются `logread`).

Сборка под OpenWrt (mipsel) — `../OpenWRT/README.md`, `OpenWRT/build.sh`.
