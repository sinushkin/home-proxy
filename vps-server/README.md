# `vps-server` — сервер с белым IP

Обычная схема «клиент — сервер» поверх библиотеки `hp-backend` (не P2P): ни STUN, ни MQTT, ни
пробива. Клиент ([`../vps-client`](../vps-client/README.md)) приходит на порт знакомства, сервер
раздаёт ему случайные порты слотов из диапазона и сам переносит дыру на другой порт при
просадках. Мост «дыры → WireGuard» и настройки — те же, что у [`hp-server`](../hp-server/README.md)
(крейт переиспользует `hp_server::{bridge, settings, Common, serve}`). Как устроен обмен —
`../hp-backend/connection/README.md`, «VPS-режим».

| Переменная | Смысл |
|---|---|
| `VPS_PUBLIC_IP` | белый IP сервера (обязателен) |
| `VPS_BOOTSTRAP_PORT` | порт знакомства, по умолчанию 40000 |
| `VPS_PORTS` | диапазон портов слотов `начало-конец`, по умолчанию `40001-49999` (порт знакомства вне его) |
| `MY_ID`, `PEER_ID` | GUID сервера и GUID клиента |
| `WG_ADDR`, `CLIENT_TIMEOUT_SECS`, `REORDER_WAIT_MS`, `DATA_HOLES`, `RUST_LOG`, `LOG_FILE` | как у `hp-server` |

Брандмауэр должен пропускать входящий UDP на порт знакомства и весь `VPS_PORTS`: порты слотов
случайные.

```bash
cargo build --release -p vps-server           # target/release/vps-server
./target/release/vps-server --config vps.env  # без --config: vps.env рядом с бинарником
```

Сервер обслуживает одного клиента (`PEER_ID`).

Режим TUN без WireGuard: `VPS_TUN_ADDR=10.80.0.1/24` (ещё `VPS_TUN_NAME`, `VPS_TUN_MTU`) — IP-пакеты
клиента идут в интерфейс `hp0` как есть; NAT подсети наружу настраивается отдельно. См.
`../OpenWRT/Tun.md`.
