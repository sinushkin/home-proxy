# `vps-client` — клиент сервера с белым IP

Клиент [`vps-server`](../vps-server/README.md): адрес сервера известен заранее, ни STUN, ни
MQTT, ни пробива. Поднимает интерфейс TUN и динамический набор дыр к серверу (4..10, дыры стареют и заменяются) и возит IP-пакеты по дырам как
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

## Windows 10+

Тот же бинарь собирается под Windows (`cargo build --release -p vps-client`, нужен `protoc`).
Нужны: права администратора и `wintun.dll` (<https://www.wintun.net>, `bin/amd64`) рядом с
`vps-client.exe` или путь в `WINTUN_DLL`. Запуск — как на Linux:

```powershell
vps-client.exe <ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера>
```

- TUN — Wintun (адаптер `hp0`, метрика интерфейса сбрасывается в 1, чтобы `/1` в туннель выиграли
  у чужого VPN); маршруты — PowerShell (`Get-NetRoute`/`New-NetRoute`), поэтому не зависят от
  языка Windows; `/32` до сервера идёт через прежний аплинк.
- Хуки — `%ProgramData%\vps-client\on-tun-up.ps1` и `on-tun-down.ps1` (образцы —
  `setup/vps-client-hooks/`: DNS на адаптере туннеля); `ON_TUN_UP` / `ON_TUN_DOWN` переопределяют.
- Остановка — Ctrl+C / Ctrl+Break / закрытие консоли / выключение системы: маршруты и хук `down`
  отрабатывают. Службы Windows (SCM) пока нет — запуск из консоли или планировщиком.
- Адаптер Wintun и его маршруты исчезают вместе с процессом; остаётся только `/32` до сервера
  (безвредный, как и на Linux).
- Проверено на Windows 10 (русской): набор дыр, пинг до адреса сервера через туннель, DNS-хук,
  весь трафик в `hp0`, скачивание ~5 МБ/с, остановка без хвостов.

Сборка под OpenWrt (mipsel) — `../OpenWRT/README.md`, `OpenWRT/build.sh`.
