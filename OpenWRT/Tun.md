# Роутер OpenWrt через `vps-client` в режиме TUN (без WireGuard)

Весь трафик LAN роутера уходит к VPS с белым IP по нашим UDP-дырам как есть, без WireGuard.
Шифрования нет: сами пакеты (HTTPS и т.п.) идут как есть, заголовок каждого пакета в дыре
маскируется XOR своим вектором на каждую дыру. TCP-пакеты получают номер внутри своего потока
(корзина по хэшу соединения), и получатель восстанавливает их порядок; UDP, ICMP и прочее
отдаются сразу. Вариант с WireGuard — [`Wireguard.md`](Wireguard.md).

```
LAN-клиент -> роутер: hp0 (10.80.0.2, TUN vps-client) -> 10 дыр ==== интернет ====
           -> VPS: vps-server (TUN hp0, 10.80.0.1) -> NAT -> интернет
```

Проверено: Xiaomi Mi Router 4C (MT7628, OpenWrt 23.05.4) и VPS на Debian 12. Ниже
`203.0.113.30` — белый IP VPS, `192.168.3.1` — шлюз роутера по WAN.

## 1. VPS

1. Брандмауэр пропускает входящий UDP на порт знакомства и диапазон слотов
   (`ufw allow 40000:40999/udp`).
2. NAT для подсети туннеля наружу и пересылка:
   ```bash
   sysctl -w net.ipv4.ip_forward=1
   iptables -t nat -A POSTROUTING -s 10.80.0.0/16 -o <внешний интерфейс> -j MASQUERADE
   ```
3. `vps-server` с `vps.env` (как в `Wireguard.md`) плюс строка режима TUN:
   ```
   VPS_TUN_ADDR=10.80.0.1/16
   ```
   Запуск от root (нужен `CAP_NET_ADMIN`): при старте поднимается интерфейс `hp0`
   (`VPS_TUN_NAME`, MTU 1400 — `VPS_TUN_MTU`).

## 2. Роутер

1. Модуль TUN: `opkg update && opkg install kmod-tun` (появится `/dev/net/tun`).
2. `vps-client` (сборка — `README.md`, `PACKAGES=vps-client ./OpenWRT/build.sh`) в фоне:
   ```sh
   start-stop-daemon -S -b -m -p /tmp/hp/vps-client.pid -x /bin/sh -- -c \
     'TUN_ADDR=10.80.0.2/24 RUST_LOG=info exec /tmp/hp/vps-client 203.0.113.30:40000 <GUID роутера> <GUID сервера> > /tmp/hp/vps-client.log 2>&1'
   ```
   Поднимается `hp0` (`TUN_NAME`, MTU 1400 — `TUN_MTU`), в логе — `connected(10)`.
3. Маршруты: по умолчанию — в `hp0`, до VPS — по WAN (иначе транспорт уйдёт в петлю):
   ```sh
   uci set network.wan.metric='20'
   uci set network.vps_direct=route
   uci set network.vps_direct.interface='wan'
   uci set network.vps_direct.target='203.0.113.30/32'
   uci set network.vps_direct.gateway='192.168.3.1'
   uci commit network; /etc/init.d/network reload
   ip route replace default dev hp0 metric 0     # после каждого запуска vps-client
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
   `hp0` пересоздаётся при каждом запуске `vps-client` — после запуска сделать
   `/etc/init.d/firewall reload`, иначе он выпадет из flowtable. Штатный
   `firewall.@defaults[0].flow_offloading` при этом выключен.

Если на роутере был поднят WireGuard из `Wireguard.md`, его на время выключить: `ifdown wg0`.

## 3. Проверка

```sh
ip route get 1.1.1.1              # dev hp0
ping -c3 10.80.0.1                # VPS внутри туннеля
wget -qO- http://ifconfig.me/ip   # белый IP VPS
```

С LAN-клиента `traceroute 1.1.1.1`: роутер → `10.80.0.1` → сеть VPS.

## 4. Откат

```sh
start-stop-daemon -K -p /tmp/hp/vps-client.pid     # hp0 исчезает вместе с процессом
ip route del default dev hp0 2>/dev/null
rm -f /etc/nftables.d/90-hp-offload.nft; uci delete firewall.hpvps
uci del_list firewall.@zone[1].device='hp0'; uci commit firewall; /etc/init.d/firewall reload
ifup wg0                                          # если возвращаемся к WireGuard
```

## Скорость (2026-09-26)

LAN-клиент (ноутбук) → Xiaomi Mi Router 4C → VPS, iperf 2, TCP, одно соединение:

| Путь | VPS → клиент | клиент → VPS |
|---|---|---|
| напрямую через WAN роутера | 68,0 Мбит/с | 88,4 Мбит/с |
| WireGuard поверх `vps-client` | 10,2 Мбит/с | 12,1 Мбит/с |
| TUN, `vps-client` без WireGuard | 21,1 Мбит/с | 21,1 Мбит/с |
| **TUN + flow offloading, `notrack`, `connect()`** (п. 5) | **32,6 Мбит/с** | **31,7 Мбит/с** |

Шифрование WireGuard в ядре и лишний UDP-переход через `127.0.0.1` ушли из пути пакета; теперь
процессор роутера почти целиком у `vps-client` (~92%). Буфер порядка на роутере под такой нагрузкой
уходит в потолок ожидания 30 мс (из ~35 тыс. TCP-пакетов по таймауту выдано ~1400).
