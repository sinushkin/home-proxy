# MQTT-брокер: доступ, ACL и просмотр содержимого

Как устроен доступ к Mosquitto и как заглянуть в то, что на нём реально лежит —
какие пиры зарегистрированы, какой у них адрес, когда они там появились.
Про TLS-сертификаты брокера — `cert/README.md`.

## Что там лежит

`connection` публикует ровно одно семейство сообщений на MQTT — `Rendezvous`
(см. `hp-backend/connection/proto/connection.proto`), retained-сообщением на
топике:

```
home-proxy/rendezvous/<guid>/<slot>
```

`<guid>` — GUID **самого пира**, `<slot>` — номер дыры (0..9). Пиры заранее
знают GUID друг друга: каждый публикует свои слоты на топиках своего GUID и
подписывается на слоты пира по `home-proxy/rendezvous/<guid-пира>/+`. Т.е. на
брокере будет до 10 топиков на пира. Payload — бинарный protobuf
(`Rendezvous { public_ip, public_port, peer_id, registered_at_unix_ms,
session_id, slot, key, extra_endpoints }`), не текст, поэтому смотреть его сырым `mosquitto_sub`
не очень полезно — ниже показано, как декодировать.

**TTL.** Публикация идёт по MQTT 5 с `message_expiry_interval` = 60 c, так
что брокер сам удаляет протухшую регистрацию. Пир обновляет свой слот, пока
не залинкуется; как залинковался — перестаёт, и запись исчезает сама.
Поэтому чаще всего активных записей на брокере вообще не видно — они есть
только пока идёт набор дыр.

Само UDP-соединение (`Punch`/`PunchAck`/`KeepAlive`) по MQTT не ходит — оно
летит напрямую между пирами, в брокере его не видно.

## Доступ: TLS и ACL

Наружу слушает **только** `8883` (MQTT over TLS). Открытого `1883` снаружи нет,
`9001` (WebSockets) убран.

Пароля нет — брокер его не проверяет. Клиент представляется так:

- `client_id` = **его собственный GUID**;
- `username` = **GUID пира, которого он ищет**.

`mosquitto/config/acl.conf` строится на этих двух полях и не содержит общих
правил, так что всё, что не разрешено, запрещено:

```
pattern write home-proxy/rendezvous/%c/+   # писать только свои слоты
pattern read  home-proxy/rendezvous/%u/+   # читать только слоты искомого пира
```

Отсюда следует: перечислить устройства через wildcard (`#`, `+/+`, `$SYS/#`)
нельзя — подписка на них ничего не отдаёт. Каждый пир знает ровно того, кого
ищет. Проверено на живом брокере, в том числе с `username` = `+` и `#`.

GUID здесь работает как пароль: знаешь чужой GUID — значит, ты «свой». Подробно
про устройство ACL, конфиг и проверки — `MosquittoACL.md`.

## Админский доступ (обход ACL)

В `mosquitto.conf` есть второй листенер `1883`, привязанный к `127.0.0.1`
**внутри контейнера**, без ACL. Порт наружу не пробрасывается, достучаться
можно только через `docker exec` — это и есть способ для оператора видеть всё.
Все примеры ниже используют его (`-h 127.0.0.1 -p 1883`).

## Инструменты

`mosquitto_sub`/`mosquitto_pub` ставить не нужно — они уже есть внутри
контейнера `eclipse-mosquitto`, вызываем через `docker exec`. А вот
**`protoc` нужен на той машине, где выполняется `protoc --decode`** — это
может быть либо ваша машина с чекаутом репозитория (тогда `docker exec`
идёт через `ssh`, а `protoc` работает локально, decode всегда в ногу с
актуальным `.proto`), либо сам хост брокера, если вы уже залогинены туда
напрямую. На `profit` `protoc` и копия `connection.proto` уже положены
(`/opt/home-proxy/proto/connection.proto`) — но это ручная копия для
удобства, при изменении протокола её надо обновить руками (`rsync
hp-backend/connection/proto/connection.proto profit:/opt/home-proxy/proto/`).
Из-за этого **вариант через `ssh`-пайп (см. ниже) надёжнее** — он всегда
берёт `.proto` из вашего рабочего чекаута.

## Список зарегистрированных пиров (топиков)

У MQTT нет команды "покажи все топики" — топики видно, только подписавшись.
Через админский листенер можно подписаться на wildcard — брокер сразу отдаст
все retained-сообщения, которые на нём есть (по одному топику на каждый
зарегистрированный слот пира):

```bash
docker exec -i mosquitto-server mosquitto_sub -h 127.0.0.1 -p 1883 -v -t 'home-proxy/rendezvous/#' -W 3
```

- `-v` — печатать топик перед payload'ом (чтобы видеть GUID пира, которому
  принадлежит сообщение).
- `-W 3` — выйти, если 3 секунды ничего не приходит (иначе `mosquitto_sub`
  продолжит висеть и ждать новых).

Payload будет выглядеть как бинарный мусор вперемешку с читаемым IP —
это нормально, protobuf кодирует строки как есть, а числа — varint'ами.

## Декодирование одной записи

**Вариант 1 (рекомендуется): с вашей машины через SSH**, брокер на `profit`,
репозиторий у вас же — `protoc` локальный, `.proto` всегда актуальный, на
`profit` ничего ставить не нужно. Выполнять из `hp-backend/connection/`
(там `proto/connection.proto` — относительный путь):

```bash
ssh profit docker exec -i mosquitto-server mosquitto_sub -h 127.0.0.1 -p 1883 -N -t 'home-proxy/rendezvous/<guid>/<slot>' -C 1 -W 3 \
  | protoc --decode=hp_backend.connection.Rendezvous proto/connection.proto
```

**Вариант 2: залогинены прямо на `profit`.** Там уже есть `protoc` и копия
`.proto` в `/opt/home-proxy/proto` (см. предупреждение про ручную синхронизацию
выше). Выполнять из `/opt/home-proxy/proto/`:

```bash
docker exec -i mosquitto-server mosquitto_sub -h 127.0.0.1 -p 1883 -N -t 'home-proxy/rendezvous/<guid>/<slot>' -C 1 -W 3 \
  | protoc --decode=hp_backend.connection.Rendezvous connection.proto
```

**Вариант 3: локальный `docker compose up` для отладки** — из
`hp-backend/connection/`, так же, как в варианте 1, только без `ssh profit`
перед `docker exec`. Для локального стека сначала сгенерируйте сертификаты
(`cert/README.md`, шаги 1–4): без них брокер не стартует.

Во всех случаях `protoc` требует запускаться из каталога, где лежит
`.proto` (или с явным `--proto_path`/`-I`) — абсолютный путь вида
`/opt/home-proxy/proto/connection.proto` без `--proto_path` он откажется
принимать с ошибкой `File does not reside within any path specified using
--proto_path`.

`-N` у `mosquitto_sub` **обязателен** — без него утилита допечатывает `\n`
после payload'а, что портит бинарный protobuf и `protoc --decode` падает с
`Failed to parse input.`

Пример вывода:

```
public_ip: "198.51.100.7"
public_port: 20006
peer_id: "305ee763-5302-43bb-b162-f42db8012cfb"
registered_at_unix_ms: 1790169870941
```

`registered_at_unix_ms` — Unix-время в миллисекундах, когда запись была
опубликована. Поле оставлено для диагностики; за уборку теперь отвечает TTL
(см. выше), так что зависших записей на брокере быть не должно.

## Проверка ACL руками

Для проверки, что чужой клиент ничего не видит (через `docker exec` внутри
контейнера, порт `8883` с TLS). Подставьте одноразовые GUID'ы: `A` — чья запись
лежит, `C` — «посторонний»:

```bash
T="docker exec mosquitto-server mosquitto_sub -h 127.0.0.1 -p 8883 --cafile /mosquitto/certs/ca.crt -V 5 -v -W 2"
$T -i <C> -u <C> -t 'home-proxy/rendezvous/#'          # пусто: wildcard запрещён
$T -i <C> -u <A> -t 'home-proxy/rendezvous/<A>/+'      # видит слоты A: знает его GUID
```

## Удаление записи вручную

Обычно не нужно — записи протухают сами по TTL. Но если хочется стереть
конкретный слот сразу, публикуем пустой payload с retain через админский
листенер:

```bash
docker exec mosquitto-server mosquitto_pub -h 127.0.0.1 -p 1883 -t 'home-proxy/rendezvous/<guid>/<slot>' -n -r
```

`-n` — пустой payload, `-r` — retained, то есть брокер удаляет сохранённую
запись на этом топике.
