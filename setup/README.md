# setup — установка и связка служб

Скрипты запускаются с рабочей машины по ssh (алиасы из `~/.ssh/config`), состояние и GUID
хранят в `setup/state/` (в `.gitignore`, права 600). Повторный запуск безопасен. Windows — `setup/win/`
(`.bat`-аналоги, см. `win/README.md`). Полные GUID — секрет пары, в git и в логи не попадают.

## 0. Свежий VPS: только root и пароль

Провайдер выдаёт `root` и пароль; остальные скрипты ходят по ключу. Один раз:

```bash
SSHPASS='пароль' setup/ssh-key.sh root@<ip>      # без SSHPASS спросит пароль сам
```

Ключ — `~/.ssh/id_ed25519.pub` (`KEY=…` — другой; нет — создаётся). Дальше вместо ssh-алиаса можно
писать `root@<ip>`. Проверка на чистых системах в Docker — `tests/README.md`.

## 1. Связка vps-client ↔ vps-server

Сервер — машина с белым IP (x86_64, Debian/Ubuntu, systemd), клиент — роутер OpenWrt (mipsel) или
Linux x86_64.

```bash
# собрать для роутера (по одному пакету за вызов, иначе vps-client раздуется до размера hp-router)
PACKAGES=vps-client TOOLCHAIN_DIR=~/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl OpenWRT/build.sh

setup/vps-prepare.sh <сервер>          # один раз: ip_forward, NAT, порты в ufw
setup/vps-server.sh  <сервер>          # vps-server → /opt/hp-vps, служба hp-vps-server (порт 40600)
setup/vps-client.sh  <клиент> [сервер] # клиент: GUID, строка в clients.txt сервера, служба, хуки DNS
# сервер + клиент одной командой:
setup/vps-client-router-vps-server.sh <сервер> <клиент>
```

- `DRY_RUN=1 setup/vps-prepare.sh <сервер>` — только напечатать команды.
- Каждый новый клиент — отдельный вызов `vps-client.sh`; сервер подхватывает строку в
  `clients.txt` за ~2 с без перезапуска.
- Проверка: в конце скрипта — не меньше 4 дыр в работе и маршруты; потом `hpctl --connect <строка> status`.
- Порты: знакомство `40600/udp`, дыры `40601–40699/udp`, туннель `10.94.0.0/24`.

## 2. Связка hp-client (телефон) ↔ hp-server

`hp-server` — домашний ПК: телефон пробивается к нему через STUN + MQTT (TLS), пакеты выходят в
интернет с ПК. Нужны доступные STUN и MQTT (свои — `docker-compose.yml`, `cert/README.md`).

1. Брокер и сертификаты: `cert/README.md`; `ca.crt` нужен и службе, и телефону.
2. Собрать `hp-server` (`cargo build --release -p hp-server`), настройки — `hp-server/.env.example`
   (`STUN_ADDR`, `MQTT_ADDR`, `MQTT_CA`; пары GUID можно не задавать — телефон добавляется из
   трея, см. раздел 4).
3. Linux (`MODE=tun`): `sudo hp-server/run.sh` и один раз NAT:
   ```bash
   sysctl -w net.ipv4.ip_forward=1
   iptables -t nat -A POSTROUTING -s 10.80.0.0/16 -o <внешний интерфейс> -j MASQUERADE
   ```
   Windows (`MODE=netstack`, без прав и NAT): `windows/README.md`.
4. Приложение на телефон (сборка — `android-vpn/README.md`):
   ```bash
   setup/android-install.sh                # adb install -r app-debug.apk
   ```
5. Сопряжение — раздел 4. Приложение и `hp-server` обновляйте вместе (версии P2P несовместимы).

## 3. hp-router (2 в 1: шлюз к VPS + пир для телефонов)

Заменяет `vps-client` на роутере. Нужен работающий `vps-server` (раздел 1: на нём в `clients.txt`
должен стоять GUID роутера) и брокер STUN/MQTT.

```bash
PACKAGES=hp-router TOOLCHAIN_DIR=~/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl OpenWRT/build.sh
scp target/openwrt/mipsel-unknown-linux-musl/release/hp-router <роутер>:/usr/bin/hp-router
ssh <роутер> 'mkdir -p /etc/hp-router'
scp hp-router/router.env.example <роутер>:/etc/hp-router/router.env     # затем заполнить
scp cert/out/ca.crt <роутер>:/etc/hp-router/ca.crt
OpenWRT/luci-app-homeproxy/install.sh root@<роутер>    # LuCI-страница + procd-служба /etc/init.d/hp-router
```

В `/etc/hp-router/router.env`: `VPS_SERVER`, `VPS_MY_ID` (GUID роутера из `clients.txt`),
`VPS_PEER_ID` (GUID сервера из `/opt/hp-vps/vps.env`), `STUN_ADDR`, `MQTT_ADDR`, `MQTT_CA`,
`CONTROL_ADDR=<адрес LAN роутера>:47001`. Хуки DNS (`ON_TUN_UP`/`ON_TUN_DOWN`) — как у `vps-client`.

```bash
/etc/init.d/hp-router start && /etc/init.d/hp-router enable   # логи: logread -e hp-router
```

Проверка: `hp0` есть, `ping 8.8.8.8` идёт, `hpctl status` — не меньше 4 дыр к VPS.
Менять работающий шлюз — только с запасным путём отката (замена `vps-client` на `hp-router` и
образец скрипта с автооткатом — `OpenWRT/Tun.md`). Подробно — `hp-router/README.md`.

## 4. Добавление телефона к существующей системе

Телефон добавляется на ходу, без правки настроек и перезапуска службы — `hp-server` или `hp-router`
сами заводят пару GUID и набор дыр.

1. Строка подключения службы (один раз):
   - `hp-server --connection-string`, `hp-router --config /etc/hp-router/router.env --connection-string`
     или LuCI → Службы → Home Proxy → «Скопировать». Утекла — `--new-connection-string`
     (старые строки перестают работать сразу).
2. Трей `hp-tray`: «Служба…» → вставить строку → «Добавить телефон» (QR). Без графики:
   `hpctl --connect '<строка>' pair` — печатает ссылку `homeproxy://pair?d=…`.
3. На телефоне (приложение установлено, раздел 2):
   - кнопка «Сканировать QR (сопряжение)» или штатная камера; или с ПК:
     `adb shell am start -a android.intent.action.VIEW -d '<ссылка>' ru.homeproxy`;
   - «1. Подключить» → «2. Включить VPN».
4. Телефон сохранится в `peers.state` (`hp-server`) / `phones.state` (`hp-router`) после первого
   подключения. Снять: трей или `hpctl --connect '<строка>' remove <имя>`.

Ссылка сопряжения — секрет пары (содержит GUID); не публиковать. Проверка: в трее у телефона не
меньше 4 дыр, потери ~0%.
