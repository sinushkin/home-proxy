# wsl — home-proxy на Windows в лёгком образе WSL2

Домашний ПК на Windows (схема без роутера OpenWrt): `hp-server` работает внутри WSL2. Он сам
поднимает интерфейс TUN `hp0`, принимает IP-пакеты телефона по дырам и пишет их в TUN; дальше
пакеты выходят через NAT WSL и Windows — тем же путём, что и остальной трафик ПК (в том числе
через ваш домашний VPN). Образ на базе Alpine, несколько мегабайт в архиве.

```
телефон -> 10 дыр ==== интернет ==== WSL2 (Alpine): hp-server -> hp0 (10.80.0.1/16)
                                         -> iptables MASQUERADE -> eth0 -> Windows -> интернет
```

Раньше вместо TUN здесь был WireGuard (`wg-quick`), а до WSL — нативная служба Windows с
WireGuard и NAT через ICS; от обоих отказались: WireGuard лишний раз шифрует уже
зашифрованный HTTPS, а на Windows без WSL нет ни TUN, ни нормального NAT.

## Что в образе

- Alpine 3.20: `iptables` (nf_tables), `iproute2`.
- `/usr/local/bin/homeproxy-server` — `hp-server`, статический musl-бинарник.
- `/etc/wsl.conf`: `[boot] command` запускает `homeproxy-start` при старте дистрибутива,
  `[interop] enabled=false` и `appendWindowsPath=false` (из дистрибутива нельзя запускать
  программы Windows и они не попадают в `PATH`).
- `/usr/local/bin/homeproxy-start`: включает пересылку, ставит `MASQUERADE` для подсети
  туннеля (из `TUN_ADDR`, по умолчанию `10.80.0.0/16`) на интерфейс маршрута по умолчанию и
  запускает `hp-server` в цикле (перезапуск через 5 с при падении). Логи —
  `/var/log/homeproxy.log`. `/usr/local/bin/homeproxy-stop` останавливает службу, не
  останавливая WSL.
- systemd не нужен.

Настройки лежат в `/etc/homeproxy/` (образ приходит с пустым каталогом; без `server.env`
`homeproxy-start` только пишет об этом в лог):

| Файл | Что |
|---|---|
| `server.env` | как `hp-server/.env.example`: `STUN_ADDR`, `MQTT_ADDR`, `MQTT_CA=ca.crt`, `MY_ID`, `PEER_ID`, при желании `TUN_ADDR` |
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

Внутри `docker build` собирает `hp-server` в `rust:alpine` (`musl`, статически) и кладёт его в
чистый Alpine. Каталог `wsl/out/` в git не попадает. Перенесите архив на Windows.

## 3. Подготовить настройки

`server.env` — по образцу `hp-server/.env.example`: `MQTT_CA=ca.crt`, `MY_ID` — GUID ПК,
`PEER_ID` — GUID телефона (оба генерируются заранее, например `uuidgen`; телефону нужны те же два
GUID наоборот). Адрес в туннеле и DNS телефону выдаёт сам `hp-server` (DNS — резолверы из
`resolv.conf` дистрибутива или `DNS=` в `server.env`, в конце 8.8.8.8 и 1.1.1.1). Файлы должны
быть с окончаниями строк LF.

## 4. Импортировать и запустить

В PowerShell на Windows:

```powershell
wsl --import homeproxy C:\homeproxy-wsl C:\путь\homeproxy-wsl.tar.gz --version 2
wsl -d homeproxy -u root --exec true            # первый запуск, чтобы появились каталоги
```

Положить настройки в дистрибутив (каталог виден из Windows как
`\\wsl.localhost\homeproxy\etc\homeproxy\`):

```powershell
Copy-Item .\server.env, .\ca.crt \\wsl.localhost\homeproxy\etc\homeproxy\
wsl --terminate homeproxy
wsl -d homeproxy -u root --exec true            # boot command ставит NAT и запускает hp-server
```

Проверка:

```powershell
wsl -d homeproxy -u root --exec ip addr show hp0
wsl -d homeproxy -u root --exec tail -n 30 /var/log/homeproxy.log
```

В логе должны появиться строки `NAT 10.80.0.0/16 -> eth0`, `рандеву (MQTT): подключено`,
дальше — `дыра открыта`.

## 5. Держать WSL запущенным

WSL2 останавливает дистрибутив, как только закрывается последняя сессия `wsl.exe`, **даже
если внутри работает `hp-server`**: фоновые процессы из `boot command` его не удерживают
(проверено: после завершения `ssh`-команды с `wsl.exe` через секунды служба пропала, а все дыры
пира потерялись). Поэтому нужна постоянная сессия. Простейший способ — фоновый процесс:

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
  надёжнее. `homeproxy-start` берёт внешний интерфейс из маршрута по умолчанию.
- **VPN на Windows.** Если весь трафик ПК идёт через VPN, STUN должен видеть тот же адрес, с
  которого идут дыры: [`../windows/stun-bypass.ps1`](../windows/README.md).
- **Localhost forwarding.** Порт, слушающий `127.0.0.1` внутри WSL2, доступен из Windows как
  `127.0.0.1` (в NAT-режиме работает `localhostForwarding`, в `mirrored` loopback общий). Это
  нужно для будущего `control` (трей на Windows обращается к `hp-server` в WSL2 по TCP).

## 7. Обновление и удаление

Новый образ: `wsl --unregister homeproxy` (удаляет весь дистрибутив вместе с
`/etc/homeproxy`) и повторный импорт, поэтому настройки держите у себя. Остановить службу без
удаления: `wsl -d homeproxy -u root --exec homeproxy-stop`. Убрать всё:
`schtasks /Delete /TN homeproxy-wsl /F` и `wsl --unregister homeproxy`.

## Если вы ставите Ubuntu вместо образа

То же можно сделать руками: `apt install iptables iproute2`, положить `homeproxy-server`
(статический бинарник из образа или своя сборка) и запускать по `[boot] command` в
`/etc/wsl.conf` (или через systemd: `[boot] systemd=true`). Файлы `homeproxy-start` и
`wsl.conf` из `wsl/rootfs/` подойдут как есть (`ipcalc` в Ubuntu другой — подсеть проще
вписать в скрипт руками).

## Что проверено

- **В настоящем WSL2** (тестовая ВМ Windows 10 22H2 под KVM, WSL 2.7.14, ядро 6.18.33.2), ещё
  в варианте с WireGuard: `wsl --import` проходит, `[boot] command` при старте дистрибутива
  срабатывает, NAT ставится, служба подключается к MQTT, из дистрибутива есть выход в интернет.
- Пробив: `peer` на VPS без NAT открыл 10 из 10 дыр к службе в WSL2 (NAT Windows/WSL, NAT
  libvirt, NAT роутера) и держал их около двух минут без потерь. Условие: дистрибутив
  удерживается сессией `wsl.exe`.

**Не проверено:** вариант с TUN в настоящем WSL2 и телефон через него, задача планировщика
(`ONLOGON`), режим `mirrored` (нужна Windows 11).
