# VPS-клиент на OpenWrt

> **Несколько клиентов:** поддерживаются. Клиенты перечислены в `clients.txt` — см. раздел
> «Несколько клиентов» в `templates/vps-client-server/README.md`. Ниже — пример с одним клиентом.

Запуск `vps-client` на роутере OpenWrt. Нужен `hp-router` (VPS-режим) или отдельный `vps-client`.

## Установка

### 1. Скопировать бинарник

Собрали `vps-client` на основной машине (Debian/Linux):

```bash
# На основной машине
cargo build --release -p vps-client --target mipsel-unknown-linux-musl
# или просто
cargo build --release -p vps-client
```

На роутер:

```bash
scp target/release/vps-client root@192.168.1.1:/tmp/
ssh root@192.168.1.1
cp /tmp/vps-client /usr/bin/vps-client
chmod +x /usr/bin/vps-client
```

### 2. Проверить TUN

```bash
ssh root@192.168.1.1
opkg update
opkg install kmod-tun
```

Проверяем:

```bash
ls -la /dev/net/tun
# должен быть: crw------- 1 root root
```

### 3. Конфиг и запуск

**Способ 1: вручную в консоли**

```bash
ssh root@192.168.1.1
export TUN_NAME=hp0
export RUST_LOG=info
/usr/bin/vps-client 203.0.113.10:40000 \
  550e8400-e29b-41d4-a716-446655440001 \
  550e8400-e29b-41d4-a716-446655440000
```

Если видите "дыры 10/10", отлично!

**Способ 2: init-скрипт (autorun)**

Создаём `/etc/init.d/vps-client`:

```bash
#!/bin/sh /etc/rc.common
START=95
STOP=05

start() {
    export TUN_NAME=hp0
    export RUST_LOG=info
    syslog -t vps-client "запуск"
    /usr/bin/vps-client 203.0.113.10:40000 \
        550e8400-e29b-41d4-a716-446655440001 \
        550e8400-e29b-41d4-a716-446655440000 \
        &
}

stop() {
    killall vps-client
}

reload() {
    stop
    start
}
```

На роутере:

```bash
scp init.d/vps-client root@192.168.1.1:/etc/init.d/vps-client
ssh root@192.168.1.1 chmod +x /etc/init.d/vps-client
ssh root@192.168.1.1 /etc/init.d/vps-client enable
ssh root@192.168.1.1 /etc/init.d/vps-client start
```

**Способ 3: procd (LuCI)**

Если у вас установлен `luci-app-homeproxy`, там уже есть поддержка.

### 4. Маршруты

После запуска `vps-client` проверяем интерфейс:

```bash
ip addr show hp0
# должно быть что-то типа:
# hp0: ... inet 10.80.1.1/16 scope global hp0
```

Добавляем маршруты так, чтобы трафик шёл через туннель:

```bash
# Весь трафик через туннель
ip route replace default via 10.80.0.1 dev hp0

# Или только определённые сети
ip route add 0.0.0.0/0 via 10.80.0.1 dev hp0
```

### 5. Логи

```bash
# Прямой вывод
logread | grep vps-client

# Или если запустили вручную:
# видите в консоли сразу
```

## Проблемы

### kmod-tun не ставится

```bash
opkg list-installed | grep kmod-tun
# Если нет, может быть несовместимость версии OpenWrt.
# Проверьте, что установлены нужные зависимости ядра:
opkg install kernel
```

### "Permission denied /dev/net/tun"

Бинарник должен запускаться с правами root:

```bash
ls -la /dev/net/tun
# Если права 600 и владелец root, всё ОК
# Если нет: chmod 666 /dev/net/tun
```

### Дыры не открываются

1. Проверьте, что сервер слушит:
   ```bash
   nc -u -z 203.0.113.10 40000 && echo "OK" || echo "FAIL"
   ```

2. Проверьте логи сервера (на VPS):
   ```bash
   journalctl -u vps-server -n 20
   ```

3. Проверьте логи клиента:
   ```bash
   logread | tail -20
   ```

### Туннель работает, но нет интернета

Проверьте маршруты и NAT на сервере:

1. На роутере маршрут до 10.80.0.1:
   ```bash
   ip route | grep 10.80
   ```

2. На VPS включен форвардинг и NAT:
   ```bash
   ssh root@203.0.113.10
   sysctl net.ipv4.ip_forward
   # net.ipv4.ip_forward = 1
   
   iptables -t nat -L POSTROUTING -v
   # должно быть правило для 10.80.0.0/16
   ```

## Производительность

- Однопоточный tokio (по умолчанию) экономит CPU на одноядерных роутерах.
- Если понадобится многопоточный: `export RUNTIME=multi`
- TCP с номерами в потоке — может тормозить на старых MIPS. Отключить: `export REORDER_WAIT_MS=0`

## Ссылки

- `../OpenWRT/Tun.md` — маршруты и firewalling
- `../OpenWRT/README.md` — сборка для OpenWrt
- `README.md` — полная документация
