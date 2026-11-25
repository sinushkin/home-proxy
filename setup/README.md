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
- Роутер с работающим OpenVPN (default через `tun1`): сначала остановить OpenVPN (`/etc/init.d/openvpn stop`), иначе `vps-client` примет `tun1` за аплинк и
  заведёт маршрут до сервера через него. Адреса, которые должны идти мимо туннеля, — в `/etc/vps-client/direct-list.txt`.
- Проверка: в конце скрипта — не меньше 4 дыр в работе и маршруты; потом `hpctl --connect <строка> status`.
- Порты: знакомство `40600/udp`, дыры `40601–40699/udp`, туннель `10.94.0.0/24`.

## 1a. Управление клиентом (`hpctl`, трей)

По умолчанию у `vps-client` управление выключено. Включает его `CONTROL_ADDR`: `127.0.0.1:47001`
(только сама машина) или конкретный адрес LAN (`0.0.0.0` нельзя). Ключ — `control.key` в
`/etc/vps-client/`, создаётся при первом запросе строки подключения.

**Сразу** — задать при установке клиента:

```bash
CONTROL_ADDR=127.0.0.1:47001 setup/vps-client.sh <клиент> [сервер]
```

**Потом** — клиент уже стоит. Либо повторить установку с `CONTROL_ADDR` (GUID и строка в
`clients.txt` не меняются, конфиг переписывается, служба перезапускается), либо вручную на клиенте:

```bash
echo 'CONTROL_ADDR=127.0.0.1:47001' >> /etc/vps-client/vps-client.conf
systemctl restart hp-vps-client        # Linux; на OpenWrt — /etc/init.d/vps-client restart
```

**Строка подключения и проверка:**

```bash
# на клиенте (создаёт ключ; --new-connection-string — новый ключ, прежние строки перестают работать)
vps-client --config /etc/vps-client/vps-client.conf --connection-string
# на своём ПК: пробросить порт и посмотреть статус (для адреса LAN туннель не нужен)
ssh -N -L 47001:127.0.0.1:47001 <клиент>
hpctl --connect 'homeproxy-control://127.0.0.1:47001/<ключ>' status
```

Та же строка подходит для трея. Строка — секрет. Подробнее — `control/README.md`,
`vps-client/README.md`.

**Трей на самом клиенте (Linux x86_64 с рабочим столом) — одной командой:**

```bash
setup/tray.sh <клиент>      # DRY_RUN=1 — только проверки и сборка, на клиенте ничего не меняется
```

Скрипт собирает `hp-tray` и `hpctl` здесь, копирует их в `~/.local/bin` пользователя ssh, ставит
user-службу `hp-tray.service` и ярлык автозапуска, добавляет `CONTROL_ADDR=127.0.0.1:47001` в
`/etc/vps-client/vps-client.conf` (если не задан; `hp-vps-client` при этом перезапускается) и кладёт
строку подключения в `~/.config/home-proxy/tray.conf`. Туннель не нужен: трей и служба на одной
машине. Нужны: ssh под пользователем рабочего стола (не root), `sudo` без пароля, поставленный
`setup/vps-client.sh`. Служба стартует при входе на рабочем столе клиента (по ssh графической сессии
нет — это не ошибка); без графики пользуйтесь `hpctl` на клиенте. Скрипт доставляет недостающую
`libxkbcommon-x11-0` (apt-get; `hp-tray` подгружает её через `dlopen`, без неё падает) и кладёт ярлык
«Home Proxy» (`control/tray/hp-tray.desktop`) на рабочий стол и в меню приложений: он открывает окно
со статусом. Нужен там, где нет панели со значками трея (например, голый `openbox`: в логе «значок в
трее недоступен»). Второй экземпляр рядом с фоновой службой работает независимо; ярлык файловый менеджер
может попросить пометить доверенным.

**Трей на другом ПК (вручную): деплой и запуск как user-service** (Linux, из корня репозитория):

```bash
cargo build --release -p hp-tray -p hp-control        # hp-tray и hpctl
install -Dm755 target/release/hp-tray ~/.local/bin/hp-tray
install -Dm755 target/release/hpctl   ~/.local/bin/hpctl
install -Dm644 control/tray/hp-tray.service           ~/.config/systemd/user/hp-tray.service
install -Dm644 control/tray/hp-tray-autostart.desktop ~/.config/autostart/hp-tray.desktop
systemctl --user daemon-reload
systemctl --user enable --now hp-tray.service

~/.local/bin/hp-tray --connect '<строка>'   # один раз: строка сохранится в ~/.config/home-proxy/tray.conf
```

- Служба стартует вместе с графической сессией (`graphical-session.target`) и перезапускается при
  падении; `--hidden` — только значок. Ярлык автозапуска нужен для сессий без systemd (Plasma с
  `systemdBoot=false`): он сам запускает `hp-tray.service`.
- Если трей на этом ПК, клиент удалённый, и в `tray.conf` строка с `127.0.0.1:47001` (так
  настраивает `setup/tray.sh` для трея на самом клиенте), нужен ssh-туннель
  `ssh -N -L 47001:127.0.0.1:47001 <клиент>`, пока трей работает: `127.0.0.1` указывает на ту машину,
  где открыт порт. Туннеля не нужно, если трей стоит на самом клиенте или `CONTROL_ADDR` клиента — его
  адрес LAN (строка тогда с этим адресом).
- Ещё одна служба в тот же трей — «Служба…» в окне; убрать — удалить строку из `tray.conf` и
  `systemctl --user restart hp-tray`.
- Обновить: снова `install … hp-tray` и `systemctl --user restart hp-tray`. Логи —
  `journalctl --user -u hp-tray`. Убрать — `systemctl --user disable --now hp-tray` и удалить файлы.
- Windows — ярлык `hp-tray.exe --hidden` в `shell:startup`.

## 1b. Релей vps-relay (второй адрес для части дыр)

Если путь от сервера к клиентам режется (провайдер или ТСПУ душит трафик с адреса сервера),
часть дыр клиенты могут вести через релей — ещё одну машину с белым IP, хорошо связанную с сервером
(пинг до него — не больше ~30 мс). Для клиента релей — такой же сервер; пакеты он перекладывает на
настоящий, подпись сквозная, ключей клиентов не знает. К серверу релей приходит сам по своему GUID,
сервер раз в минуту сообщает клиентам подключённые релеи; пропал релей (10 с без пакетов) — клиенты
узнают сразу. Клиентам настраивать ничего не нужно (нужна версия с поддержкой релеев).

```bash
setup/vps-relay.sh <релей> <сервер>       # vps-relay → /opt/hp-relay, служба hp-vps-relay;
                                          # GUID релея — в relays.txt сервера
# второй экземпляр сервера (например, dev на другом порту):
SERVER_DIR=/opt/hp-vps-dev SERVER_UNIT=hp-vps-dev setup/vps-relay.sh <релей> <сервер>
```

- Порты релея по умолчанию те же, что у сервера (`VPS_BOOTSTRAP_PORT`, `VPS_PORTS` из его
  `vps.env`); свои — `RELAY_PORT`, `RELAY_SLOTS`. Входящий UDP на них должен проходить: при активном
  ufw скрипт открывает сам, при политике `INPUT DROP` без ufw — предупреждает (открыть вручную и
  сохранить правило).
- Новый релей — перезапуск службы сервера (`relays.txt` читается при запуске): у его клиентов
  несколько секунд без дыр. Повторный запуск с тем же релеем ничего не перезапускает.
- Доля новых дыр клиентов через релеи — `VPS_RELAY_SHARE` в `vps.env` сервера (%, по умолчанию 50).
- Релей с серым адресом на интерфейсе: порты должны быть проброшены на него, клиентам уходит адрес,
  с которого релей пришёл к серверу (или задайте `RELAY_PUBLIC_IP`).
- Проверка: `journalctl -u hp-vps-server | grep 'релей'` — «на связи» и порт знакомства; у клиента в
  логе «сервер разрешил релеи» и «идём на порт релея».

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
