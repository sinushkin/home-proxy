# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Назначение проекта

`home-proxy` — система прямого peer-to-peer соединения через UDP hole
punching. Оба пира заранее знают полный GUID друг друга — это общий секрет пары
(из двух GUID выводятся все ключи, `auth.rs`); наружу уходит только **имя** — первая
группа GUID (8 hex-символов). По имени они находят
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
- Локальные секреты живут в `.gitignore`-путях: `hp-backend/peer/.env`, `hp-server/.env`, `hp-router/router.env`,
  `cert/out/`, `wsl/out/`, `android-vpn/local.properties`,
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
  `target/`). Исполняемые службы `hp-server/`, `hp-router/`, `vps-server/`, `vps-client/`
  лежат уровнем выше, но
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
    - `src/auth.rs` — подлинность: `peer_name()` (имя = первая группа GUID),
      `PairSecret` (SHA-256 двух полных GUID) и вывод из него XOR-векторов (по
      `session_id` получателя), ключей подписи (по паре сессий, на направление),
      ключей порта знакомства VPS и подписи `Rendezvous` (HMAC-SHA256). Подпись
      пакета — в начале: `метка (16) ‖ счётчик (8)`, ChaCha20-Poly1305 с пустым
      открытым текстом по первым `SIGNED_PREFIX` (128) байт protobuf, длина — в
      nonce; окно на 64 счётчика против повтора. На MT7628 ~19 мкс на подпись и
      столько же на проверку (по всему пакету было бы ~50).
    - `src/codec.rs` — encode/decode для `UdpMessage`: protobuf ‖ подпись (`auth`)
      + XOR первых `MASKED_PREFIX` (24 + 128) байт — подписи и подписанной части —
      циклическим 16-байтным вектором (`XorKey`) для маскировки. Вектор свой у каждой
      дыры и направления, выводится из секрета пары (в `Rendezvous` не публикуется). На
      `Rendezvous` в MQTT кодек не распространяется (внутри `Lite` по дыре он
      кодируется как всё остальное).
    - `src/p2p/stun.rs` — (P2P) минимальный клиент STUN (RFC 5389, только IPv4):
      Binding Request → парсинг `XOR-MAPPED-ADDRESS` из ответа; `query` ждёт
      ответ среди чужого трафика (на сокет слота уже стучится пир),
      `parse_servers` разбирает `STUN_ADDR=ip:порт,ip2:порт`, `describe_nat`
      пишет в лог расхождение адресов разных серверов.
    - `src/link_id.rs` — `PeerLinkId`: симметричная свёртка двух GUID сессий
      (наша + пира) в 32 байта. Одинаков на обеих сторонах дыры, поэтому обе
      присваивают ей один и тот же номер слота.
    - `src/rendezvous.rs` — общее для обоих режимов: подписанная секретом пары запись знакомства
      (`our_record`, `peer_session_from`, `decode_peer_session`) и `PeerSession`/`PeerRegistration`.
    - `src/p2p/mqtt.rs` — (P2P) рандеву через **MQTT 5** (`rumqttc::v5`), нужен, пока живых дыр нет:
      `connect()` подписывается на все дыры пира (`.../{имя пира}/+`) и отдаёт поток `PeerSession`,
      `Registrar::publish_slot()` публикует запись дыры с `message_expiry_interval` =
      `REGISTRATION_TTL` (60 с) — брокер сам удаляет протухшую запись. Остальные дыры договариваются
      по уже пробитым (виртуал-брокер, `p2p`), так что MQTT — не точка отказа. К брокеру ходим только
      по TLS (`mqtt_options()`: единственный доверенный корень — `ca.crt`, адрес проверяется по SAN);
      `client_id` = наше имя, `username` = имя пира, пароль пустой — по ним ACL брокера пускает писать
      только свои дыры и читать только дыры искомого пира. При обрыве переподключается сам.
    - `src/port_utils.rs` — чистые функции перебора портов: `sweep_bounds()`
      (диапазон `[min(a, b) - margin, max(a, b) + margin]`) и
      `zigzag_ports()` (порядок обхода от центра наружу). Примеры в шапке
      модуля — настоящие doc-тесты (`cargo test --doc`).
    - `src/punch.rs` — пробив ОДНОЙ дыры: `establish()` перебирает порты
      (`port_utils`) с фиксированного сокета, шлёт `PeerMessage::Init` (имена
      пиров) с `Punch`; `receive_loop` сначала проверяет подпись (чужой, поддельный,
      повторённый пакет отбрасывается), `Init` принимает только с ожидаемым
      `from_peer_id`, а `Lite` — с любого адреса, если слот наш: этот адрес становится текущим эндпоинтом
      пира (роуминг, у пира может быть несколько провайдеров), `LinkSender` шлёт на текущий
      эндпоинт, `PunchAck` уходит туда, откуда пришёл `Punch`. Возвращает `PeerLink` с `PeerLinkId` и
      `LinkSender` (шлёт `Lite`: keepalive/data/stats/delete/slot-offer) —
      keep-alive НЕ запускает, это делает менеджер. `PeerLink::lost()` — когда
      от пира `keepalive_timeout` (15 с) не было валидных пакетов.
    - `src/multilink.rs` — менеджер набора дыр (`MultiLink`), общий для P2P и VPS-режимов. **Набор дыр
      динамический во всех режимах**: дыра «поработала — умерла», номер (`SlotId`, u32) монотонный;
      дыры заводят менеджеры режимов (`p2p::Manager`, `vps::Manager`/`ClientActor`) через
      `HoleFactory`, у каждой дыры — задача в один проход и `SlotBase`. `LinkRegistry` =
      `Map<PeerLinkId, SlotId>` + `LiveLink` (сендер, `opened_at`, состояние `Active`/`Draining`/
      `Warming`); данные идут только по `Active` (`SlotPicker`: случайная среди ещё не
      использованных в цикле), приём — по любой живой. Одна общая keep-alive-таска (**случайный
      период 2..10 c**). `SlotBase::hold` держит дыру в реестре и сливает её по договорённости (`drain.rs`;
      `confirm_peer` — данные не идут, пока пир не подтвердил путь). Плохая дыра (по `Stats` пира
      доставлено меньше половины) сливается, замену открывает менеджер. `MultiLink::status()` —
      снимок: общее состояние и дыры (возраст, `Draining`, счётчики, потери в обе стороны по отчёту
      `Stats` пира), `LinkStatus::in_work()` — сколько дыр в работе; последняя регистрация пира на
      брокере (`PeerRegistration`). Задачи набора принадлежат `MultiLink`: его дроп останавливает
      дыры, сокеты и MQTT. `stats_loop` раз в 10 c шлёт пиру статистику по всем дырам,
      `control_loop` разбирает события дыр (`Drain`, `DeleteLink`, `Stats`, `Rendezvous` пира) и
      отдаёт команды задачам дыр (`HoleCmd`). Полезная нагрузка: `MultiLink::send_data()`/
      `send_client`/`send_ordered`, лимит `MAX_DATA_LEN` = 1400 байт; входящие приходят приложению
      каналом `Incoming { slot, payload }` (`start()` возвращает `(MultiLink, Receiver<Incoming>)`).
      `StateTracker` ведёт фазу каждой дыры и пишет в лог (`INFO`) смену общего состояния:
      `rendezvous` → `punching` → `connected(N)`.
    - `src/xor.rs` — сам XOR пакета 16-байтным вектором: одно тело, три
      ядра (базовое SSE2/NEON — 16 байт, AVX2 — 32, AVX-512 — 64), ядро
      выбирается в рантайме по `is_x86_feature_detected!`. Тесты сверяют
      каждое ядро с побайтовым эталоном на длинах 0..=300.
    - `src/label.rs` — `Label`: метка набора дыр в логах (`[phone] `,
      `[server] `; пустая ничего не печатает).
    - `src/reorder.rs` — буфер порядка TCP-пакетов на приёме (по корзине потока и номеру от
      отправителя: `Ordered`, `WrappedData` с корзиной — ключ ещё и по клиенту), адаптивное
      ожидание 3–30 мс по p99 отставания. Пакеты без номера идут сразу. (Раньше разбирал
      счётчик из заголовка WireGuard.)
    - `src/p2p/mod.rs` — P2P-режим (оба пира за NAT), динамический набор дыр **без ролей**: каждая сторона
      сама держит набор (`holes::plan` по всем дырам) и открывает дыру, когда нужно — кто начал, тот
      начал; пир, увидев запись с неизвестным номером, открывает ответную дыру, если у него сейчас
      меньше `max_total` дыр (иначе пропускает; не дождавшаяся ответа дыра закрывается через
      `RECORD_WAIT`). Первые `START_GRACE` после запуска свои дыры не открываем — ждущий пир успеет
      прислать свои. Решения менеджера — чистое ядро `Core` (события и время на входе, `Effect`-ы на
      выходе, юнит-тесты без сети), оболочка `Manager` их исполняет. Задача дыры (`P2pHole`, один
      проход): свой сокет → STUN → анонс записи (по живым дырам — виртуал-брокер; пока живых нет — в
      MQTT) → запись пира → пробив (`HOLE_DEADLINE` 90 с) → `SlotBase::hold`. `local_port_base` ≠ 0 —
      локальные порты дыр из `base..=base+99`. Версии P2P несовместимы с прежними (фиксированные
      слоты): обе стороны обновляем вместе.
    - `src/drain.rs` — слив дыры по договорённости, чистый конечный автомат: просящий шлёт `Drain` и
      держит дыру в работе до подтверждения (`Drain` пира; повтор каждые 300 мс, не дождался за 2 с —
      сливает сам), получивший просьбу сразу перестаёт слать и подтверждает; после подтверждения дыра
      `Draining` (не шлём, принимаем ещё ~2 с) и закрывается; встречные просьбы подтверждают друг друга.
      Оболочка — `SlotBase::drain` (`multilink.rs`).
    - `src/vps/mod.rs` — VPS-режим (белый IP сервера), свои стейт-машины сервера и клиента, с P2P
      не смешиваются. **Набор дыр динамический**: дыра «поработала — умерла», номер (`SlotId`, u32)
      монотонный, назначает клиент. Клиентский `Manager` раз в секунду зовёт `holes::plan`
      (`PoolPolicy`: в работе ≥ `min_active` = 4, всего ≤ `max_total` = 10, +1 дыра в 10 с, срок
      жизни дыры случайный 60–180 с, худшая по потерям уходит раньше) и открывает/сливает дыры;
      сервер реактивный: на запрос знакомства с новым номером `ClientActor` заводит `ServerHole`
      (порт из банка, `max_total × 2` дыр на клиента, недавно закрытые `(номер, сессия)` не
      воскрешает; тот же номер с новой сессией — перерегистрация прошлого клиента с фиксированными
      слотами 0..9, обратная совместимость; рост времени запуска клиента в записи
      (`registered_at_unix_ms`) = клиент перезапущен, его прошлые дыры закрываются сразу). Задача дыры
      — один проход (`ClientHole`/`ServerHole`). Слив (`Drain`): `SlotBase::hold` помечает дыру
      `Draining` (данные не шлём, принимаем ещё `DRAIN_GRACE` ≈ 2 с), пиру — `Drain` и `DeleteLink`
      (для прошлых версий) по трём дырам трижды. Дыра сервера `Warming`, пока клиент не прислал
      первый пакет после пробива (`PeerLink::is_confirmed`; иначе через `CONFIRM_TIMEOUT` закрыта):
      ответы по новой паре портов бывает не доходят; клиент бросает дыру, не открывшуюся за
      `OPEN_TIMEOUT` (12 с). `MultiLink::start_discovery` с `Discovery::{StunMqtt, VpsServer,
      VpsClient}`, `MultiLink::move_slot` (слив). В `multilink.rs` — общее: реестр (`LiveLink` с
      `opened_at`/`state`), `HoleFactory`/`SlotBase`/`HoleCmd`, keep-alive, статистика, приём.
      Настройки клиента: `HOLES_MIN`, `HOLES_MAX`, `HOLE_AGE` (`vps-client`).
    - `src/bind.rs` — `udp(ip, port, ifindex)`: сокет слота на адресе и, на Linux, на интерфейсе
      (`SO_BINDTOIFINDEX`, без прав на ядре 5.7+): там маршрут выбирается по назначению, и сокет с
      адресом `eth0` без привязки к интерфейсу всё равно ушёл бы в VPN. На Windows хватает адреса.
      `MultiLinkOptions::{bind_ip, bind_ifindex}`.
    - `src/lib.rs`, `src/discovery.rs` — устройство крейта. **Общее** (всегда): `auth`, `codec`, `wire`,
      `xor`, `pool` (шифрование, подпись, сборка и разбор пакетов), `punch` (пробив и `LinkSender`),
      `multilink` (`MultiLink`, реестр дыр, слив, статус — о режимах не знает: режим подключается
      через `MultiLink::start_mode`), `holes`, `drain`, `dedup`, `reorder`, `rendezvous` (записи
      знакомства), `port_pool`, `port_utils`, `bind`, `link_id`, `label`, `proto`. **Режимы** — отдельные
      каталоги под фичами Cargo: `p2p/` (фича `p2p`: `mod.rs`, `mqtt.rs`, `stun.rs`; тянет `rumqttc`)
      и `vps/` (фича `vps`); обе включены по умолчанию. `discovery.rs` — тонкий слой с `Discovery` и
      `MultiLink::start_discovery`/`start`/`start_with`, собирающий настройки режима. `vps-client`
      подключает только `vps` (без MQTT/TLS: на MIPS 1,4 МБ вместо 2,2), клиент телефона — только
      `p2p`. Внимание: при сборке нескольких пакетов одной командой `cargo` объединяет фичи — клиент
      роутера собираем отдельно (`PACKAGES=vps-client OpenWRT/build.sh`).
  - `client/` — крейт `hp-client`: клиент телефона (`MultiLink` + мост TUN ↔ дыры,
    `attach_tun`/`detach_tun`), ядро Android-библиотеки, проверяется на хосте. Раньше тут был
    UDP-мост для WireGuard на `127.0.0.1`.
  - `android-lib/` — крейт `homeproxy-android`: JNI-обёртка над `hp-client`
    (`libhomeproxy.so`, `Java_ru_homeproxy_HomeProxy_*`); собирается под NDK
    скриптом `android-vpn/build-native.sh`.
  - `tun/` — крейт `hp-tun` (всё ОС-зависимое — в `src/platform/`: трейты `TunDevice`, `RouteBackend`, `HookLauncher`, `NativeShutdown`; реализации по `cfg`: `linux/` (ioctl, `ip`, `sh`) и `windows/` (Wintun, PowerShell; `vps-client` на Windows — `vps-client/README.md`); политика маршрутов, `Hooks` и `hook_env` общие в `routes.rs`): свой TUN (`libc` + `AsyncFd`, без сторонних tun-крейтов): `Tun::create`
    (Linux, OpenWrt — нужен `kmod-tun`, WSL2), `Tun::from_fd` (Android `VpnService`), `packet` —
    разбор IPv4/IPv6 (протокол, порты, `flow_hash`). Кроссплатформенно: `device::PacketDevice`
    (TUN или канал в памяти `channel_pair()`), `bridge::Bridge` (клиент: запрос адреса и DNS),
    `hub::Hub` (сервер: маршрут по выданному адресу, проверка источника). Тест с ядром —
    `unshare -rn cargo test -p hp-tun`. Проверка на машине — `hp-tun-check`. `hp-backend/tun/README.md`.
  - `netstack/` — крейт `hp-netstack`: свой сетевой стек процесса (`ipstack`) для
    `hp-server MODE=netstack`: TCP/UDP телефона завершаются в процессе и открываются заново
    обычными сокетами ОС (значит, идут маршрутами ПК — в том числе через его VPN), ping отвечает
    сам. Без TUN, NAT и прав — так работает Windows. Окно TCP 64 КБ без масштабирования (около
    6 Мбит/с на соединение при 80 мс RTT).
  - `logging/` — крейт `hp-logging`: общая инициализация логов (`env_logger`
    без regex, `LOG_TARGET=syslog` — в syslog для OpenWrt (только Unix), `LOG_FILE=путь` —
    дописывать в файл; `init_with(get)` берёт настройки
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
- `hp-router/` — бинарный крейт для роутера OpenWrt, две роли в одном процессе: шлюз дома
  (TUN `hp0` ↔ 10 дыр к `vps-server`, как `vps-client`) и P2P-пир для телефонов (`PHONE_<n>_*`,
  n = `client_id`, u8): пакеты телефона не разбирает и не упорядочивает, перекладывает к VPS как
  `WrappedData { client_id, flow? }` с номерами телефона (`MultiLinkOptions::reorder_clients =
  false`), ответы VPS — обратно телефону. Сокеты дыр телефонов, STUN и MQTT идут мимо `hp0`
  (маршрут по умолчанию в `hp0` — только для LAN, правило по источнику). Настройки —
  `router.env` (`router.env.example`), `hp-router/README.md`, `OpenWRT/Tun.md`. Раньше на этом
  месте был `router` — релей телефоны ↔ ПК для WireGuard.
- `hp-server/` — крейт (библиотека + бинарник `hp-server`), домашний ПК: дыры (STUN + MQTT) → TUN
  (`TUN_ADDR`, по умолчанию `10.80.0.1/16`) → NAT на ПК. Адреса в туннеле раздаёт сервер
  (`addresses.rs`: хосты и роутеры — `10.80.0.x`, телефоны — `10.80.1.x`+, закрепляются за
  клиентом в `addresses.state`; вместе с адресом — DNS: `DNS=` или резолверы машины, в конце
  8.8.8.8, 1.1.1.1). Пиров может быть несколько (`PEER_<n>_*`), мост — `hp_tun::hub` (маршрут
  по выданному адресу, проверка адреса источника). `serve(discoveries, common)` поднимает TUN,
  `MultiLink` и мост `hp_tun::bridge`; клиенты — телефон напрямую или телефоны за роутером
  (адрес источника → `client_id`). `src/settings.rs` — файл настроек `server.env` (`KEY=VALUE`,
  `--config`, по умолчанию `server.env` рядом с бинарником; переменная окружения главнее файла;
  относительные пути — от каталога файла). `MODE=tun` (Linux, root, NAT подсети) или `netstack`
  (`hp-netstack`, по умолчанию на Windows — нативно, без WSL). `src/bypass.rs` — `BIND_ADDR`
  (`auto` по умолчанию, IP или `off`): STUN по маршруту по умолчанию и с каждого адреса машины
  (на Linux — с привязкой к интерфейсу), ответы сравниваются по каждому серверу отдельно (к
  разным серверам домашняя сеть может выходить с разных адресов); адаптер, через который ответ
  другой (или есть, когда по умолчанию тишина), — путь мимо VPN, к нему привязываются сокеты
  дыр. Соединения телефона при этом идут через VPN ПК. Библиотека
  (`settings`, `Common`, `serve`) переиспользуется `vps-server` и `hp-router`. Раньше был мост к
  WireGuard (сокет на клиента к `WG_ADDR`) и служба Windows. `hp-server/README.md`.
- `vps-server/`, `vps-client/` — схема «клиент — сервер» для VPS с белым IP (не P2P):
  без STUN, MQTT и пробива (`connection::vps`). `vps-server` — `hp_server::serve` с
  `Discovery::VpsServer` (`VPS_PUBLIC_IP`, `VPS_BOOTSTRAP_PORT`, `VPS_PORTS`, `TUN_ADDR`),
  `vps-client` — TUN + `Discovery::VpsClient` для хоста (на роутере — `hp-router`); управление
  (`hpctl`, трей) — `CONTROL_ADDR`, `vps-client --connection-string`. TCP идёт
  `Ordered` (номер в корзине потока, порядок восстанавливает `reorder`), прочее — `Data` сразу.
  Android их не использует. `OpenWRT/Tun.md`.
- `control/` — крейт `hp-control`: протокол управления службой (`proto/control.proto`, кадр
  `u32` BE + protobuf; только loopback/LAN), `secure.rs` — канал, зашифрованный ключом из строки
  подключения `homeproxy-control://<ip:порт>/<ключ>` (рукопожатие с nonce обеих сторон, ключи
  сессии SHA-256, ChaCha20-Poly1305 со счётчиком кадров; ключ по сети не ходит), `server.rs` —
  общий сервер (`trait Controlled`, ключ перечитывается на каждое соединение), клиент, ссылка
  сопряжения `homeproxy://pair?d=<base64url(PairingBundle)>`; `hpctl` — консольный клиент
  (`--connect <строка>`: `status`, `pair`, `remove`). Ключ хранит служба (`control.key`, 600),
  строку печатает `hp-server|hp-router|vps-client --connection-string` (`--new-connection-string` —
  новый ключ); службы с управлением: `hp-server` (и `vps-server`), `hp-router`, `vps-client`
  (`CONTROL_ADDR`, по умолчанию выключено; один «пир» — VPS-сервер, пары не выпускает). В статусе у
  каждой дыры номер, адрес, возраст (`age_secs`), признак слива (`draining`), потери; `live` —
  дыры в работе (`LinkStatus::in_work`), трей считает набор здоровым при ≥ 4. `control/tray/` — `hp-tray`, трей на Slint (winit +
  программный рендер): значок `ksni` (Linux) / `tray-icon` (Windows), окно со статусом, телефонами,
  дырами и потерями, QR сопряжения. Строку подключения трей получает от пользователя при первом
  запуске и хранит в `tray.conf` (каталог настроек пользователя, 600) — по файлам службы не ходит.
  Пиры на ходу — `hp-server/src/service.rs` (ожидающий сопряжения телефон ≤ 1, сопряжённые —
  `peers.state`, 600) и `hp-router` (`phones.state`, свободный `client_id`, `RwLock` на телефонах;
  `CONTROL_ADDR` — только адрес LAN). `OpenWRT/luci-app-homeproxy/` — страница LuCI (JS): показать
  и скопировать строку, «Новая строка подключения» (`fs.exec` `hp-router --config
  /etc/hp-router/router.env --connection-string|--new-connection-string`, права — ACL rpcd),
  плюс `/etc/init.d/hp-router` (procd; бинарник `/usr/bin`, настройки `/etc/hp-router`).
  Полные GUID — секрет пары: в статус и в логи идут только имена (`peer_name`). `control/README.md`.
- `setup/win/` — `.bat`-аналоги `setup/*.sh` для Windows: `vps-server.bat` (Linux-сервер по ssh, GUID уже работающего сервера не меняет), `vps-client.bat` (этот ПК как клиент: задача планировщика, Wintun, хуки `.ps1`), `vps-client-remove.bat`, `vps-prepare.bat` (через bash из Git for Windows), `DRY_RUN=1`; `.bat` только ASCII, CRLF (`.gitattributes`). `setup/win/README.md`.
- `wsl/` — образ для WSL2 (Alpine + `iptables` + статический `hp-server`): `Dockerfile`,
  `build.sh` (→ `wsl/out/homeproxy-wsl.tar.gz`, в git нет), `rootfs/` (`wsl.conf` с `[boot] command`,
  `homeproxy-start` — пересылка, `MASQUERADE` подсети туннеля, запуск службы в цикле;
  `homeproxy-stop`). Настройки — `/etc/homeproxy/{server.env,ca.crt}`. В контейнере проверено:
  `hp0` поднимается, NAT ставится. Дистрибутив надо удерживать сессией `wsl.exe`, иначе WSL
  останавливает его вместе со службой. `wsl/README.md`.
- `windows/` — README (нативный `hp-server.exe`, `MODE=netstack`, основной путь на Windows) и
  `stun-bypass.ps1` (для WSL2: если маршрут до STUN идёт не через физический адаптер,
  добавляет /32-маршрут через физический шлюз; `-WhatIf`, `-Remove`). PowerShell-файлы —
  UTF-8 **с BOM** (иначе PowerShell 5.1 ломает кириллицу). Нативная служба Windows с WireGuard и
  NAT (ICS) удалена вместе с WireGuard.
- `iPhone/` — только заметки (`README.md`): что нужно для iOS-клиента (Mac, платный
  Apple Developer Program, Network Extension) и почему это отложено. Кода нет.
- `android-vpn/` — Android-приложение (Kotlin, Gradle): экран, foreground-сервис, свой
  `HpVpnService` (TUN с адресом и DNS от сервера, маршрут на всё, приложение исключено из VPN, дескриптор
  отдаётся ядру через JNI); сначала поднимаются дыры с keep-alive, VPN включается только при
  наличии живых дыр. Раньше — библиотека WireGuard. Тулчейн — NDK 26.1 как в
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
  (`%c` = client_id = своё имя), читать только `home-proxy/rendezvous/%u/+`
  (`%u` = username = имя искомого пира; имя — первая группа GUID). Перечислить чужие устройства
  через wildcard нельзя. Подробно — `mosquitto/MosquittoACL.md`.
- `mosquitto/README.md` — доступ к брокеру, как посмотреть, что на нём
  лежит (через админский листенер), декодирование `Rendezvous` через
  `protoc --decode`, удаление retained-записи.

## Протокол (`connection::proto`)

Два семейства сообщений, разнесённые намеренно — у них разные транспорты:

- `Rendezvous { public_ip, public_port, peer_id, registered_at_unix_ms,
  session_id, slot, extra_endpoints, signature }` — адрес слота, другие внешние
  адреса того же сокета (их видели другие STUN-серверы) и подпись секретом пары;
  `peer_id` — имя. Для bootstrap-слота публикуется на
  MQTT-топике `home-proxy/rendezvous/{имя}/{slot}` (MQTT 5, с TTL); для
  остальных слотов **та же запись** едет напрямую по дыре внутри `Lite` (см.
  виртуал-брокер). `session_id` меняется при каждой новой публикации слота и
  вместе с нашим даёт `PeerLinkId`.
- `PeerMessage` — верхний конверт всего, что летит между пирами по UDP (через
  `codec`), oneof `body`:
  - `InitMessage { session_id, from_peer_id, to_peer_id, slot, oneof{Punch,
    PunchAck} }` — фаза пробива: сессия и имена пиров.
  - `Lite { slot, oneof{KeepAlive, Data, Stats, DeleteLink, Rendezvous, WrappedData, Ordered} }` —
    всё после установки дыры: идентичность подтверждена пробивом, поэтому
    только `slot` (короткий заголовок — меньше сигнатура). Принимается с любого
    адреса при верном слоте и подписи, адрес отправителя становится текущим эндпоинтом.
    - `KeepAlive { seq }` — держит NAT-маппинг живым (случайный период 2..10 c).
    - `Data { payload }` — IP-пакет без номера (UDP, ICMP) поверх дыры.
    - `Stats { links: [LinkStat{slot, sent, received}] }` — статистика пакетов
      по всем дырам (раз в 10 c).
    - `DeleteLink { slot }` — команда пиру удалить линк (пробиваем заново).
    - `AddressRequest { kind, client_id? }` / `AddressAssign { address, prefix, client_id?, dns }`
      — адрес в туннеле: клиент просит, сервер (VPS или ПК) выдаёт; роутер пересылает запрос
      телефона на VPS с `client_id` и возвращает ответ телефону.
    - `WrappedData { seq, payload, client_id }` — пакет, обёрнутый роутером:
      исходная датаграмма как есть, порядковый номер (свой счётчик на
      клиента и направление) и номер клиента `client_id` (u8, 0..=255), по
      которому роутер и сервер различают телефоны. `optional flow` (TUN-режим) —
      корзина TCP-потока: `seq` тогда номер в ней от исходного отправителя
      (телефона или VPS); роутер номера не трогает (`MultiLinkOptions::reorder_clients
      = false`), порядок возвращает конечный получатель по (клиент, корзина).
    - `Ordered { flow, seq, payload }` — TUN-режим: TCP-пакет с номером в
      корзине потока, получатель восстанавливает порядок; UDP/ICMP идут `Data`.
    - `Rendezvous {...}` — виртуал-брокер: тот же `Rendezvous`, что ушёл бы в
      MQTT, но отправленный напрямую по этой дыре, чтобы договориться о других
      слотах без брокера.

Каждый пакет по дыре подписан (`auth`); пакет без верной подписи (скан, мусор,
подделка, повтор) отбрасывается до разбора и эндпоинт не меняет. `Init` с чужим
`from_peer_id` отсеивается, `Lite` с чужим слотом — тоже.

Шифрования нет намеренно: нагрузка — и так TLS/HTTPS, XOR — только маскировка
заголовков. Подписаны первые 128 байт и длина: вставить, подделать, обрезать или
повторить пакет без полного GUID обеих сторон нельзя, но тот, кто на пути, может
испортить байты дальше 128-го в настоящем пакете (TLS это заметит). Полные GUID —
секрет пары: в репозиторий, логи и на брокер не попадают, наружу — только имена.

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
  WSL2 на ней работает (CPU ВМ — `Skylake-Client-noTSX-IBRS` + `vmx`). Нативный `hp-server.exe`
  (`MODE=netstack`) проверен здесь с эмулятором телефона и с полным туннелем OpenVPN (`BIND_ADDR`).
  Процесс, запущенный из ssh, умирает вместе с сессией — долгие прогоны через WMI
  (`Win32_Process.Create`). Скрипты для PowerShell через ssh запускаем как `-File`
  (stdin-режим ломает многострочные блоки). OpenVPN и WireGuard с неё удалены (2026-10-08) —
  выход напрямую; `vps-client.exe` проверен здесь с Wintun (`wintun.dll` в `C:\Users\user\`).
- `jump1` (SSH-алиас через `note`; Xiaomi 4C, OpenWrt 23.05) — **тестовый стенд** `hp-router`, не
  домашний шлюз. Аплинк — Wi-Fi-клиент `phy0-sta1` к `jump` (192.168.17.1, OpenWrt, точка доступа
  WRT-104, за ней провайдер). `hp-router` во флеше: `/usr/bin/hp-router`, настройки
  `/etc/hp-router/` (`router.env` с `CONTROL_ADDR=192.168.1.1:47001`, `ca.crt`, `control.key`,
  `phones.state`, проверочные хуки `on-tun-up.sh`/`on-tun-down.sh`), служба procd
  `/etc/init.d/hp-router` (автозапуск **не** включён), LuCI-страница `luci-app-homeproxy`.
  Маршруты ставит сама служба (`ROUTES=auto`, `hp-router/src/routes.rs`): /1 в `hp0`, /32 до VPS
  и STUN/MQTT через аплинк. В `/etc/nftables.d/90-hp-notrack.nft` UDP с портов VPS 40000–40999
  идёт мимо conntrack: ответы этих портов машинам из LAN не доходят (так и задумано — им VPS не
  нужен), для проверок пути к VPS из LAN брать другие порты. LAN роутера с этого ПК не видна
  (ПК со стороны его WAN): трей — через `ssh -N -L 47001:192.168.1.1:47001 jump1` и строку с
  `127.0.0.1:47001`.
- `test` (SSH-алиас, libvirt-ВМ на этом ПК, Debian 13, `sudo` без пароля) — Linux-машина для
  проверок с VPN на ПК. На `test` постоянно стоит полный туннель OpenVPN до `profit` через
  обфускацию equalizer (`server-rs` на `profit`, порты 51410–51419; голый OpenVPN из домашней
  сети ТСПУ режет после рукопожатия, `1194/udp` на `profit` закрыт). Установщики, клиент и
  сборка комплектов — в отдельном проекте equalizer2 (`installer/README.md`): на машине лежат
  `~/hptest-linux` с `install`/`uninstall`. SSH из LAN при
  включённом VPN работает.
- Тестовые прогоны — только со свежими одноразовыми GUID
  (`cat /proc/sys/kernel/random/uuid`): `exchange()` публикует retained-запись
  на топик *своего* имени (первая группа GUID), так что тест с чужим GUID затирает регистрацию
  живого пира. После прогона чистить свои записи (`mosquitto_pub -n -r`).
