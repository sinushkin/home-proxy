# WireGuard на роутере OpenWrt через `vps-client`

Роутер с OpenWrt выпускает весь трафик своей LAN через WireGuard к VPS с белым IP. Транспорт
для WireGuard — наши 10 UDP-дыр: `vps-client` на роутере ↔ `vps-server` на VPS (схема
«клиент — сервер», без STUN, MQTT и пробива; см. `../vps-server`, `../vps-client`).

```
LAN-клиент -> роутер: wg0 (10.78.0.3) -> 127.0.0.1:51821 (мост vps-client)
           -> 10 дыр ==== интернет ==== VPS: vps-server (порт знакомства 40000, слоты 40001-40999)
           -> 127.0.0.1:51820 -> WireGuard wghp (10.78.0.1) -> NAT -> интернет
```

Проверено на Xiaomi Mi Router 4C (MT7628, OpenWrt 23.05.4) и VPS на Debian 12. Ниже
`203.0.113.30` — белый IP VPS, `192.168.3.1` — шлюз, через который роутер выходит в интернет
по WAN; подставьте свои.

## 1. VPS: WireGuard и `vps-server`

На VPS уже поднят WireGuard `wghp` (`10.78.0.1/24`, слушает `127.0.0.1:51820`, NAT для
подсети) — как в `../wireguard/README.md` или вручную. Дальше:

1. Брандмауэр пропускает входящий UDP на порт знакомства и диапазон слотов, например
   `ufw allow 40000:40999/udp`. Диапазон не должен пересекаться с чужими сервисами.
2. `vps-server` (статическая сборка под musl, например из `../wsl/Dockerfile` с `-p vps-server`)
   и `vps.env`:
   ```
   VPS_PUBLIC_IP=203.0.113.30
   VPS_BOOTSTRAP_PORT=40000
   VPS_PORTS=40001-40999
   MY_ID=<GUID сервера>
   PEER_ID=<GUID роутера>
   WG_ADDR=127.0.0.1:51820
   LOG_FILE=/opt/home-proxy/vps.log
   ```
   Запуск: `vps-server --config vps.env` (или юнит systemd).
3. Пир роутера в WireGuard VPS (публичный ключ роутера из шага 3.2):
   ```bash
   wg set wghp peer <публичный ключ роутера> allowed-ips 10.78.0.3/32
   ```
   и то же в конфиге `wghp`, чтобы пережило перезапуск.

## 2. Роутер: `vps-client`

Сборка под mipsel (тулчейн OpenWrt, см. `README.md`):

```bash
TOOLCHAIN_DIR=~/owrt/staging_dir/toolchain-mipsel_24kc_gcc-12.3.0_musl PACKAGES=vps-client ./OpenWRT/build.sh
scp -O target/openwrt/mipsel-unknown-linux-musl/release/vps-client root@<роутер>:/tmp/hp/
```

Запуск в фоне (`nohup` в busybox OpenWrt нет, есть `start-stop-daemon`):

```sh
start-stop-daemon -S -b -m -p /tmp/hp/vps-client.pid -x /bin/sh -- -c \
  'RUST_LOG=info exec /tmp/hp/vps-client 203.0.113.30:40000 <GUID роутера> <GUID сервера> 51821 > /tmp/hp/vps-client.log 2>&1'
```

В логе должно появиться `состояние соединения: ... -> connected(10)`. `/tmp` на роутере — в
оперативной памяти: после перезагрузки бинарник и процесс пропадут (постоянный вариант —
бинарник во флеш и init-скрипт procd; пока не сделано).

## 3. Роутер: WireGuard

1. Пакеты (≈140 КБ флеша) и **перезапуск сети**: обработчик протокола `wireguard` netifd
   подхватывает только при `restart`, после `reload` интерфейс стоит в `NO_DEVICE`.
   ```sh
   opkg update && opkg install kmod-wireguard wireguard-tools
   /etc/init.d/network restart
   ```
2. Ключи (на любой машине с `wg`): `wg genkey | tee wg.key | wg pubkey > wg.pub`.
3. Интерфейс `wg0`, пир — WireGuard VPS через мост `vps-client`:
   ```sh
   cp /etc/config/network /etc/config/network.bak; cp /etc/config/firewall /etc/config/firewall.bak
   uci set network.wg0=interface
   uci set network.wg0.proto='wireguard'
   uci set network.wg0.private_key='<приватный ключ роутера>'
   uci add_list network.wg0.addresses='10.78.0.3/32'
   uci set network.wg0.mtu='1360'
   uci set network.wgvps=wireguard_wg0
   uci set network.wgvps.public_key='<публичный ключ WireGuard VPS>'
   uci set network.wgvps.endpoint_host='127.0.0.1'
   uci set network.wgvps.endpoint_port='51821'
   uci add_list network.wgvps.allowed_ips='0.0.0.0/0'
   uci set network.wgvps.route_allowed_ips='1'
   uci set network.wgvps.persistent_keepalive='25'
   ```
4. Маршруты. `route_allowed_ips` ставит маршрут по умолчанию в `wg0`. Сам транспорт
   (`vps-client` → VPS) должен идти по WAN, иначе петля: отдельный маршрут до VPS и более
   высокая метрика у WAN.
   ```sh
   uci set network.wan.metric='20'
   uci set network.vps_direct=route
   uci set network.vps_direct.interface='wan'
   uci set network.vps_direct.target='203.0.113.30/32'
   uci set network.vps_direct.gateway='192.168.3.1'
   ```
5. `wg0` в зону `wan` (NAT для LAN-клиентов):
   ```sh
   uci add_list firewall.@zone[1].network='wg0'
   uci commit network; uci commit firewall
   /etc/init.d/network restart; /etc/init.d/firewall reload
   ```
6. Управление роутером по WAN (если нужно), только из частных сетей:
   ```sh
   uci add firewall rule
   uci set firewall.@rule[-1].name='Allow-SSH-WAN'
   uci set firewall.@rule[-1].src='wan'
   uci set firewall.@rule[-1].proto='tcp'
   uci set firewall.@rule[-1].dest_port='22'
   uci set firewall.@rule[-1].src_ip='192.168.0.0/16'
   uci set firewall.@rule[-1].target='ACCEPT'
   uci commit firewall; /etc/init.d/firewall reload
   ```
   Ответы на SSH уходят по подсети WAN (она подключена напрямую), а не в туннель.

## 4. Проверка

На роутере:

```sh
wg show wg0                    # latest handshake, transfer
ip route get 1.1.1.1           # dev wg0
ip route get 203.0.113.30      # via 192.168.3.1 dev <wan>
ping -c3 10.78.0.1             # VPS внутри туннеля
wget -qO- http://ifconfig.me/ip   # внешний адрес = белый IP VPS
```

С LAN-клиента: `traceroute 1.1.1.1` → роутер → `10.78.0.1` → дальше сеть VPS.

## 5. Откат

```sh
cp /etc/config/network.bak /etc/config/network; cp /etc/config/firewall.bak /etc/config/firewall
/etc/init.d/network restart; /etc/init.d/firewall reload
start-stop-daemon -K -p /tmp/hp/vps-client.pid
```

## Скорость (2026-09-26)

LAN-клиент (ноутбук) → Xiaomi Mi Router 4C → VPS, iperf 2, TCP, одно соединение:

| Путь | VPS → клиент | клиент → VPS |
|---|---|---|
| через WAN роутера, без туннеля | 68,0 Мбит/с | 88,4 Мбит/с |
| через `wg0` → `vps-client` → 10 дыр → `vps-server` → WireGuard | 7,3 Мбит/с | 9,5 Мбит/с |

Узкое место — процессор роутера (одно ядро MT7628, 580 МГц): во время замера idle 0%,
`vps-client` ~45%, потоки WireGuard в ядре ~45%, softirq ~15%. Каждый пакет проходит и шифрование
WireGuard в ядре, и обработку в пространстве пользователя (`vps-client`: XOR 64 байт, protobuf,
буфер порядка). Для десятков Мбит/с нужен роутер мощнее (MT7621 и новее) или меньше работы на
пакет.
