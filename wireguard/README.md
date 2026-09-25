# wireguard — схема «телефон → дыры → этот ПК → интернет»

Минимальная схема без роутера: телефон по нашим UDP-дырам (10 штук, с keep-alive)
доходит прямо до этого ПК, где стоят две службы:

```
телефон: WireGuard (libwg-go) -> 127.0.0.1:51821 (мост)
      -> 10 дыр (libhomeproxy.so) ==== интернет ====
ПК:   homeproxy-server (hp-backend/server) -> 127.0.0.1:51820 -> WireGuard wghp (10.77.0.1)
      -> NAT (enp4s0) -> интернет
```

STUN и MQTT (рандеву) — на `profit`: STUN на порту **3499**, MQTT по TLS на 8883.

## 1. Что нужно заранее

- `wireguard-tools` на ПК: `sudo apt install wireguard-tools` (модуль ядра уже есть).
- Сертификаты брокера: `../cert/README.md` (нужен `cert/out/ca.crt`).
- Android SDK и NDK 26.1 (как в `build-android.sh`), телефон по `adb`.

## 2. Ключи и конфиги

```bash
cd wireguard
./gen.sh                # первый раз; ./gen.sh --force — заново (ключи перезапишутся)
```

В `wireguard/out/` (в git не попадает, права 600):

| Файл | Что |
|---|---|
| `server.key`, `server.pub` | ключи WireGuard на ПК |
| `client.key`, `client.pub` | ключи телефона |
| `wghp.conf` | конфиг WireGuard для ПК (интерфейс `wghp`, `10.77.0.1/24`, порт 51820) |
| `client.conf` | конфиг телефона (`10.77.0.2/32`, `AllowedIPs = 0.0.0.0/0`, Endpoint-заглушка) |
| `guids.env` | `PC_ID` и `PHONE_ID` — GUID'ы для дыр (знают оба конца заранее) |
| `homeproxy-server.service` | юнит systemd для прокси-службы, с путями этой машины |

Параметры (переменные окружения `gen.sh`): `WG_PORT` (51820), `WG_MTU` (1360),
`WG_DNS` (1.1.1.1), `WG_SUBNET_PREFIX` (10.77.0), `WG_OUT_IFACE` (внешний
интерфейс, по умолчанию как у маршрута по умолчанию, здесь `enp4s0`).

## 3. Поднять WireGuard на ПК

```bash
sudo install -m 600 out/wghp.conf /etc/wireguard/wghp.conf
sudo systemctl start wg-quick@wghp          # автозапуск при загрузке: systemctl enable
sudo wg show wghp                           # должен показать пира и порт 51820
```

`PostUp` в конфиге включает `ip_forward`, NAT (`MASQUERADE`) трафика телефона наружу
и правило, закрывающее UDP 51820 для всех, кроме `lo` (WireGuard принимает пакеты
только от локального моста службы). `PostDown` всё возвращает.

## 4. Поднять прокси-службу (дыры → WireGuard)

```bash
cd ../hp-backend
cargo build --release -p server
. ../wireguard/out/guids.env
cat > server/.env <<EOF2
STUN_ADDR=203.0.113.10:3499
MQTT_ADDR=203.0.113.10:8883
MQTT_CA=../../cert/out/ca.crt
MY_ID=$PC_ID
PEER_ID=$PHONE_ID
EOF2
sudo install -m 644 ../wireguard/out/homeproxy-server.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl start homeproxy-server
journalctl -u homeproxy-server -f            # логи
```

`PEER_ID` — GUID телефона: без роутера сервер принимает от него обычную `Data`,
как «прямого клиента», и отвечает тем же (`hp-backend/server/README.md`).

**Про STUN на ПК.** STUN должен идти тем же маршрутом, каким ПК выходит к телефону.
По правилам `o1` (`vpn-bypass.nft`) UDP с портом назначения 3479–19000 и
20000–65535 идёт мимо VPN (через WAN), остальное — через `tun1`. Поэтому STUN
на `3499` подходит, а, например, Google STUN (`19302`, вне диапазонов) показал бы
другой внешний адрес (выход `tun1`), и телефон стучался бы не туда. Серверов можно
указать несколько через запятую: `STUN_ADDR=ip:порт,ip2:порт` — клиент опрашивает
все, публикует все увиденные адреса, а пир стучится по каждому.

## 5. Собрать APK с ключами и поставить на телефон

```bash
cd ../android-vpn
./build-native.sh --release      # .so под ABI + client.conf и ca.crt в assets приложения
./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb shell cmd appops set ru.homeproxy ACTIVATE_VPN allow   # согласие на VPN без диалога
```

`client.conf` (с приватным ключом телефона) попадает в `assets/wg.conf` APK и
используется приложением по умолчанию; в git он не попадает. Телефон armeabi-v7a
(TECNO KG5m) — эту ABI `build-native.sh` собирает по умолчанию.

## 6. Запустить на телефоне

```bash
. ../wireguard/out/guids.env
adb shell am start -n ru.homeproxy/.MainActivity \
  --es stun 203.0.113.10:3499 --es mqtt 203.0.113.10:8883 \
  --es peerId $PC_ID --es myId $PHONE_ID --ei localPort 51821 \
  --ez autostart true --ez vpn true
adb logcat -s homeproxy:V
```

Порядок: сначала приложение поднимает дыры (по ним сразу идёт keep-alive), VPN
включается только когда есть хотя бы одна живая дыра. Проверка на ПК: `sudo wg show
wghp` — `latest handshake` и `transfer`; на телефоне: `ping 10.77.0.1`, `ping 8.8.8.8`.

## 7. Что важно про сеть телефона

- **Телефон и ПК не должны быть за одним внешним адресом.** NAT не возвращает
  пакеты «самому себе» (hairpin), а `o1` этого не делает. Домашний Wi-Fi телефона
  (та же сеть, что у ПК) не подходит: нужна другая сеть.
- **Симметричный NAT у оператора/точки доступа** (порт для каждого адресата свой)
  ломает обычный пробив. Точка доступа iPhone у оператора оказалась именно такой:
  для ПК телефон виден с портом, отличным от того, что видит STUN. Обычный домашний
  Wi-Fi-роутер (сохраняет порты) подходит лучше.
- После перезапуска одной из сторон на брокере может лежать запись прошлого
  запуска (до 60 с): слот застревает на ней до конца окна пробива (80 с).
  Перед проверкой подождите минуту или перезапускайте обе стороны вместе.
- MTU внутри туннеля 1360 (пакет WireGuard на 32 байта длиннее вложенного, а в дыру
  влезает до 1400 байт).

## 8. Остановка и уборка

```bash
adb shell am force-stop ru.homeproxy                  # клиент и VPN на телефоне
sudo systemctl stop homeproxy-server wg-quick@wghp    # службы на ПК
```

Удалить насовсем: `sudo rm /etc/systemd/system/homeproxy-server.service
/etc/wireguard/wghp.conf && sudo systemctl daemon-reload`, `rm -r wireguard/out`.

## Что проверено

- Обе службы на ПК поднимаются, WireGuard принимает пиров, NAT настраивается.
- Телефон через дыры **один раз дошёл до WireGuard на ПК**: `latest handshake`,
  `transfer 13.47 KiB received / 19.88 KiB sent`, счётчики моста растут. Стабильного
  канала на точке доступа iPhone добиться не удалось (симметричный NAT, см. п. 7),
  поэтому «ping через VPN» на реальном телефоне пока не проверен.
- Логика проверена тестами (81 тест в `hp-backend`, 6 Kotlin-тестов конфига).
