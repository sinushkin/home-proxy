# VPS-клиент / VPS-сервер: ручной деплой

> **Несколько клиентов:** поддерживаются. Клиенты перечислены в `clients.txt` — см. раздел
> «Несколько клиентов» в `templates/vps-client-server/README.md`. Ниже — пример с одним клиентом.

Руководство по сборке, настройке и запуску `vps-server` и `vps-client` вручную через systemd.

## Предварительные условия

### Сервер
- VPS с белым IP (`VPS_PUBLIC_IP`)
- Доступ по SSH с правами root (или sudo)
- Открытые UDP-порты:
  - Порт знакомства (по умолчанию `40000`)
  - Диапазон портов слотов (по умолчанию `40001-49999`)
- Linux (протестировано на Debian, Ubuntu, OpenWrt)

### Клиент
- Linux с root (необходим для TUN)
- `/dev/net/tun` (включить kmod-tun на OpenWrt)
- Доступ к серверу через интернет

## Шаг 1: Генерация GUID

GUID — это общий секрет пары сервер-клиент. Генерируем два разных GUID:

```bash
# На локальной машине (не на сервере!)
uuidgen > server-guid.txt
uuidgen > client-guid.txt

cat server-guid.txt
# Результат (пример): 550e8400-e29b-41d4-a716-446655440000

cat client-guid.txt
# Результат (пример): 550e8400-e29b-41d4-a716-446655440001
```

Запомните эти GUID или сохраните в переменные окружения. **Не коммитьте в git**, держите локально.

## Шаг 2: Сборка бинарников

Собираем оба бинарника из корня репозитория:

```bash
cd ~/home-proxy-public
cargo build --release -p vps-server
cargo build --release -p vps-client

# Проверяем, что собрались
ls -lh target/release/vps-server target/release/vps-client
```

Размер примерно 30–40 МБ (release-сборка со всеми символами).

## Шаг 3: Развёртывание VPS-сервера

### Копирование бинарника и конфига

Подключитесь к серверу:

```bash
ssh root@203.0.113.10

# Создаём директории
mkdir -p /etc/hp-vps /var/lib/hp-vps
```

На локальной машине скопируйте файлы:

```bash
# С локальной машины
scp target/release/vps-server root@203.0.113.10:/usr/local/bin/vps-server
scp templates/vps-client-server/vps-server.env.example root@203.0.113.10:/etc/hp-vps/server.env.example
```

### Настройка конфига сервера

На сервере отредактируйте `/etc/hp-vps/server.env`:

```bash
ssh root@203.0.113.10
cp /etc/hp-vps/server.env.example /etc/hp-vps/server.env
nano /etc/hp-vps/server.env
```

Необходимые переменные:

```bash
# IP сервера в интернете (основной IP интерфейса)
VPS_PUBLIC_IP=203.0.113.10

# GUID сервера (из шага 1)
MY_ID=550e8400-e29b-41d4-a716-446655440000

# GUID клиентов — не здесь, а в clients.txt (раздел «Несколько клиентов»)

# (опционально) логирование
RUST_LOG=info
LOG_FILE=/var/log/hp-vps/server.log
```

Проверьте права на файл:

```bash
chmod 600 /etc/hp-vps/server.env
```

### Тестовый запуск сервера

```bash
/usr/local/bin/vps-server --config /etc/hp-vps/server.env
# Вывод:
# vps-server: белый IP 203.0.113.10, порт знакомства 40000, порты слотов 40001-49999
```

Если всё работает, остановите Ctrl+C.

### Установка systemd-сервиса сервера

```bash
scp templates/vps-client-server/vps-server.service root@203.0.113.10:/etc/systemd/system/

ssh root@203.0.113.10
systemctl daemon-reload
systemctl enable vps-server
systemctl start vps-server

# Проверяем статус
systemctl status vps-server
journalctl -u vps-server -n 20
```

### Настройка NAT на сервере

IP-пакеты из туннеля должны уходить в интернет через NAT:

```bash
ssh root@203.0.113.10

# Включаем форвардинг
sysctl -w net.ipv4.ip_forward=1

# Добавляем правило NAT (замените eth0 на основной интерфейс)
iptables -t nat -A POSTROUTING -s 10.80.0.0/16 -o eth0 -j MASQUERADE

# Сохраняем правило (опционально, зависит от дистрибутива)
# Debian/Ubuntu:
iptables-save > /etc/iptables/rules.v4
```

Проверяем:

```bash
sysctl net.ipv4.ip_forward
# net.ipv4.ip_forward = 1

iptables -t nat -L POSTROUTING -v
# должно быть правило с 10.80.0.0/16
```

## Шаг 4: Развёртывание VPS-клиента

### Копирование бинарника

На локальной машине (та, которая будет клиентом):

```bash
sudo cp target/release/vps-client /usr/local/bin/vps-client
sudo chmod +x /usr/local/bin/vps-client
```

### Тестовый запуск клиента

Запустите вручную перед сервисом:

```bash
sudo /usr/local/bin/vps-client \
  203.0.113.10:40000 \
  550e8400-e29b-41d4-a716-446655440001 \
  550e8400-e29b-41d4-a716-446655440000

# Вывод (пример):
# vps-client: сервер 203.0.113.10:40000, TUN hp0 10.80.1.1/16
# дыры 10/10, к серверу 100 (TCP с номером 100), от сервера 100, потеряно 0
```

Если видите примерно такой вывод, все дыры открыты. Остановите Ctrl+C.

### Установка systemd-сервиса клиента

Отредактируйте `vps-client.service`, заменив значения на реальные:

```bash
# На локальной машине
sudo nano templates/vps-client-server/vps-client.service
```

Замените в строке `ExecStart`:
- `203.0.113.10:40000` — адрес вашего сервера
- первый UUID — ваш GUID клиента
- второй UUID — GUID сервера

Установите сервис:

```bash
sudo cp templates/vps-client-server/vps-client.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable vps-client
sudo systemctl start vps-client

# Проверяем
sudo systemctl status vps-client
sudo journalctl -u vps-client -n 20
```

### Настройка маршрутов (опционально)

Чтобы весь трафик шёл через туннель:

```bash
sudo ip route add 10.80.0.0/16 dev hp0
# или весь трафик:
# sudo ip route add 0.0.0.0/0 via 10.80.0.1 dev hp0
```

Проверяем:

```bash
ping 10.80.0.1
# должны быть ответы
```

## Шаг 5: Проверка соединения

### На сервере

```bash
journalctl -u vps-server -f
# Ищите: "приход пакетов", "добавление слота", "дыры"
```

### На клиенте

```bash
sudo journalctl -u vps-client -f
# Должны видеть: "дыры 10/10"
```

Если дыры открываются медленно, это нормально: каждая дыра пробивается за 1–5 секунд.

### Трафик

Если маршруты настроены, попробуйте ping:

```bash
ping 10.80.0.1
# должны быть ответы от сервера
```

## Отладка

### Проверка портов на сервере

```bash
ssh root@203.0.113.10
ss -ulnp | grep 400
# Должны быть слушающие сокеты на портах 40000-40999
```

### Логи

**Сервер:**
```bash
ssh root@203.0.113.10
journalctl -u vps-server -e
# или в файл (если LOG_FILE задан):
tail -f /var/log/hp-vps/server.log
```

**Клиент:**
```bash
sudo journalctl -u vps-client -e
```

### Проверка от другой машины

Если хотите убедиться, что сервер слушает:

```bash
nc -u -z 203.0.113.10 40000
# или
echo "" | ncat -u 203.0.113.10 40000
```

## Удаление сервисов

Если нужно остановить и удалить:

**Сервер:**
```bash
ssh root@203.0.113.10
systemctl stop vps-server
systemctl disable vps-server
rm /etc/systemd/system/vps-server.service
systemctl daemon-reload
```

**Клиент:**
```bash
sudo systemctl stop vps-client
sudo systemctl disable vps-client
sudo rm /etc/systemd/system/vps-client.service
sudo systemctl daemon-reload
```

## Параметры окружения (полный список)

### VPS-сервер

| Переменная | По умолчанию | Описание |
|---|---|---|
| `VPS_PUBLIC_IP` | **обязателен** | Белый IP сервера |
| `VPS_BOOTSTRAP_PORT` | 40000 | Порт знакомства |
| `VPS_PORTS` | 40001-49999 | Диапазон портов слотов |
| `MY_ID` | опционально | GUID сервера, общий для всех клиентов |
| `CLIENTS_FILE` | `clients.txt` | файл клиентов, перечитывается каждые 2 с |
| `TUN_ADDR` | 10.80.0.1/16 | Адрес сервера и подсеть |
| `TUN_NAME` | hp0 | Имя интерфейса TUN |
| `TUN_MTU` | 1400 | MTU интерфейса |
| `ADDRESS_FILE` | addresses.state | Файл выданных адресов |
| `DNS` | резолверы сервера | DNS для клиентов |
| `RUST_LOG` | info | Уровень логирования |
| `LOG_FILE` | не задан | Файл логов (дописывается) |
| `REORDER_WAIT_MS` | 30 | Буфер порядка TCP (0 — выключить) |
| `DATA_HOLES` | 0 | Дыры для данных (0 — все) |
| `RUNTIME` | single | `multi` — многопоточный tokio |

### VPS-клиент (аргументы)

```bash
vps-client <ip_сервера[:порт]> <мой_guid> <guid_сервера>
```

### VPS-клиент (переменные окружения)

| Переменная | По умолчанию | Описание |
|---|---|---|
| `TUN_NAME` | hp0 | Имя интерфейса TUN |
| `TUN_MTU` | 1400 | MTU интерфейса |
| `RUST_LOG` | info | Уровень логирования |
| `LOG_TARGET` | - | `syslog` для OpenWrt |
| `REORDER_WAIT_MS` | 8 | Буфер порядка TCP (0 — выключить) |
| `DATA_HOLES` | 0 | Дыры для данных (0 — все) |
| `RUNTIME` | single | `multi` — многопоточный tokio |

## Примеры

### Несколько клиентов

Сервер один (`MY_ID`), клиентов сколько угодно — каждому свой GUID. Список клиентов — в файле
`clients.txt` рядом с `vps.env` (путь можно задать `CLIENTS_FILE`):

```
# клиенты VPS-сервера: один GUID на строку, # — комментарий
550e8400-e29b-41d4-a716-446655440001
550e8400-e29b-41d4-a716-446655440002
```

- **Добавить клиента:** дописать его GUID в файл. **Удалить:** убрать строку. Сервер перечитывает
  файл каждые 2 с и **не перезапускается**; другие клиенты не затрагиваются.
- Клиент подключается как раньше: своим GUID и GUID сервера (`MY_ID`), адрес сервера — тот же.
- Два клиента с одинаковой первой группой GUID (первые 8 hex-символов) не могут работать вместе:
  второй строка пропускается с предупреждением в логе.
- Пустой файл (или только комментарии) — **все клиенты удаляются**. Если файла нет или он не
  читается, список не меняется.
- Удалённого клиента сервер забывает. Его процесс `vps-client` сам не остановится, пока его не
  остановят на клиенте.
- Диапазон `VPS_PORTS` общий: на клиента уходит 10 портов из него, поэтому ёмкость примерно
  размер диапазона / 10.

## Минимальная конфигурация сервера

`/etc/hp-vps/server.env`:
```bash
VPS_PUBLIC_IP=203.0.113.10
MY_ID=550e8400-e29b-41d4-a716-446655440000
# клиенты — в clients.txt (см. «Несколько клиентов»)
```

### Расширенная конфигурация сервера

`/etc/hp-vps/server.env`:
```bash
VPS_PUBLIC_IP=203.0.113.10
MY_ID=550e8400-e29b-41d4-a716-446655440000
# клиенты — в clients.txt (см. «Несколько клиентов»)
VPS_BOOTSTRAP_PORT=40000
VPS_PORTS=40001-49999
TUN_ADDR=10.80.0.1/16
DNS=8.8.8.8,1.1.1.1
RUST_LOG=info,connection=debug
LOG_FILE=/var/log/hp-vps/server.log
RUNTIME=multi
```

### Systemd-сервис клиента с переменными окружения

`/etc/systemd/system/vps-client.service`:
```ini
[Service]
ExecStart=/usr/local/bin/vps-client 203.0.113.10:40000 550e8400-e29b-41d4-a716-446655440001 550e8400-e29b-41d4-a716-446655440000
Environment="TUN_NAME=hp0"
Environment="RUST_LOG=info,connection=debug"
Environment="RUNTIME=multi"
```

## Ссылки

- `../vps-server/README.md` — документация сервера
- `../vps-client/README.md` — документация клиента
- `../hp-backend/connection/README.md` — протокол и архитектура
- `../OpenWRT/Tun.md` — маршруты и конфигурация для OpenWrt
