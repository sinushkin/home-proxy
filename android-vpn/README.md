# android-vpn — Android-клиент (VPN поверх дыр)

Приложение для телефона: держит набор из 10 UDP-дыр к роутеру и даёт WireGuard
локальный мост, так что VPN-трафик уходит в дыры, а не напрямую к серверу.

```
приложения телефона -> tun0 -> WireGuard (libwg-go) -> 127.0.0.1:<порт моста>
      -> libhomeproxy.so (10 дыр, keep-alive) -> роутер -> сервер -> WireGuard-сервер
```

Три части (бэкенд отдельно от приложения):

| Что | Где | Что делает |
|---|---|---|
| `hp-client` | `../hp-backend/client` | ядро: `MultiLink` + локальный UDP-мост (`Bridge`), тестируется на хосте |
| `libhomeproxy.so` | `../hp-backend/android-lib` | JNI-обёртка над `hp-client` (`Java_ru_homeproxy_HomeProxy_*`) |
| приложение | здесь | Kotlin: экран, foreground-сервис, VPN на официальной библиотеке WireGuard |

## Порядок работы: сначала дыры, потом VPN

1. **«Подключить»** запускает клиента (`ProxyService`, foreground): дыры к роутеру
   открываются, по ним сразу идёт keep-alive (общая таска `MultiLink`).
2. **«Включить VPN»** доступна, только когда есть хотя бы одна живая дыра
   (`HomeProxy.liveHoles() >= 1`). `VpnController.start` без дыр отказывается
   включать VPN, а автозапуск ждёт их до 90 секунд.

В логе (`logcat -s homeproxy`) это видно так:

```
состояние: дыры 0/10, …            # VPN ещё не включён
слот 0: дыра открыта … (живых дыр: 1)
VPN: запускаем, живых дыр к роутеру: 1
VPN: UP
```

Что делает `WgConfig.prepare` с конфигом WireGuard (формат wg-quick), который вы
вставляете в приложение: `Endpoint` каждого пира заменяется на
`127.0.0.1:<порт моста>`; приложение исключается из туннеля
(`ExcludedApplications`, иначе UDP-дыры и MQTT пошли бы внутрь VPN);
`MTU` по умолчанию 1360, больше 1368 не допускается (пакет WireGuard на 32 байта
длиннее вложенного, а в дыру влезает до 1400 байт).

## Разрешения (`AndroidManifest.xml`)

| Разрешение | Зачем |
|---|---|
| `INTERNET` | UDP-дыры, STUN, MQTT по TLS |
| `FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_DATA_SYNC` | `ProxyService` работает при свёрнутом приложении |
| `POST_NOTIFICATIONS` | уведомление foreground-сервиса (Android 13+) |
| `BIND_VPN_SERVICE` | у сервиса `GoBackend$VpnService` из библиотеки WireGuard (подмешивается при сборке) |

Согласие на VPN пользователь даёт системным диалогом (`VpnService.prepare`).

## Сборка

```bash
# 1. Нативная библиотека под ABI (NDK 26.1, API 24 — как build-android.sh)
./build-native.sh --release            # armeabi-v7a и x86_64
ABIS="armeabi-v7a arm64-v8a x86_64" ./build-native.sh --release   # + arm64: rustup target add aarch64-linux-android

# 2. APK
./gradlew assembleDebug                # app/build/outputs/apk/debug/app-debug.apk
./gradlew testDebugUnitTest            # Kotlin-тесты (WgConfig)
```

`build-native.sh` кладёт `.so` в `app/src/main/jniLibs/<abi>/` и копирует
`../cert/out/ca.crt` в `app/src/main/assets/` (в git оба не попадают). Сертификат
брокера генерируется по `../cert/README.md`.

Замечания по Gradle: библиотека WireGuard (`com.wireguard.android:tunnel`) использует
Java record, поэтому включены `isCoreLibraryDesugaringEnabled` и свойство
`android.enableApiModelingAndGlobalSynthetics=true` (без них D8 падает с «Record
desugaring … without a global-synthetics consumer»).

## Проверка из adb (без ручного ввода)

```bash
adb shell cmd appops set ru.homeproxy ACTIVATE_VPN allow      # согласие на VPN без диалога
adb shell am start -n ru.homeproxy/.MainActivity \
  --es stun IP:3499[,IP2:порт] --es mqtt IP:8883 --es peerId <GUID роутера или ПК> [--es myId <GUID телефона>] \
  --ei localPort 51821 --es wgConfigB64 "$(base64 -w0 wg.conf)" \
  --ez autostart true --ez vpn true
adb logcat -s homeproxy:V
```

## Что проверено

- Эмулятор Android 12 (x86_64): клиент поднимается, 10/10 дыр к роутеру, keep-alive идёт.
- VPN включается только после появления живых дыр; без дыр (неверный GUID роутера)
  не включается и `tun0` не создаётся.
- При включённом VPN дыры остаются `10/10` (приложение исключено из туннеля);
  настоящие пакеты WireGuard (инициация handshake) доходят через мост, роутер и
  сервер до UDP-эхо на `127.0.0.1:51820` сервера.
- Тесты: `hp-client` (5), `homeproxy-android` (5), Kotlin `WgConfigTest` (6).

Реальный телефон (TECNO KG5m, Android 11, armeabi-v7a) и схема без роутера
(телефон -> ПК, см. `../wireguard/README.md`):

- Приложение ставится, клиент поднимает дыры, `libwg-go` и VPN включаются.
- Через дыры до WireGuard на ПК дошёл один настоящий handshake
  (`transfer 13.47 KiB received / 19.88 KiB sent`), затем связь пропала.
- Стабильного канала на точке доступа iPhone не получилось: NAT оператора
  симметричный (порт зависит от адресата), обычный пробив его не берёт. Домашний
  Wi-Fi-роутер с сохранением портов нужно проверить отдельно.
- Пока туннель не работает, остальные приложения телефона теряют интернет:
  перед проверкой это нужно учитывать (`adb shell am force-stop ru.homeproxy`
  выключает VPN).

Не проверено: работа на симметричном NAT, arm64, длительная работа и смена сети.
