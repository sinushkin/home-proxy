# wsl — home-proxy в лёгком образе WSL2

Вместо нативной службы Windows (`../windows/`) вся Linux-часть работает внутри WSL2:
WireGuard с NAT и `server` (дыры → WireGuard). Образ на базе Alpine, около 7 МБ в архиве.

```
телефон -> 10 дыр ==== интернет ==== WSL2 (Alpine): server -> 127.0.0.1:51820 -> wghp (10.77.0.1)
                                                    -> iptables MASQUERADE -> eth0 -> Windows -> интернет
```

Всё то, что на Windows приходилось обходить (`PostUp` не выполняется, нет NAT-модуля,
брандмауэр, служба под SYSTEM), здесь работает как на Linux: `wg-quick` с `PostUp`,
`iptables`, `wg set`.

## Что в образе

- Alpine 3.20: `wireguard-tools`, `iptables` (nf_tables), `iproute2`.
- `/usr/local/bin/homeproxy-server` — `server`, статический musl-бинарник (3,3 МБ).
- `/etc/wsl.conf`: `[boot] command` запускает `homeproxy-start` при старте дистрибутива,
  `[interop] enabled=false` и `appendWindowsPath=false` (из дистрибутива нельзя запускать
  программы Windows и они не попадают в `PATH`).
- `/usr/local/bin/homeproxy-start`: поднимает `wg-quick up wghp` и запускает `server` в цикле
  (перезапуск через 5 с при падении). Логи — `/var/log/homeproxy.log`.
  `/usr/local/bin/homeproxy-stop` останавливает службу и туннель, не останавливая WSL.
- systemd не нужен.

Настройки лежат в `/etc/homeproxy/` (образ приходит с пустым каталогом; без файлов
`homeproxy-start` только пишет об этом в лог):

| Файл | Что |
|---|---|
| `server.env` | как `server/.env`: `STUN_ADDR`, `MQTT_ADDR`, `MQTT_CA=ca.crt`, `MY_ID`, `PEER_ID` |
| `wghp.conf` | конфиг WireGuard с `PostUp`/`PostDown` (их делает `wireguard/gen.sh`) |
| `ca.crt` | CA-сертификат брокера (путь из `MQTT_CA` считается от каталога `server.env`) |

## 1. Что нужно

- Windows 10 22H2 или Windows 11 с включённой виртуализацией (BIOS/UEFI и, для ВМ,
  вложенная виртуализация).
- WSL 2 с поддержкой `[boot] command` (WSL 0.67.6 и новее): `wsl --install --no-distribution`,
  затем `wsl --update`. Проверка: `wsl --version`.
- Для сборки образа: Docker на любой Linux-машине.

## 2. Собрать образ

На Linux, из корня репозитория:

```bash
wsl/build.sh          # результат: wsl/out/homeproxy-wsl.tar.gz
```

Внутри `docker build` собирает `server` в `rust:alpine` (`musl`, статически) и кладёт его в
чистый Alpine. Каталог `wsl/out/` в git не попадает. Перенесите архив на Windows.

## 3. Подготовить настройки

Ключи, GUID'ы и `wghp.conf` делает `wireguard/gen.sh`. В WSL2 внешний интерфейс — `eth0`, а
`gen.sh` по умолчанию берёт интерфейс своей машины, поэтому укажите его явно:

```bash
WG_OUT_IFACE=eth0 wireguard/gen.sh          # или WG_OUT_DIR=out/wsl WG_OUT_IFACE=eth0 ./gen.sh
```

`server.env` создайте по образцу `server/.env.example`, `MQTT_CA=ca.crt`, `MY_ID = PC_ID`,
`PEER_ID = PHONE_ID` из `guids.env`. Файлы должны быть с окончаниями строк LF.

## 4. Импортировать и запустить

В PowerShell на Windows:

```powershell
wsl --import homeproxy C:\homeproxy-wsl C:\путь\homeproxy-wsl.tar.gz --version 2
wsl -d homeproxy -u root --exec true            # первый запуск, чтобы появились каталоги
```

Положить настройки в дистрибутив (каталог виден из Windows как
`\\wsl.localhost\homeproxy\etc\homeproxy\`):

```powershell
Copy-Item .\server.env, .\wghp.conf, .\ca.crt \\wsl.localhost\homeproxy\etc\homeproxy\
wsl --terminate homeproxy
wsl -d homeproxy -u root --exec true            # boot command поднимает туннель и server
```

Проверка:

```powershell
wsl -d homeproxy -u root --exec wg show wghp
wsl -d homeproxy -u root --exec tail -n 30 /var/log/homeproxy.log
```

В логе `server` должны появиться строки `рандеву (MQTT): подключено`, дальше — `дыра открыта`.

## 5. Держать WSL запущенным

WSL2 останавливает дистрибутив, как только закрывается последняя сессия `wsl.exe`, **даже
если внутри работает `server`**: фоновые процессы из `boot command` его не удерживают (проверено:
после завершения `ssh`-команды с `wsl.exe` через секунды `server` пропал, а все дыры пира
потерялись). Поэтому нужна постоянная сессия. Простейший способ — фоновый процесс:

```powershell
wsl -d homeproxy -u root --exec sleep infinity          # держит дистрибутив запущенным
schtasks /Create /SC ONLOGON /TN homeproxy-wsl /TR "wsl.exe -d homeproxy -u root --exec sleep infinity"
```

Ограничение: задача стартует при входе пользователя, не как служба Windows. WSL2 всё равно
принадлежит пользователю, который импортировал дистрибутив.

## 6. Сеть

- **NAT-режим (по умолчанию).** У WSL2 свой адрес (172.x) за NAT Windows, а тот за домашним
  роутером. Для пробива это двойной NAT: порт, который видит STUN, может расходиться с
  реальным. Перебор портов это частично лечит.
- **`mirrored`** (Windows 11 22H2 и новее). В `%UserProfile%\.wslconfig`:
  ```
  [wsl2]
  networkingMode=mirrored
  ```
  Тогда WSL2 делит сетевые интерфейсы с Windows (тот же адрес, общий loopback), пробив
  надёжнее, а порты можно открывать напрямую. Имя внешнего интерфейса в `wghp.conf` (`PostUp`)
  проверьте командой `ip route` внутри дистрибутива.
- **Localhost forwarding.** Порт, слушающий `127.0.0.1` внутри WSL2, доступен из Windows как
  `127.0.0.1` (в NAT-режиме работает `localhostForwarding`, в `mirrored` loopback общий). Это
  нужно для будущего `control` (трей на Windows обращается к `server` в WSL2 по TCP).

## 7. Обновление и удаление

Новый образ: `wsl --unregister homeproxy` (удаляет весь дистрибутив вместе с
`/etc/homeproxy`) и повторный импорт, поэтому настройки держите у себя. Остановить службу без
удаления: `wsl -d homeproxy -u root --exec homeproxy-stop`. Убрать всё:
`schtasks /Delete /TN homeproxy-wsl /F` и `wsl --unregister homeproxy`.

## Если вы ставите Ubuntu вместо образа

То же можно сделать руками: `apt install wireguard-tools iptables`, положить
`homeproxy-server` (статический бинарник из образа или своя сборка) и запускать по
`[boot] command` в `/etc/wsl.conf` (или через systemd: `[boot] systemd=true`). Файлы
`homeproxy-start` и `wsl.conf` из `wsl/rootfs/` подойдут как есть.

## Что проверено

- Образ собирается: 7 МБ в архиве, `server` 3,3 МБ. В привилегированном контейнере с одноразовой
  парой ключей и GUID `homeproxy-start` поднимает `wghp`, ставит `MASQUERADE`, `server`
  подключается к MQTT.
- **В настоящем WSL2** (тестовая ВМ Windows 10 22H2 под KVM, WSL 2.7.14, ядро 6.18.33.2):
  `wsl --import` проходит, `[boot] command` при старте дистрибутива поднимает `wghp` (ядерный
  WireGuard в ядре WSL есть), правило `MASQUERADE` ставится, `server` подключается к MQTT,
  из дистрибутива есть выход в интернет (`ping 8.8.8.8`).
- Пробив: `peer` на VPS без NAT открыл 10 из 10 дыр к `server` в WSL2 (NAT Windows/WSL, NAT
  libvirt, NAT роутера) и держал их около двух минут без потерь, сообщение от пира создало
  «прямого» клиента в `server`. Условие: дистрибутив удерживается сессией `wsl.exe`.

**Не проверено:** телефон и рукопожатие WireGuard через дыры из WSL2, задача планировщика
(`ONLOGON`), режим `mirrored` (нужна Windows 11).
