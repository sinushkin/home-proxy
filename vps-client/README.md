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

## Управление: `hpctl` и трей

`vps-client` умеет то же, что `hp-server` и `hp-router`: отдавать состояние по протоколу
управления, `hpctl` и трей подключаются к нему так же (строка `homeproxy-control://…`).
По умолчанию выключено; включается переменной `CONTROL_ADDR` (`127.0.0.1:47001` или адрес
LAN роутера; `0.0.0.0` нельзя). Ключ — `CONTROL_KEY_FILE`, по умолчанию `control.key` рядом с
хуками (`/etc/vps-client/`, на Windows `%ProgramData%\vps-client\`).

```bash
CONTROL_ADDR=192.168.1.1:47001 vps-client --connection-string   # печатает строку (создаёт ключ)
vps-client --new-connection-string                              # новый ключ, прежние строки не работают
hpctl --connect 'homeproxy-control://192.168.1.1:47001/<ключ>' status
```

`status` показывает один «пир» — VPS-сервер: живые дыры в работе, у каждой номер, адрес, возраст,
признак «сливается», счётчики и потери в обе стороны; ниже — трафик моста. Набор дыр
динамический: `HOLES_MIN`, `HOLES_MAX`, `HOLE_AGE` (по умолчанию `180-600`, секунды). На роутере с `setup/vps-client.sh`
включить так: `CONTROL_ADDR=192.168.1.1:47001 setup/vps-client.sh <клиент> <сервер>`. Строку подключения
показывает и страница LuCI (**Службы → Home Proxy**, `OpenWRT/luci-app-homeproxy`) — для неё
нужен адрес LAN в `CONTROL_ADDR`, `vps-client --config /etc/vps-client/vps-client.conf
--connection-string` читает его из файла настроек. Срок жизни дыры — `HOLE_AGE`, по умолчанию 3–10 минут (`180-600`).

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
