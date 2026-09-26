# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Назначение проекта

`home-proxy` — система прямого peer-to-peer соединения через UDP hole
punching. Оба пира заранее знают GUID друг друга — это их взаимное "имя",
никакой отдельный shared-secret/ключ не нужен. По этому GUID они находят
друг друга через MQTT-брокер (для ПЕРВОЙ, bootstrap-дыры), а адреса остальных
дыр обменивают уже по пробитому каналу — «виртуал-брокер». Затем пробивают
прямой UDP-канал (NAT может не сохранять порт STUN'а, т.е. не быть full-cone)
и держат его периодическими keep-alive.

Стек `docker-compose.yml` (coturn STUN + Mosquitto) — только для **локальной
отладки**. В проде STUN и MQTT — публичные или свои сервера, не обязательно
на одном хосте, поэтому код в `hp-backend` не должен предполагать, что они
живут рядом.

Сейчас поднят debug-стек на удалённом хосте `profit` (SSH-алиас,
root@203.0.113.10): `docker-compose` (coturn + mosquitto) в
`/opt/home-proxy`, открыты порты `3499/udp+tcp` (STUN, нестандартный порт вместо 3478) и `8883/tcp` (MQTT
over TLS с ACL) через `ufw`. Правила `1883/tcp` и `9001/tcp` в `ufw` остались
от старой схемы, но слушать на них теперь нечему. Брокер там работает с
`mosquitto/config` и тремя файлами из `cert/out/` (`ca.crt`, `server.crt`,
`server.key`); файлы `server.key`, `acl.conf`, `mosquitto.conf` на хосте должны
принадлежать `1883:1883` с правами `600` (пользователь `mosquitto` в
контейнере), иначе брокер не прочитает ключ. Управление — `docker-compose`
(v1, не `docker compose`). Там же `apt`-пакетом стоит `protoc`
и вручную положена копия `/opt/home-proxy/proto/connection.proto` — для
декодирования MQTT-сообщений прямо на хосте (см. `mosquitto/README.md`);
копия ручная, при изменении протокола её надо обновлять `rsync`'ом отдельно.
Это реальный внешний сервер с другими сервисами (VPN на `tun0` и т.д.) —
трогать firewall/сервисы за пределами `home-proxy` без явного запроса
нельзя.

## Правила публичного репозитория

Этот каталог — основной рабочий и одновременно публичный клон
github.com/sinushkin/home-proxy. Всё, что попадает в git, видят все:

- В коммитируемых файлах (код, доки, конфиги, тесты) **никаких реальных IP,
  ключей, GUID'ов, сертификатов**. Вместо адресов — документационные
  плейсхолдеры: `203.0.113.10` (`profit`), `203.0.113.20` (`ruhor`),
  `198.51.100.7` (домашний WAN). Реальные адреса лежат только в SSH-алиасах
  (`~/.ssh/config`) и в игнорируемых файлах.
- Локальные секреты живут в `.gitignore`-путях: `hp-backend/peer/.env`, `hp-server/.env`, `router/.env`,
  `cert/out/`, `wireguard/out/`, `android-vpn/local.properties`,
  `android-vpn/app/src/main/{assets,jniLibs}/`. Перед `git add` смотреть
  `git status`, не использовать `git add -A` вслепую.
- Коммиты — с публичной личностью, глобальный git config не менять:
  `git -c user.name=sinushkin -c user.email=sinushkin@users.noreply.github.com commit ...`.
- Коммитить и пушить только по явной просьбе.
- Старый репозиторий `~/home-proxy` (полная история, без remote) — архив,
  не удалять и не править.

## Ограничения платформы (обязательно)

- **Никаких 64-битных атомарных операций** — ни в нашем коде, ни в зависимостях: `AtomicU64`,
  `AtomicI64`, 64-битные `__atomic_*` в C-частях. Целевая платформа — роутеры на MIPS32 (MT7628,
  OpenWrt): там их нет в железе, компилятор уходит в `libatomic` (на роутер её не ставим, статически
  не линкуется: `libatomic.a` в тулчейне OpenWrt без `-fPIC`). Счётчики — `AtomicU32` (или под
  мьютексом), `AtomicUsize` тоже 32-битный на MIPS32, но без нужды не используем.
- Зависимости, которые тянут 64-битные атомики (например, `mimalloc`), не подключаем. Проверка —
  сборка `OpenWRT/build.sh`: `undefined reference to __atomic_*_8` при линковке.
- MTU известен (пакет в дыре ≤ 1500 байт): буферы под пакеты — фиксированного размера из банка
  буферов, а не выделение памяти на каждый пакет.

## Структура репозитория

- `hp-backend/` — библиотечные крейты и `peer` (CLI для живых проверок), входят в Cargo
  workspace, корень которого — `Cargo.toml` в корне репозитория (там же `Cargo.lock`,
  `target/`). Исполняемые службы `hp-server/` и `router/` лежат уровнем выше, но
  собираются тем же workspace. Все
  версии зависимостей закреплены один раз в `[workspace.dependencies]`,
  крейты-участники подключают их через `{ workspace = true }` — никогда не
  указывать версию прямо в `Cargo.toml` крейта. Ошибки — через `anyhow`
  (`anyhow::Result`, `.context(...)` там, где голая ошибка была бы
  безымянной), а не `Box<dyn Error>`.
  - `connection/` — библиотечный крейт: протокол, кодек, STUN-клиент,
    MQTT-рандеву, пробив одной дыры и менеджер набора дыр. Архитектура
    целиком (слотовая модель, транспорты, поток, TTL) — в
    `connection/README.md`.
    - `proto/connection.proto` — протокол на проводе, компилируется
      `prost-build` через `build.rs` в `OUT_DIR/hp_backend.connection.rs`,
      реэкспортируется как `connection::proto`.
    - `src/codec.rs` — encode/decode для `UdpMessage`: protobuf-сериализация
      + XOR первых 64 байт (`MASKED_PREFIX`) циклическим 16-байтным вектором (`XorKey`; дальше в
      `Data` идёт шифротекст WireGuard, его не маскируем; временный
      шифр-заглушка). Вектор свой у каждой дыры и у каждого направления:
      владелец генерирует его на каждую регистрацию слота, публикует в
      `Rendezvous.key`, а пир кодирует им всё, что шлёт владельцу. На
      `Rendezvous` в MQTT кодек не распространяется (внутри `Lite` по дыре он
      кодируется как всё остальное).
    - `src/stun.rs` — минимальный клиент STUN (RFC 5389, только IPv4):
      Binding Request → парсинг `XOR-MAPPED-ADDRESS` из ответа; `query` ждёт
      ответ среди чужого трафика (на сокет слота уже стучится пир),
      `parse_servers` разбирает `STUN_ADDR=ip:порт,ip2:порт`, `describe_nat`
      пишет в лог расхождение адресов разных серверов.
    - `src/link_id.rs` — `PeerLinkId`: симметричная свёртка двух GUID сессий
      (наша + пира) в 32 байта. Одинаков на обеих сторонах дыры, поэтому обе
      присваивают ей один и тот же номер слота.
    - `src/rendezvous.rs` — рандеву через **MQTT 5** (`rumqttc::v5`), только
      для **bootstrap-слота** (0): `connect()` подписывается на слоты пира и
      отдаёт поток `PeerSession`, `Registrar::publish_slot()` публикует наш
      слот с `message_expiry_interval` = `REGISTRATION_TTL` (60 с) — брокер сам
      удаляет протухшую запись. Об остальных слотах договариваемся через
      виртуал-брокер (см. `multilink`), так что MQTT — не точка отказа для 9
      из 10 дыр. Оба GUID известны заранее, отдельного секрета нет. К брокеру
      ходим только по TLS (`mqtt_options()`: единственный доверенный корень —
      `ca.crt`, адрес проверяется по SAN); `client_id` = наш GUID,
      `username` = GUID пира, пароль пустой — по ним ACL брокера пускает
      писать только свои слоты и читать только слоты искомого пира. При обрыве
      MQTT-соединения переподключается сам (подписка ставится заново).
    - `src/port_utils.rs` — чистые функции перебора портов: `sweep_bounds()`
      (диапазон `[min(a, b) - margin, max(a, b) + margin]`) и
      `zigzag_ports()` (порядок обхода от центра наружу). Примеры в шапке
      модуля — настоящие doc-тесты (`cargo test --doc`).
    - `src/punch.rs` — пробив ОДНОЙ дыры: `establish()` перебирает порты
      (`port_utils`) с фиксированного сокета, шлёт `PeerMessage::Init` (полный
      GUID) с `Punch`; `receive_loop` принимает `Init` только с ожидаемым
      `from_peer_id` (отсеивает hairpin-эхо), а `Lite` — с любого адреса, если слот
      наш и пакет прошёл XOR-вектор и protobuf: этот адрес становится текущим эндпоинтом
      пира (роуминг, у пира может быть несколько провайдеров), `LinkSender` шлёт на текущий
      эндпоинт, `PunchAck` уходит туда, откуда пришёл `Punch`. Возвращает `PeerLink` с `PeerLinkId` и
      `LinkSender` (шлёт `Lite`: keepalive/data/stats/delete/slot-offer) —
      keep-alive НЕ запускает, это делает менеджер. `PeerLink::lost()` — когда
      от пира `keepalive_timeout` (15 с) не было валидных пакетов.
    - `src/multilink.rs` — менеджер набора дыр (`MultiLink`). Держит
      `TARGET_LINKS` (10) слотов, у каждого свой сокет; на слот — задача:
      STUN → анонс → ожидание сессии пира → пробив с окном `PUNCH_WINDOW`
      (80 с). Анонс слота 0 — публикацией в MQTT; слотов 1..9 — через
      **виртуал-брокер**: тот же `Rendezvous` шлётся напрямую по живым дырам
      (`spawn_hole_announce`), а `control_loop` скармливает пришедшие
      `Rendezvous` пира соответствующим слотам. Слот k пробивается только к слоту k
      пира → сходятся без N×N. Одна общая keep-alive-таска (**случайный
      период 2..10 c** — маскировка ритма). `LinkRegistry` = `Map<PeerLinkId,
      u8>` (номер слота = «через какую дыру», поедет в `PeerMessage`-slot) +
      сендеры по слотам; хватает одной живой дыры, чтобы гонять трафик, пока
      остальные добираются. Потеря/плохая дыра → перерегистрация со свежей
      сессией. `stats_loop` раз в 10 c шлёт пиру статистику по всем дырам,
      `control_loop` разбирает статистику пира — если по дыре
      получено меньше половины отправленного (при выборке ≥ `MIN_STATS_SAMPLE`),
      дыра плохая: `request_redrop` пробивает её заново, пиру уходит
      `DeleteLink`. Пакеты считают `LinkSender`/`receive_loop` (`LinkStats`).
      Полезная нагрузка: `MultiLink::send_data()` шлёт `Data` по одной из
      живых дыр: случайной среди ещё не использованных в цикле, пока не
      выберутся все, потом снова (`SlotPicker`); лимит `MAX_DATA_LEN` = 1200
      байт. Входящие `Data` приходят приложению каналом `Incoming { slot, payload }`
      (`start()` возвращает `(MultiLink, Receiver<Incoming>)`).
      `StateTracker` ведёт фазу каждого слота и пишет в лог (`INFO`) смену
      общего состояния: `rendezvous` → `punching` → `connected(N)`.
    - `src/xor.rs` — сам XOR пакета 16-байтным вектором: одно тело, три
      ядра (базовое SSE2/NEON — 16 байт, AVX2 — 32, AVX-512 — 64), ядро
      выбирается в рантайме по `is_x86_feature_detected!`. Тесты сверяют
      каждое ядро с побайтовым эталоном на длинах 0..=300.
    - `src/relay.rs` — релей роутера: `forward()` перекладывает `Incoming` из
      одного `MultiLink` в другой через `Outbound` (`PlainOut` — обычная
      `Data`, `WrappedOut` — `WrappedData` с `client_id`), `route_by_client()`
      раскладывает пакеты сервера по телефонам; счётчики `DirectionStats`
      (32-битные: на MIPS нет 64-битных атомиков).
    - `src/label.rs` — `Label`: метка набора дыр в логах (`[phone] `,
      `[server] `; пустая ничего не печатает).
    - `src/reorder.rs` — буфер порядка пакетов WireGuard на приёме (по `receiver index` и
      счётчику из открытого заголовка), адаптивное ожидание 3–30 мс по p99 отставания.
    - `src/vps.rs` — VPS-режим (белый IP сервера): порт знакомства вместо MQTT, случайные
      порты слотов из диапазона, пассивный пробив на сервере; `MultiLink::start_discovery`
      с `Discovery::{StunMqtt, VpsServer, VpsClient}`, `MultiLink::move_slot`.
    - `src/lib.rs` — подключает модули (`codec`, `label`, `link_id`,
      `multilink`, `port_utils`, `punch`, `relay`, `reorder`, `rendezvous`, `stun`, `vps`,
      `xor`, `proto`).
  - `client/` — крейт `hp-client`: клиент телефона (`MultiLink` + локальный UDP-
    мост `Bridge` «WireGuard <-> дыры»), ядро Android-библиотеки, проверяется на хосте.
  - `android-lib/` — крейт `homeproxy-android`: JNI-обёртка над `hp-client`
    (`libhomeproxy.so`, `Java_ru_homeproxy_HomeProxy_*`); собирается под NDK
    скриптом `android-vpn/build-native.sh`.
  - `logging/` — крейт `hp-logging`: общая инициализация логов (`env_logger`
    без regex, `LOG_TARGET=syslog` — в syslog для OpenWrt (только Unix), `LOG_FILE=путь` —
    дописывать в файл: у службы Windows нет консоли; `init_with(get)` берёт настройки
    не из окружения, а из переданной функции).
  - `peer/` — бинарный крейт, CLI-обвязка: поднимает `MultiLink`, шлёт по
    Enter набранную строку пиру (`отправлено по дыре [#N]: ...`) и печатает
    входящие как `[#N] ...` (N — номер дыры); keep-alive, добавление и
    удаление дыр и т.п. — в `DEBUG` (`RUST_LOG=peer=debug,connection=debug`).
    С `LOG_TARGET=syslog` пишет в syslog (OpenWrt: `logread`).
    Кросс-сборка под OpenWrt (mipsel, Xiaomi 4C, `-Z build-std`, ~1.7 МБ) —
    в `OpenWRT/` (`README.md`, `build.sh`). Логи — через `log` + `env_logger` (цветные, `RUST_LOG`,
    по умолчанию `info`; `connection` зависит только от фасада `log`, вывод
    инициализирует бинарник `peer`). Аргументы: `<stun_addr[,stun2_addr]> <mqtt_addr>
    <mqtt_ca> <my_peer_id> <peer_id>` (`mqtt_ca` — путь к PEM с `ca.crt`
    брокера) — оба GUID'а операторы знают заранее (например,
    `uuidgen`), сам бинарник их не генерирует. Примеры команд — в
    `hp-backend/peer/README.md`.
- `router/` — бинарный крейт, релей телефоны ↔ сервер: набор из 10 дыр к
  серверу и по набору к каждому телефону (`PHONE_<n>_*`, n = `client_id`,
  u8). От телефона принимает `Data`, оборачивает в
  `WrappedData { client_id }` и шлёт серверу; от сервера по `client_id`
  находит телефон, разворачивает и шлёт ему `Data`. Настройки —
  переменные окружения (`run.sh` + `.env`), описание и проверка —
  `router/README.md`.
- `hp-server/` — крейт (библиотека + бинарник `hp-server`), мост клиенты → WireGuard (как
  `server-rs`): для каждого клиента свой локальный UDP-сокет
  `127.0.0.1:0` к `WG_ADDR`, неактивные клиенты удаляются. Клиент — либо
  `client_id` за роутером (`WrappedData`, ответ обёрнутый с тем же
  `client_id`), либо телефон напрямую (обычная `Data`, «прямой» клиент,
  ответ тоже обычной `Data`). Собирается и под Windows: `src/settings.rs` — файл
  настроек `server.env` (`KEY=VALUE`, `--config`, по умолчанию `server.env` рядом с
  бинарником; переменная окружения главнее файла; относительные пути — от каталога
  файла), `src/winsvc.rs` (только `cfg(windows)`) — служба `homeproxy-server`:
  `hp-server install --config …` / `uninstall` / `--service` (запуск диспетчером служб,
  остановка по SCM). Читатель ответов моста переживает рестарт WireGuard
  (`ConnectionReset`/`ConnectionRefused` — не конец клиента). `hp-server/README.md`.
  Библиотека (`bridge`, `settings`, `Common`, `serve`) переиспользуется `vps-server`.
- `vps-server/`, `vps-client/` — схема «клиент — сервер» для VPS с белым IP (не P2P):
  без STUN, MQTT и пробива (`connection::vps`). `vps-server` — мост `hp-server` с
  `Discovery::VpsServer` (`VPS_PUBLIC_IP`, `VPS_BOOTSTRAP_PORT`, `VPS_PORTS`), `vps-client` — мост
  `hp_client::bridge` с `Discovery::VpsClient`. Android их не использует; `vps-client` собирается
  под OpenWrt.
- `control/` — только описание (`README.md`): протокол управления (тот же protobuf, без XOR, TCP в
  «демилитаризованной зоне»: loopback или доверенная LAN), трей на Slint, показ QR с пакетом
  сопряжения и его сканирование в Android. Кода пока нет; `hp-server`/`router` от него не зависят.
- `wsl/` — образ для WSL2 (Alpine + `wireguard-tools` + `iptables` + статический `server`): `Dockerfile`,
  `build.sh` (→ `wsl/out/homeproxy-wsl.tar.gz`, в git нет), `rootfs/` (`wsl.conf` с `[boot] command`,
  `homeproxy-start`/`homeproxy-stop`). Настройки — `/etc/homeproxy/{server.env,wghp.conf,ca.crt}`.
  Проверен в контейнере и в настоящем WSL2 на тестовой ВМ (пробив 10/10 с VPS); дистрибутив надо
  удерживать сессией `wsl.exe`, иначе WSL останавливает его вместе со `server`. `wsl/README.md`.
- `windows/` — установка службы на Windows: `install.ps1` (WireGuard-туннель `wghp`
  из `wghp.conf` без PostUp/PostDown, NAT через `New-NetNat`, правило брандмауэра,
  служба после туннеля, обход VPN для STUN), `uninstall.ps1`, `stun-bypass.ps1`
  (если маршрут до STUN идёт не через физический адаптер, добавляет /32-маршрут
  через физический шлюз; `-WhatIf`, `-Remove`). PowerShell-файлы — UTF-8 **с BOM**
  (иначе PowerShell 5.1 ломает кириллицу); NAT — `New-NetNat` или ICS (`-Nat`). `windows/README.md`,
  `hp-server/WINDOWS.md`.
- `wireguard/` — схема «телефон -> дыры -> этот ПК -> интернет» без роутера:
  `gen.sh` (ключи и конфиги WireGuard, GUID'ы; результат в `wireguard/out/`, в git
  не попадает) и README по пунктам (WireGuard `wghp` на ПК, прокси-служба
  `homeproxy-server`, APK с ключами, запуск на телефоне, сетевые оговорки).
- `iPhone/` — только заметки (`README.md`): что нужно для iOS-клиента (Mac, платный
  Apple Developer Program, Network Extension) и почему это отложено. Кода нет.
- `android-vpn/` — Android-приложение (Kotlin, Gradle): экран, foreground-сервис,
  VPN на библиотеке WireGuard; сначала поднимаются дыры с keep-alive, VPN
  включается только при наличии живых дыр. Тулчейн — NDK 26.1 как в
  `build-android.sh`. Сборка, проверка из adb — `android-vpn/README.md`.
- `docker-compose.yml` — локальный dev-стек: `stun` (coturn, только STUN) и
  `mqtt` (Eclipse Mosquitto, только TLS-порт `8883`). Порты переопределяются
  через `.env` (см. `.env.example`); `.env` в `.gitignore`. Сертификаты
  монтируются файлами из `cert/out/` (без `ca.key`) и должны быть
  сгенерированы заранее — иначе брокер не стартует.
- `cert/` — `README.md`: пошаговая инструкция по своему CA и сертификату
  брокера. Сгенерированное лежит в `cert/out/` (в `.gitignore`); `ca.key`
  наружу и на брокер не копируется.
- `coturn/turnserver.conf` — конфиг coturn, намеренно только STUN
  (`stun-only`, `no-tcp-relay`), без TURN-релея и без аутентификации.
- `mosquitto/config/mosquitto.conf` — внешний листенер только `8883` (MQTT
  over TLS, `allow_anonymous true`, пароль не проверяется) с `acl.conf`;
  плюс админский листенер `1883` на `127.0.0.1` внутри контейнера без ACL
  (достучаться можно только через `docker exec`). `per_listener_settings true`.
- `mosquitto/config/acl.conf` — ACL: писать только `home-proxy/rendezvous/%c/+`
  (`%c` = client_id = свой GUID), читать только `home-proxy/rendezvous/%u/+`
  (`%u` = username = GUID искомого пира). Перечислить чужие устройства
  через wildcard нельзя. Подробно — `mosquitto/MosquittoACL.md`.
- `mosquitto/README.md` — доступ к брокеру, как посмотреть, что на нём
  лежит (через админский листенер), декодирование `Rendezvous` через
  `protoc --decode`, удаление retained-записи.

## Протокол (`connection::proto`)

Два семейства сообщений, разнесённые намеренно — у них разные транспорты:

- `Rendezvous { public_ip, public_port, peer_id, registered_at_unix_ms,
  session_id, slot, key, extra_endpoints }` — адрес слота, XOR-вектор (16 байт) и
  другие внешние адреса того же сокета (их видели другие STUN-серверы). Для bootstrap-слота публикуется на
  MQTT-топике `home-proxy/rendezvous/{peer_id}/{slot}` (MQTT 5, с TTL); для
  остальных слотов **та же запись** едет напрямую по дыре внутри `Lite` (см.
  виртуал-брокер). `session_id` меняется при каждой новой публикации слота и
  вместе с нашим даёт `PeerLinkId`.
- `PeerMessage` — верхний конверт всего, что летит между пирами по UDP (через
  `codec`), oneof `body`:
  - `InitMessage { session_id, from_peer_id, to_peer_id, slot, oneof{Punch,
    PunchAck} }` — фаза пробива: полный GUID-заголовок, чтобы отличить пакет
    пира от вернувшегося к себе (hairpin) и от постороннего.
  - `Lite { slot, oneof{KeepAlive, Data, Stats, DeleteLink, Rendezvous, WrappedData} }` —
    всё после установки дыры: идентичность подтверждена пробивом, поэтому
    только `slot` (короткий заголовок — меньше сигнатура). Принимается с любого
    адреса при верном слоте и векторе, адрес отправителя становится текущим эндпоинтом.
    - `KeepAlive { seq }` — держит NAT-маппинг живым (случайный период 2..10 c).
    - `Data { payload }` — нагрузка поверх дыры (будущий WireGuard).
    - `Stats { links: [LinkStat{slot, sent, received}] }` — статистика пакетов
      по всем дырам (раз в 10 c).
    - `DeleteLink { slot }` — команда пиру удалить линк (пробиваем заново).
    - `WrappedData { seq, payload, client_id }` — пакет, обёрнутый роутером:
      исходная датаграмма как есть, порядковый номер (свой счётчик на
      клиента и направление) и номер клиента `client_id` (u8, 0..=255), по
      которому роутер и сервер различают телефоны.
    - `Rendezvous {...}` — виртуал-брокер: тот же `Rendezvous`, что ушёл бы в
      MQTT, но отправленный напрямую по этой дыре, чтобы договориться о других
      слотах без брокера.

Битый/чужой protobuf (случайный пакет, скан) просто игнорируется:
`codec::decode` даёт `Err`. `Init`-пакет с чужим `from_peer_id` отсеивается,
`Lite`-пакет с чужим слотом или вектором игнорируется, с верным — принимается с любого адреса.

Аутентификации сообщений всё ещё нет (XOR 16-байтным вектором — не защита, вектор
открыт на брокере вместе с GUID, `Lite` вообще без подписи — доверяем по адресу дыры). Это осознанный
временный компромисс, см. обзор недостатков.

## Команды

```bash
# Сборка / тесты Rust-workspace (из корня репозитория)
cargo build
cargo test
cargo test -p connection punch::tests::zigzag_fans_outward_from_center   # один тест

# Ручной запуск пира (например, локально против дебаг-стека)
cargo run -p peer -- 127.0.0.1:3499 127.0.0.1:8883 cert/out/ca.crt <мой-guid> <guid-пира>

# Локальный debug-стек (STUN + MQTT); сначала сертификаты — см. cert/README.md
docker compose up -d
docker compose config   # проверить compose-файл
docker compose down
```

Сборка переносимая: ядро XOR (SSE2/NEON, AVX2, AVX-512) выбирается в
рантайме (`xor.rs`), поэтому один бинарник работает на любом CPU. Флаг
`-C target-cpu=native` по умолчанию НЕ включаем: с ним
`is_x86_feature_detected!` сворачивается в константу времени компиляции, а
бинарник, собранный на `po`, может упасть с `SIGILL` на `ruhor`. Под
конкретную машину при желании: `RUSTFLAGS="-C target-cpu=native" cargo build
--release`.

Нужен бинарник `protoc` в `PATH` — `prost-build` вызывает его при сборке
(vendored/bundled protoc нет).

## Известные ограничения тестового окружения

Хосты `rud` и `po` (SSH-алиасы) сидят за одним и тем же NAT-шлюзом (общий
внешний IP) — прямой пробив между ними не проходит из-за отсутствия hairpin
NAT на этом шлюзе (пакет на собственный публичный IP роутера не
заворачивается обратно во внутреннюю сеть). Это ограничение сети, а не баг в
`punch.rs` — для реальной проверки hole punching нужны пиры за разными
NAT/провайдерами.

## Тестовые хосты и деплой

- `profit` — STUN + MQTT (см. выше). `peer` там не запускаем.
- `ruhor` (SSH-алиас, root@203.0.113.20) — настоящий VPS с публичным IP
  прямо на интерфейсе, **без NAT**, ufw нет, входящий UDP не фильтруется. Это
  единственный хост, с которым пробив реально проходит (из локальной сети —
  как обычный исходящий UDP, поэтому широкую развёртку он не проверяет).
  Toolchain'а на нём нет и ставить его не надо: бинарник собираем локально и
  копируем. Локальная сборка (glibc 2.41) на `ruhor` (glibc 2.39) запускается:
  `peer` требует лишь символы glibc ≤ 2.34 (проверка:
  `objdump -T target/release/peer | grep -o 'GLIBC_[0-9.]*' | sort -V -u`);
  если после обновления зависимостей появится требование выше 2.39, соберите
  на `po` (glibc 2.36). Копировать через временный файл + `mv -f`, иначе
  "Text file busy" при работающем пире. Для запуска на хосте есть `run.sh` +
  `.env` (см. `hp-backend/peer/README.md`).
- `rud` и `po` сидят за одним NAT — между собой пробить не могут (hairpin).
- `win` (SSH-алиас, libvirt-ВМ на этом ПК, Windows 10, свой `~/.ssh/config`) —
  тестовая Windows-машина: Rust MSVC и git есть, `protoc` лежит в
  `C:\Users\user\tools\protoc`, исходники синхронизируем `tar` через ssh в
  `C:\Users\user\home-proxy` и собираем там (`set PROTOC=…`, `cargo build --release -p hp-server`).
  WireGuard для Windows установлен, `New-NetNat` не работает (нет WMI-провайдера NetNat),
  поэтому NAT на ВМ — через ICS (`install.ps1 -Nat Auto` выбирает его сам). Ловушки и выводы —
  `hp-server/WINDOWS.md`. Сквозная проверка с телефоном на этой ВМ
  проходила (дыры 10/10, рукопожатие WireGuard, ping туннеля) с отдельной парой из
  `wireguard/out/win/` (`WG_OUT_DIR=out/win ./gen.sh`). Скрипты для
  PowerShell через ssh запускаем как `-File` (stdin-режим ломает многострочные блоки).
- Тестовые прогоны — только со свежими одноразовыми GUID
  (`cat /proc/sys/kernel/random/uuid`): `exchange()` публикует retained-запись
  на топик *своего* GUID, так что тест с чужим GUID затирает регистрацию
  живого пира. После прогона чистить свои записи (`mosquitto_pub -n -r`).
