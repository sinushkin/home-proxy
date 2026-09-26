# windows — служба home-proxy на Windows

Домашний ПК на Windows вместо Linux: те же две части, что в [`wireguard/`](../wireguard/README.md),
только под Windows.

```
телефон -> 10 дыр ==== интернет ==== ПК: служба homeproxy-server (hp-server.exe)
                                      -> 127.0.0.1:51820 -> WireGuard-туннель wghp (10.77.0.1)
                                      -> NAT (New-NetNat) -> интернет
```

`install.ps1` ставит всё сразу: туннель WireGuard `wghp`, NAT для его подсети, правило
брандмауэра, службу `homeproxy-server` (запускается после туннеля, перезапускается при
сбое) и обход VPN для STUN. `uninstall.ps1` всё убирает.

## 1. Что нужно

- Windows 10/11 Pro, права администратора, PowerShell 5.1 (штатный).
- [WireGuard для Windows](https://www.wireguard.com/install/): `winget install WireGuard.WireGuard`.
- NAT: `New-NetNat` (на обычной Windows он есть, проверка: `Get-NetNat` не должен падать с
  «Недопустимый класс»). Если его нет, `install.ps1` сам возьмёт общий доступ к интернету
  (ICS, подсеть туннеля должна быть `/24`). Режим задаёт `-Nat Auto|NetNat|Ics|None`.
- Ключи и GUID'ы: `wireguard/gen.sh` на Linux (нужен `wg`), результат в `wireguard/out/`.
- `hp-server.exe`. Собирается на самой Windows: Rust (MSVC), [`protoc`](https://github.com/protocolbuffers/protobuf/releases)
  в `PATH` или в `PROTOC`, затем из корня репозитория:

  ```powershell
  cargo build --release -p hp-server        # target\release\hp-server.exe
  ```

## 2. Подготовить каталог

Положите в один каталог (например, `C:\hp-src`):

| Файл | Откуда |
|---|---|
| `hp-server.exe` | сборка выше |
| `server.env` | из [`server.env.example`](server.env.example): адреса STUN/MQTT, `MY_ID = PC_ID`, `PEER_ID = PHONE_ID` |
| `wghp.conf` | `wireguard/out/wghp.conf` (строки `PostUp`/`PostDown` скрипт отбросит: WireGuard для Windows их не выполняет) |
| `ca.crt` | `cert/out/ca.crt` (путь в `MQTT_CA`) |
| `install.ps1`, `uninstall.ps1`, `stun-bypass.ps1` | этот каталог |

## 3. Установить

Из PowerShell **от администратора**:

```powershell
powershell -ExecutionPolicy Bypass -File C:\hp-src\install.ps1 -SourceDir C:\hp-src
Get-Service homeproxy-server, 'WireGuardTunnel$wghp'
Get-Content C:\ProgramData\homeproxy\server.log -Tail 20 -Wait
```

Всё копируется в `C:\ProgramData\homeproxy` (доступ только SYSTEM и администраторам:
там приватный ключ). Параметры: `-InstallDir`, `-Nat` (по умолчанию `Auto`), `-SkipNat` = `-Nat None` (NAT настроен иначе),
`-SkipStunBypass`. Скрипт можно запускать повторно (обновление конфигов и `hp-server.exe`).

Сама служба управляется и без скриптов: `hp-server.exe install --config C:\путь\server.env`
регистрирует её, `hp-server.exe uninstall` удаляет, `hp-server.exe --config server.env` запускает
обычным процессом в консоли (для отладки).

## 4. Обход VPN для STUN

Если ПК сам выходит в интернет через VPN, STUN-запрос может уйти не тем маршрутом, каким
уходят пакеты к телефону, и телефон получит адрес, с которого дыры не открываются.
`stun-bypass.ps1` берёт `STUN_ADDR` из `server.env` и для каждого адреса:

- если маршрут идёт через физический адаптер — ничего не делает;
- если через виртуальный (VPN, WireGuard и т. п.) — добавляет постоянный маршрут
  `<stun>/32` через шлюз физического адаптера.

```powershell
.\stun-bypass.ps1 -EnvFile C:\ProgramData\homeproxy\server.env -WhatIf   # только показать
.\stun-bypass.ps1 -EnvFile C:\ProgramData\homeproxy\server.env           # применить
.\stun-bypass.ps1 -EnvFile C:\ProgramData\homeproxy\server.env -Remove   # убрать свои маршруты
```

После смены сети или шлюза запустите его ещё раз. **Ограничение:** обходятся только адреса
STUN-серверов, а пакеты пробива к телефону идут по обычным маршрутам ПК. Чтобы они тоже
шли мимо VPN, нужны правила самого VPN-клиента (исключение UDP или сплит-туннель).

## 5. Удалить

```powershell
powershell -ExecutionPolicy Bypass -File C:\ProgramData\homeproxy\uninstall.ps1
```

Убирает службу, туннель, NAT, правило брандмауэра, маршруты STUN и каталог установки
(`-KeepFiles` — оставить файлы). Включённая пересылка (`Forwarding`) на интерфейсах и
сам WireGuard остаются.

## Что проверено

На тестовой ВМ (Windows 10 Pro 22H2), пир — `peer` на VPS без NAT:

- служба ставится, стартует после туннеля, поднимается сама после перезагрузки ВМ;
- 10 из 10 дыр открываются, сообщение от пира создаёт «прямого» клиента и уходит на
  `127.0.0.1:51820`, где слушает WireGuard-туннель;
- `wghp.conf` без `PostUp`/`PostDown`, адаптер `wghp` получает `10.77.0.1/24`;
- `stun-bypass.ps1` (эмуляция VPN маршрутом `0.0.0.0/1` через `wghp`): обнаружение,
  `-WhatIf`, добавление, повторный запуск, `-Remove`;
- `uninstall.ps1` убирает всё; 13 тестов `server` проходят на Windows;
- **с настоящим телефоном** (Android за точкой доступа iPhone, отдельная пара GUID и ключей
  WireGuard): 10 из 10 дыр держатся с обеих сторон, WireGuard на Windows получает
  рукопожатие через дыры (`wg show wghp`: `latest handshake`, `endpoint: 127.0.0.1:<порт клиента>`),
  `ping 10.77.0.1` с телефона — 4 из 4, `ttl=128`.

- NAT через ICS (на ВМ `New-NetNat` недоступен): с телефона через туннель проходят
  `ping 8.8.8.8` и `ping google.com`; настройки ICS переживают перезагрузку, `uninstall.ps1`
  их снимает.

**Не проверено:** путь через `New-NetNat` (на ВМ его нет) и устойчивость дыр на точке доступа
iPhone (симметричный NAT): в части прогонов дыры пропадали. Подробности и ловушки — в
[`../hp-server/WINDOWS.md`](../hp-server/WINDOWS.md).
