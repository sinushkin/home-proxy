# android-vpn — Android-клиент (свой VPN поверх дыр)

Приложение для телефона: держит набор из 10 UDP-дыр к роутеру OpenWrt или домашнему ПК и
поднимает свой VPN — интерфейс TUN, IP-пакеты из которого уходят по дырам как есть (TCP с
номером в потоке, получатель возвращает порядок).

```
приложения телефона -> tun0 (адрес от сервера) -> libhomeproxy.so (10 дыр, keep-alive)
      -> роутер OpenWrt -> VPS -> интернет        (или -> домашний ПК -> интернет)
```

Раньше VPN был на официальной библиотеке WireGuard (`libwg-go`): WireGuard шифровал пакеты и
отдавал их в локальный UDP-мост на `127.0.0.1`, а мост — в дыры. От WireGuard отказались:
трафик и так HTTPS, а повторное шифрование и лишний круг через мост стоили процессора (на
роутере MT7628 — вдвое по скорости, `../Performance.md`). APK стал меньше, десугаринг Java не
нужен.

Три части (бэкенд отдельно от приложения):

| Что | Где | Что делает |
|---|---|---|
| `hp-client` | `../hp-backend/client` | ядро: `MultiLink` + мост TUN ↔ дыры (`hp_tun::bridge`), тестируется на хосте |
| `libhomeproxy.so` | `../hp-backend/android-lib` | JNI-обёртка над `hp-client` (`Java_ru_homeproxy_HomeProxy_*`) |
| приложение | здесь | Kotlin: экран, foreground-сервис, `HpVpnService` (TUN) |

## Порядок работы: сначала дыры, потом VPN

1. **«Подключить»** запускает клиента (`ProxyService`, foreground): дыры к роутеру
   открываются, по ним сразу идёт keep-alive (общая таска `MultiLink`).
2. **«Включить VPN»** доступна, только когда есть хотя бы одна живая дыра
   (`HomeProxy.liveHoles() >= 1`) и сервер выдал адрес; автозапуск ждёт до 90 секунд.
   `HpVpnService` поднимает интерфейс: адрес и DNS от сервера (DNS не пришли — 8.8.8.8 и
   1.1.1.1), маршрут на всё, MTU 1400, **само приложение
   исключено** (`addDisallowedApplication`, иначе дыры, STUN и MQTT ушли бы в свой же туннель).
   Дескриптор отдаётся клиенту (`HomeProxy.attachTun(pfd.detachFd())`).
3. «Выключить VPN» закрывает TUN (`detachTun`), дыры остаются.

Адрес в туннеле и DNS выдаёт тот, где трафик выходит в интернет: VPS (за роутером — запрос
`AddressRequest` роутер пересылает на VPS с номером телефона) или домашний ПК (`hp-server`).
Телефону всё равно, какая схема: в приложении только GUID телефона и GUID роутера или ПК. Адрес
закреплён за телефоном и при переподключении тот же.

В логе (`logcat -s homeproxy`):

```
слот 0: дыра открыта … (живых дыр: 1)
VPN включён: 10.80.1.1/16, DNS [8.8.8.8, 77.8.8.8, 1.1.1.1]
состояние: дыры 10/10, адрес 10.80.1.1/16, TUN подключён, отправлено 1208, получено 1008, потеряно 0
```

## Разрешения (`AndroidManifest.xml`)

| Разрешение | Зачем |
|---|---|
| `INTERNET` | UDP-дыры, STUN, MQTT по TLS |
| `FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_DATA_SYNC` | `ProxyService` работает при свёрнутом приложении |
| `POST_NOTIFICATIONS` | уведомление foreground-сервиса (Android 13+) |
| `BIND_VPN_SERVICE` | у сервиса `HpVpnService` (так система понимает, что это VPN) |

Согласие на VPN пользователь даёт системным диалогом (`VpnService.prepare`).

## Сборка

```bash
# 1. Нативная библиотека под ABI (NDK 26.1, API 24 — как build-android.sh)
./build-native.sh --release            # armeabi-v7a и x86_64
ABIS="armeabi-v7a arm64-v8a x86_64" ./build-native.sh --release   # + arm64: rustup target add aarch64-linux-android

# 2. APK
./gradlew assembleDebug                # app/build/outputs/apk/debug/app-debug.apk
```

`build-native.sh` кладёт `.so` в `app/src/main/jniLibs/<abi>/` и копирует
`../cert/out/ca.crt` в `app/src/main/assets/` (в git оба не попадают). Сертификат
брокера генерируется по `../cert/README.md`.

## Проверка из adb (без ручного ввода)

```bash
adb shell cmd appops set ru.homeproxy ACTIVATE_VPN allow      # согласие на VPN без диалога
adb shell am start -n ru.homeproxy/.MainActivity \
  --es stun IP:3499[,IP2:порт] --es mqtt IP:8883 \
  --es myId <GUID телефона> --es peerId <GUID роутера или ПК> \
  --ez autostart true --ez vpn true
adb logcat -s homeproxy:V
adb shell ping -c3 10.80.0.1      # adb shell ходит через VPN (исключено только приложение)
```

## Что проверено

Реальный телефон (TECNO KG5m, Android 11, armeabi-v7a) на LTE через точку доступа iPhone →
роутер OpenWrt (`hp-router`) → VPS (2026-09-26):

- 10/10 дыр к роутеру, у роутера 10/10 к VPS; VPN включается после появления дыр;
- через туннель `ping 10.80.0.1` и `ping 1.1.1.1` (маршрут `dev tun0 src 10.80.1.1`);
- speedtest в браузере: 14 Мбит/с на приём, 7 на отдачу, внешний IP — VPS.

Раньше, на WireGuard: эмулятор Android 12 (10/10 дыр, handshake доходил до сервера) и этот же
телефон до домашнего ПК на Windows (10/10 дыр, `ping` через туннель).

Не проверено: схема телефон → ПК с TUN, arm64, длительная работа и смена сети.
