# `vps-client` — клиент сервера с белым IP

Клиент [`vps-server`](../vps-server/README.md): адрес сервера известен заранее, ни STUN, ни
MQTT, ни пробива. Поднимает интерфейс TUN и 10 дыр к серверу и возит IP-пакеты по дырам как
есть (нужен root и `/dev/net/tun`). На роутере OpenWrt вместо него —
[`hp-router`](../hp-router/README.md): то же плюс телефоны.

```bash
vps-client <ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера>
```

Порт знакомства по умолчанию 40000. Адрес в туннеле выдаёт сервер. Окружение: `TUN_NAME`
(`hp0`), `TUN_MTU` (1400), `REORDER_WAIT_MS` (начальное ожидание буфера порядка, 8; 0 —
выключить), `DATA_HOLES` (0 — данные через все живые дыры), `RUNTIME=multi`, `RUST_LOG`,
`LOG_TARGET=syslog` (на OpenWrt логи читаются `logread`). Маршруты в TUN — отдельно, см.
`../OpenWRT/Tun.md`.

Раньше клиент умел и мост для WireGuard на `127.0.0.1:<порт>`; от WireGuard отказались
(`../Performance.md`: на роутере 10–12 Мбит/с с WireGuard против 21 без него).

Сборка под OpenWrt (mipsel) — `../OpenWRT/README.md`, `OpenWRT/build.sh`.
