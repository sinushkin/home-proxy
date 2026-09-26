# Роутер OpenWrt: `hp-router` — дом через VPS и телефоны

Весь трафик LAN и Wi-Fi роутера уходит к VPS с белым IP по нашим UDP-дырам как есть, без
WireGuard; телефоны пробиваются до роутера и выходят в интернет тем же путём. Шифрования нет:
сами пакеты (HTTPS и т.п.) идут как есть, каждый пакет в дыре подписан (первые 128 байт и длина)
и его заголовок замаскирован XOR. TCP-пакеты получают номер внутри своего потока (корзина по хэшу
соединения), и получатель восстанавливает их порядок; UDP, ICMP и прочее отдаются сразу.

```
LAN-клиент -> роутер: hp0 (10.80.0.2, TUN hp-router) -> 10 дыр ==== интернет ====
           -> VPS: vps-server (TUN hp0, 10.80.0.1/16) -> NAT -> интернет
телефон 10.80.1.<n> -> 10 P2P-дыр -> роутер (hp-router, без TUN) -> дыры к VPS -> ...
```

Раньше здесь был WireGuard поверх `vps-client` (10–12 Мбит/с на этом роутере) — от него
отказались, см. «Скорость» ниже.

Проверено: Xiaomi Mi Router 4C (MT7628, OpenWrt 23.05.4), VPS на Debian 12, телефон Android на
LTE. Ниже `203.0.113.30` — белый IP VPS, `192.168.3.1` — шлюз роутера по WAN, LAN —
`192.168.1.0/24`.

## 1. VPS

1. Брандмауэр пропускает входящий UDP на порт знакомства и диапазон слотов
   (`ufw allow 40000:40999/udp`).
2. NAT для подсети туннеля наружу и пересылка:
   ```bash
   sysctl -w net.ipv4.ip_forward=1
   iptables -t nat -A POSTROUTING -s 10.80.0.0/16 -o <внешний интерфейс> -j MASQUERADE
   ```
3. `vps-server` (`../vps-server/README.md`) с `vps.env`:
   ```
   VPS_PUBLIC_IP=203.0.113.30
   VPS_PORTS=40001-40999
   MY_ID=<GUID сервера>
   PEER_ID=<GUID роутера для VPS>
   TUN_ADDR=10.80.0.1/16
   ```
   Запуск от root (нужен `CAP_NET_ADMIN`): при старте поднимается интерфейс `hp0`.

## 2. Роутер

1. Модуль TUN: `opkg update && opkg install kmod-tun` (появится `/dev/net/tun`).
2. `hp-router` (сборка — `README.md`, `PACKAGES=hp-router ./OpenWRT/build.sh`) и настройки
   `router.env` рядом с ним (образец — `../hp-router/router.env.example`):
   ```
   VPS_SERVER=203.0.113.30:40000
   VPS_MY_ID=<GUID роутера для VPS>
   VPS_PEER_ID=<GUID сервера>
   TUN_ADDR=10.80.0.2/24
   STUN_ADDR=203.0.113.10:3499
   MQTT_ADDR=203.0.113.10:8883
   MQTT_CA=ca.crt
   PHONE_1_MY_ID=<GUID роутера для телефона 1>
   PHONE_1_PEER_ID=<GUID телефона 1>
   ```
   Запуск в фоне:
   ```sh
   start-stop-daemon -S -b -m -p /tmp/hp/hp-router.pid -x /bin/sh -- -c \
     'cd /tmp/hp && exec ./hp-router --config /tmp/hp/router.env > /tmp/hp/hp-router.log 2>&1'
   ```
   Поднимается `hp0`, в логе — `[vps] ... connected(10)`, для телефонов — `[phone1] ...`.
3. Маршруты: в `hp0` — только LAN (правило по источнику). Сам роутер — STUN, MQTT, дыры к VPS и
   к телефонам — ходит напрямую через WAN, иначе дыры телефонов ушли бы через VPS:
   ```sh
   ip rule add prio 99 lookup main suppress_prefixlength 0   # сначала конкретные маршруты main
   ip rule add prio 100 from 192.168.1.0/24 table 100         # LAN — в таблицу 100
   ip route replace default dev hp0 table 100                  # после каждого запуска hp-router
   ```
4. `hp0` в зону `wan` (NAT для LAN и ограничение MSS под MTU 1400):
   ```sh
   uci add_list firewall.@zone[1].device='hp0'
   uci commit firewall; /etc/init.d/firewall reload
   ```
5. Ускорение (+50% на MT7628, см. `Performance.md`): транспорт дыр мимо conntrack и программный
   flow offloading с `hp0` (штатный `flow_offloading` fw4 TUN во flowtable не включает). NAT
   остаётся. `/etc/nftables.d/90-hp-offload.nft`:
   ```
   # UDP дыр до VPS — мимо conntrack.
   chain hp_notrack_pre {
   	type filter hook prerouting priority raw; policy accept;
   	ip saddr 203.0.113.30 udp sport 40000-40999 notrack
   }
   chain hp_notrack_out {
   	type filter hook output priority raw; policy accept;
   	ip daddr 203.0.113.30 udp dport 40000-40999 notrack
   }
   # Установленные соединения LAN <-> hp0 — по быстрому пути (NAT делает flowtable).
   flowtable hpft {
   	hook ingress priority filter; devices = { eth0.1, eth0.2, hp0 };
   	counter
   }
   chain hp_offload {
   	type filter hook forward priority filter - 1; policy accept;
   	meta l4proto { tcp, udp } ct state established flow add @hpft
   }
   ```
   Входящий UDP без conntrack не попадает под «established», его надо разрешить явно:
   ```sh
   uci set firewall.hpvps=rule
   uci set firewall.hpvps.name=hp-vps-transport
   uci set firewall.hpvps.src=wan
   uci set firewall.hpvps.proto=udp
   uci set firewall.hpvps.src_ip=203.0.113.30
   uci set firewall.hpvps.src_port=40000-40999
   uci set firewall.hpvps.target=ACCEPT
   uci commit firewall; /etc/init.d/firewall reload
   ```
   `hp0` пересоздаётся при каждом запуске `hp-router` — после запуска сделать
   `/etc/init.d/firewall reload`, иначе он выпадет из flowtable. Штатный
   `firewall.@defaults[0].flow_offloading` при этом выключен.

## 3. Телефон

В приложении (`../android-vpn/README.md`): GUID телефона (`PHONE_<n>_PEER_ID`), GUID роутера
(`PHONE_<n>_MY_ID`), адрес в туннеле `10.80.1.<n>`, те же STUN и MQTT. Телефон должен быть в
другой сети, чем роутер (например, на мобильном интернете): hairpin NAT на домашнем шлюзе
обычно не работает.

## 4. Проверка

```sh
ip route get 1.1.1.1 from 192.168.1.10 iif br-lan   # dev hp0 (LAN)
ip route get 1.1.1.1                                # через WAN (сам роутер)
ping -c3 10.80.0.1                                  # VPS внутри туннеля
```

С LAN-клиента `traceroute 1.1.1.1`: роутер → `10.80.0.1` → сеть VPS; внешний IP — VPS. В логе
роутера раз в 30 с: дыры к VPS, пакеты из TUN и в TUN, телефоны (дыры, пересылка к VPS и к
телефонам).

## 5. Откат

```sh
start-stop-daemon -K -p /tmp/hp/hp-router.pid      # hp0 исчезает вместе с процессом
ip rule del prio 99; ip rule del prio 100; ip route flush table 100
rm -f /etc/nftables.d/90-hp-offload.nft; uci delete firewall.hpvps
uci del_list firewall.@zone[1].device='hp0'; uci commit firewall; /etc/init.d/firewall reload
```

## Скорость (2026-09-26)

LAN-клиент (ноутбук) → Xiaomi Mi Router 4C → VPS, iperf 2, TCP, одно соединение:

| Путь | VPS → клиент | клиент → VPS |
|---|---|---|
| напрямую через WAN роутера | 68,0 Мбит/с | 88,4 Мбит/с |
| WireGuard поверх `vps-client` (прежняя схема) | 10,2 Мбит/с | 12,1 Мбит/с |
| TUN, `vps-client` без WireGuard | 21,1 Мбит/с | 21,1 Мбит/с |
| TUN + flow offloading, `notrack`, `connect()` (п. 5) | 32,6 Мбит/с | 31,7 Мбит/с |
| **то же + подпись пакетов** (`hp-router`) | **25,4 Мбит/с** | **27,0 Мбит/с** |

Отказ от WireGuard убрал из пути пакета шифрование в ядре и лишний UDP-переход через
`127.0.0.1`: вдвое быстрее. Процессор роутера почти целиком у нашего процесса; подпись стоит
~19 мкс на пакет (по всему пакету было бы ~50). Буфер порядка на роутере под такой нагрузкой
уходит в потолок ожидания 30 мс.

Телефон на LTE (через точку доступа) → роутер → VPS → speedtest: 14 Мбит/с на приём, 7 на
отдачу.
