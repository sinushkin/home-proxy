# VPS: быстрый старт (5 минут)

Минимальный путь к рабочему туннелю.

## 1. Генерируем GUID

```bash
# На локальной машине
./generate-guids.sh
# Скопируйте значения
export SERVER_GUID=550e8400-e29b-41d4-a716-446655440000
export CLIENT_GUID=550e8400-e29b-41d4-a716-446655440001
```

## 2. Собираем бинарники

```bash
cd ~/home-proxy-public
cargo build --release -p vps-server -p vps-client
```

## 3. VPS-сервер

### 3.1. На сервере (SSH)

```bash
ssh root@203.0.113.10

# Копируем бинарник (если локально собрали)
# scp root@203.0.113.10:/tmp/vps-server /usr/local/bin/

mkdir -p /etc/hp-vps
```

### 3.2. Конфиг сервера

Создаём `/etc/hp-vps/server.env`:

```bash
VPS_PUBLIC_IP=203.0.113.10
MY_ID=550e8400-e29b-41d4-a716-446655440000
PEER_ID=550e8400-e29b-41d4-a716-446655440001
RUST_LOG=info
```

### 3.3. Тестируем вручную

```bash
/usr/local/bin/vps-server --config /etc/hp-vps/server.env
# Должно вывести:
# vps-server: белый IP 203.0.113.10, порт знакомства 40000, порты слотов 40001-49999
# Ctrl+C для выхода
```

### 3.4. Systemd (опционально)

```bash
# Копируем сервис (если есть)
# scp templates/vps-client-server/vps-server.service root@203.0.113.10:/etc/systemd/system/
# systemctl daemon-reload
# systemctl enable --now vps-server
```

### 3.5. Настраиваем NAT

```bash
sysctl -w net.ipv4.ip_forward=1
iptables -t nat -A POSTROUTING -s 10.80.0.0/16 -o eth0 -j MASQUERADE
```

## 4. VPS-клиент

### 4.1. Копируем бинарник

```bash
sudo cp target/release/vps-client /usr/local/bin/
```

### 4.2. Тестируем вручную

```bash
# В переменных окружения или в команде
sudo /usr/local/bin/vps-client 203.0.113.10:40000 \
  550e8400-e29b-41d4-a716-446655440001 \
  550e8400-e29b-41d4-a716-446655440000

# Должно вывести:
# vps-client: сервер 203.0.113.10:40000, TUN hp0 10.80.1.1/16
# дыры 10/10, ...
```

### 4.3. Проверяем туннель

```bash
# В другом терминале
ping 10.80.0.1
# Должны быть ответы
```

## 5. Systemd (опционально)

Отредактируйте `/etc/systemd/system/vps-client.service`, замените в `ExecStart`:
- IP сервера
- UUID клиента
- UUID сервера

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now vps-client
sudo systemctl status vps-client
```

## Проверка

**На сервере:**
```bash
journalctl -u vps-server -f
```

**На клиенте:**
```bash
sudo journalctl -u vps-client -f
```

Оба должны писать про дыры и статистику.

## Если не работает

### Клиент не подключается

- Проверьте, что сервер слушит: `nc -u -z 203.0.113.10 40000`
- Проверьте файервол на сервере: `ufw status` / `iptables -L`
- Проверьте, что диапазон `VPS_PORTS` открыт

### Нет дыр

- Проверьте логи: `journalctl -u vps-client`
- Повторите запуск сервера и клиента

### Нет туннеля после дыр

- Проверьте, что TUN создался: `ip addr show hp0`
- Проверьте маршрут: `ip route | grep 10.80`
- Проверьте NAT на сервере: `iptables -t nat -L POSTROUTING -v`

## Полная документация

Смотрите `README.md` в этой директории.
