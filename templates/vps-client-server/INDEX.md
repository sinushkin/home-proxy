# VPS-клиент / VPS-сервер: справка по файлам

Этот каталог содержит всё необходимое для ручного деплоя туннеля VPS.

## Файлы

### Основная документация

| Файл | Назначение |
|---|---|
| **README.md** | Полное руководство: сборка, настройка, NAT, отладка, параметры |
| **QUICKSTART.md** | Быстрый старт за 5 минут (минимум информации) |
| **OPENWRT.md** | Специфика для OpenWrt: kmod-tun, init-скрипты, логи |

### Конфигурация

| Файл | Назначение |
|---|---|
| **vps-server.env.example** | Пример конфига сервера (обязательные и опциональные переменные) |
| **vps-client.env.example** | Пример переменных окружения клиента |

### Systemd-сервисы

| Файл | Назначение |
|---|---|
| **vps-server.service** | Systemd-юнит для автозапуска сервера; скопировать в `/etc/systemd/system/` |
| **vps-client.service** | Systemd-юнит для автозапуска клиента; отредактировать и скопировать |

### Скрипты

| Файл | Назначение |
|---|---|
| **generate-guids.sh** | Генерирует GUID для сервера и клиента |
| **run-server.sh** | Запускает сервер напрямую (для тестирования, без systemd) |
| **run-client.sh** | Запускает клиент напрямую (для тестирования, без systemd) |

## Типовый процесс

### 1️⃣ Первый раз

```bash
cd templates/vps-client-server

# Генерируем GUID
./generate-guids.sh

# Читаем быстрый старт
cat QUICKSTART.md
```

### 2️⃣ На локальной машине

```bash
cd ~/home-proxy-public

# Собираем оба бинарника
cargo build --release -p vps-server -p vps-client
```

### 3️⃣ На VPS-сервере

Копируете `vps-server` в `/usr/local/bin/`, конфиг в `/etc/hp-vps/`, настраиваете NAT, запускаете через systemd или вручную.

Полные инструкции: [README.md](README.md) → Шаг 3.

### 4️⃣ На клиенте

Копируете `vps-client` в `/usr/local/bin/`, опционально настраиваете systemd-сервис, запускаете.

Полные инструкции: [README.md](README.md) → Шаг 4.

## Выбор способа запуска

### Для проверки / отладки

Используйте скрипты `run-*.sh`:

```bash
# Сервер (требует конфиг vps-server.env)
./run-server.sh vps-server.env

# Клиент (требует GUID и адрес сервера в аргументах)
sudo ./run-client.sh 203.0.113.10:40000 <my-guid> <server-guid>
```

### Для production (Linux)

Используйте systemd-сервисы:

1. Отредактируйте `*.service` файлы
2. Скопируйте в `/etc/systemd/system/`
3. `systemctl daemon-reload && systemctl enable --now vps-server`
4. `systemctl enable --now vps-client`

### Для OpenWrt

Смотрите [OPENWRT.md](OPENWRT.md): там есть init-скрипты и примеры интеграции с luci.

## Параметры

### Быстрые настройки

Достаточно трёх переменных для работы:

```bash
# Сервер
VPS_PUBLIC_IP=203.0.113.10      # ваш белый IP
MY_ID=<guid-1>
PEER_ID=<guid-2>

# Клиент (аргументы)
vps-client 203.0.113.10:40000 <guid-2> <guid-1>
```

### Расширенные настройки

Логирование, MTU, буфер порядка, количество дыр, runtime (однопоточный vs многопоточный) — смотрите [README.md](README.md) → Параметры окружения.

## Типичные ошибки

| Ошибка | Решение |
|---|---|
| "Permission denied /dev/net/tun" | Запустить с `sudo` |
| "не собран vps-client" | `cargo build --release -p vps-client` |
| Дыры не открываются | Проверить логи сервера: `journalctl -u vps-server` |
| Дыры есть, но нет интернета | Проверить NAT на сервере и маршруты на клиенте |
| Конфиг не найден | Скопировать `.example` в `.env` и отредактировать |

## Ссылки в проекте

- `../vps-server/README.md` — полная документация сервера
- `../vps-client/README.md` — полная документация клиента
- `../hp-backend/connection/README.md` — протокол и архитектура
- `../hp-router/README.md` — роутер OpenWrt
- `../OpenWRT/Tun.md` — маршруты и firewalling для OpenWrt

## Чек-лист деплоя

```
Подготовка:
☐ Собраны vps-server и vps-client
☐ Сгенерированы GUID для сервера и клиента
☐ Знаете адрес VPS (белый IP)

VPS-сервер:
☐ Скопирован vps-server в /usr/local/bin/
☐ Создан конфиг /etc/hp-vps/server.env
☐ Тестовый запуск работает
☐ Настроены iptables: ip_forward=1 и MASQUERADE
☐ Установлен systemd-сервис (опционально)

VPS-клиент:
☐ Скопирован vps-client в /usr/local/bin/
☐ Тестовый запуск работает (видны 10 дыр)
☐ Интерфейс hp0 поднялся
☐ Есть маршрут до 10.80.0.1
☐ Ping до сервера работает
☐ Установлен systemd-сервис (опционально)

Готово! Туннель работает.
```
