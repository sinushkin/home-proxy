# TLS для MQTT-брокера (Mosquitto) со своим CA

Как сделать так, чтобы `peer` ходил к брокеру по `mqtts` (TLS), а не по
открытому `1883`. Схема: свой мини-CA подписывает серверный сертификат
брокера, клиенты (`peer`) доверяют только файлу `ca.crt`. Домен не нужен —
адрес брокера идёт в сертификат как IP (SAN). Let's Encrypt не используем.

Доступ к брокеру (кто что может читать и писать) — отдельно, в
`../mosquitto/README.md`. Здесь только сертификаты и выкатка.

Что где лежит:

| Файл | Секретный? | Куда едет |
|---|---|---|
| `ca.key` | да, самый важный | никуда, хранить только у себя |
| `ca.crt` | нет | на брокер и на все пиры |
| `server.key` | да | только на брокер |
| `server.crt` | нет | только на брокер |

Всё генерируется в `cert/out/` — он уже в `.gitignore`, в git не попадает.

`vps-server` в командах ниже — SSH-алиас хоста с брокером (`~/.ssh/config`).
Если алиас у вас называется иначе (в этом репозитории исторически `profit`),
подставьте свой.

## 1. Убедиться, что ключи не уедут в git

`cert/out/` уже в `.gitignore`. Проверка (должна вывести путь):

```ssh
git check-ignore cert/out/ca.key
```

## 2. Сгенерировать CA

Корневой сертификат на 10 лет. Ключ — эллиптический (P-256), `-nodes` значит
«без пароля на ключе». Из-за долгого срока CA переживёт много перевыпусков
серверного сертификата, и клиентов при этом трогать не придётся.

```ssh
mkdir -p cert/out && cd cert/out
openssl req -new -x509 -days 3650 -nodes \
  -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout ca.key -out ca.crt -subj "/CN=Internal CA"
chmod 600 ca.key
```

## 3. Сгенерировать ключ и сертификат брокера

Три шага: ключ + запрос на подпись (CSR), файл с расширениями, подпись CA.

Расширение `subjectAltName` обязательно: rustls (и любой современный клиент)
проверяет адрес брокера только по нему, `CN` игнорируется. Здесь адрес — IP
брокера; если он переедет, сертификат надо выпустить заново с новым IP.
`IP:127.0.0.1` нужен для локального стека и проверок изнутри контейнера.

```ssh
openssl req -new -nodes \
  -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout server.key -out server.csr -subj "/CN=broker"

printf '%s\n' \
  'subjectAltName=IP:203.0.113.10,IP:127.0.0.1' \
  'basicConstraints=CA:FALSE' \
  'extendedKeyUsage=serverAuth' > server.ext

openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -days 825 -extfile server.ext -out server.crt
chmod 600 server.key
```

Срок 825 дней — не «вечно»: по истечении надо повторить этот шаг (см. п. 9).

## 4. Проверить, что получилось

Убедиться, что цепочка сходится и SAN на месте:

```ssh
openssl verify -CAfile ca.crt server.crt
openssl x509 -in server.crt -noout -subject -issuer -dates -ext subjectAltName
```

Ожидается `server.crt: OK` и в SAN нужный IP. Возврат в корень репозитория:

```ssh
cd ../..
```

## 5. Конфиг Mosquitto (уже в репозитории)

Править ничего не нужно, всё лежит в репозитории:

- `mosquitto/config/mosquitto.conf` — внешний листенер только `8883` с TLS
  (`/mosquitto/certs/{ca.crt,server.crt,server.key}`) и `acl_file`; открытый
  `1883` слушает только `127.0.0.1` внутри контейнера (для оператора через
  `docker exec`), порт наружу не пробрасывается;
- `mosquitto/config/acl.conf` — правила доступа;
- `docker-compose.yml` — пробрасывает `8883` и монтирует **три файла** из
  `cert/out/` (`ca.crt`, `server.crt`, `server.key`), а не каталог целиком:
  `ca.key` в контейнер попадать не должен.

Локальный стек (сертификаты уже сгенерированы в п. 2–4) и проверка старта:

```ssh
docker compose up -d mqtt
docker compose logs mqtt | tail -20
```

В логе должно быть `Opening ipv4 listen socket on port 8883` и
`mosquitto version ... running`, без ошибок про `keyfile`.

**Права на файлы.** Mosquitto в контейнере работает от пользователя
`mosquitto` (uid `1883`). Если ключ принадлежит другому пользователю и закрыт
(`600`), брокер пишет `Unable to load server key file ... Permission denied`.
Рабочий вариант: отдать `server.key` (и `acl.conf` — иначе брокер предупреждает
«в будущих версиях откажусь его грузить») uid `1883` с правами `600`. Для
локального стека, где файлы принадлежат вашему пользователю, достаточно
`chmod 644 cert/out/server.key`, пока каталог недоступен посторонним.

## 6. Выкатить на vps-server

Брокер там живёт в `/opt/home-proxy`. Копируем только то, что нужно ему:
`ca.crt`, `server.crt`, `server.key` — без `ca.key`. Затем конфиги и compose.

```ssh
ssh vps-server 'mkdir -p /opt/home-proxy/cert/out'
rsync -rt cert/out/ca.crt cert/out/server.crt cert/out/server.key \
  vps-server:/opt/home-proxy/cert/out/
rsync -rt mosquitto/config/ vps-server:/opt/home-proxy/mosquitto/config/
rsync -t docker-compose.yml .env.example vps-server:/opt/home-proxy/
```

Права (после `rsync`, иначе он их перезапишет). Mosquitto читает ключ и ACL от
uid `1883`:

```ssh
ssh vps-server 'cd /opt/home-proxy \
  && chown 1883:1883 cert/out/server.key mosquitto/config/acl.conf mosquitto/config/mosquitto.conf \
  && chmod 600 cert/out/server.key mosquitto/config/acl.conf mosquitto/config/mosquitto.conf \
  && chmod 644 cert/out/ca.crt cert/out/server.crt'
```

Порт `8883/tcp` на хосте закрыт ufw. Это реальный сервер с другими сервисами,
так что открывать только `8883/tcp` и ничего больше, осознанно:

```ssh
ssh vps-server 'ufw allow 8883/tcp comment mqtt-tls'
```

Перезапуск только сервиса `mqtt` (STUN не трогаем). На хосте `docker-compose`
версии 1, команды `docker compose` там нет:

```ssh
ssh vps-server 'cd /opt/home-proxy && docker-compose up -d mqtt && sleep 3 && docker logs --tail 15 mosquitto-server'
```

Retained-записи лежат в томе `mosquitto-data` и переживают перезапуск.

## 7. Проверить брокер снаружи

Сначала сам TLS-handshake, без MQTT. Ожидается `Verification: OK` и
`Verify return code: 0 (ok)`:

```ssh
openssl s_client -connect 203.0.113.10:8883 -CAfile cert/out/ca.crt -verify_return_error </dev/null
```

Без `-CAfile` (или с чужим CA) проверка должна упасть с `certificate verify
failed`:

```ssh
openssl s_client -connect 203.0.113.10:8883 -verify_return_error </dev/null
```

Проверка ACL (что чужой клиент ничего не видит) — в
`../mosquitto/README.md`, раздел «Проверка ACL руками».

## 8. Подхватить TLS в `peer` (tokio / rumqttc)

Уже сделано в коде (`connection/src/rendezvous.rs`, `mqtt_options()`):

1. `rumqttc` умеет TLS через rustls (feature `use-rustls`, в версии 0.24 она
   включена по умолчанию). Транспорт MQTT-клиента переключается с plain TCP
   на TLS.
2. Клиент кладёт `ca.crt` (PEM) в `TlsConfiguration::Simple`. Системные корни
   не используются — доверяем только нашему CA, без клиентского сертификата.
3. Handshake делает сам rumqttc внутри tokio-задачи event loop — своего
   `TlsConnector` писать не надо.
4. Адрес брокера остаётся IP: он есть в SAN, поэтому имя проверится без DNS.
5. `client_id` = наш GUID, `username` = GUID пира (по ним ACL брокера
   разрешает доступ), пароль пустой.

Путь к `ca.crt` — пятый аргумент `peer` (`<stun> <mqtt> <mqtt_ca> <my_guid>
<peer_guid>`). `ca.crt` публичный, его можно спокойно копировать на пиры. Раздача,
например на `ruhor` и `po`:

```ssh
scp cert/out/ca.crt ruhor:/root/ca.crt
scp cert/out/ca.crt po:~/ca.crt
```

Пример запуска (одноразовые GUID'ы — `cat /proc/sys/kernel/random/uuid`):

```ssh
./target/release/peer 203.0.113.10:3478 203.0.113.10:8883 ~/ca.crt <мой-guid> <guid-пира>
```

## 9. Перевыпуск серверного сертификата

Когда `server.crt` близок к истечению (проверить срок можно командой ниже) —
повторить п. 3 (`server.key` можно оставить тот же, нужен только новый
CSR/сертификат, либо сгенерировать заново), затем п. 6 и перезапуск брокера.
`ca.crt` у клиентов остаётся прежним, пока не истёк сам CA.

```ssh
openssl x509 -in cert/out/server.crt -noout -enddate
```

## Чего TLS не решает

- **Проверки клиентов.** TLS шифрует канал и подтверждает, что мы говорим с
  настоящим брокером. Пароля у клиентов нет: доступ ограничивает ACL по
  `client_id`/`username` = GUID'ам, и GUID работает как пароль (см.
  `../mosquitto/MosquittoACL.md`).
- **STUN.** Он идёт по UDP в открытом виде, TLS его не касается.
- **Утечки `ca.key`.** Если он попал не в те руки, кто угодно выпустит
  «валидный» сертификат брокера. Держите его вне репозитория и вне
  `vps-server`.
