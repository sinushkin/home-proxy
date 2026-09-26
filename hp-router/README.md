# `hp-router` — роутер OpenWrt: дом через VPS и телефоны

Один бинарник (~2 МБ на mipsel) с двумя ролями:

1. **Шлюз дома.** Поднимает TUN `hp0` и 10 дыр к [`vps-server`](../vps-server/README.md) с белым
   IP (как [`vps-client`](../vps-client/README.md)): весь трафик LAN и Wi-Fi уходит к VPS как
   есть, TCP — с номером в потоке, VPS возвращает порядок.
2. **Пир для телефонов.** Для каждого телефона (`PHONE_<n>_*`) — свой набор из 10 P2P-дыр
   (STUN + MQTT, как у [`hp-server`](../hp-server/README.md)). Пакеты телефона роутер не
   разбирает и порядок им не восстанавливает: перекладывает в дыры к VPS как
   `WrappedData { client_id: n }` с номерами телефона, ответы VPS для `n` — обратно телефону.
   TUN и стек ядра роутера пакеты телефонов не проходят.

```
LAN/Wi-Fi -> hp0 -> hp-router ==10 дыр==> vps-server -> hp0 -> NAT -> интернет
телефон n ==10 P2P-дыр==> hp-router ==те же дыры, WrappedData{n}==> vps-server
```

Раньше на этом месте был `router`: релей телефоны ↔ домашний ПК для WireGuard (роутер
оборачивал датаграммы WireGuard, ПК их расшифровывал). От WireGuard отказались — на MT7628 он
давал 10–12 Мбит/с против 21–32 без него (`../OpenWRT/Tun.md`, `../Performance.md`).

## Запуск

```sh
hp-router [--config router.env]     # без --config: router.env рядом с бинарником
```

Настройки — [`router.env.example`](router.env.example): VPS (`VPS_SERVER`, `VPS_MY_ID`,
`VPS_PEER_ID`), адрес в туннеле (`TUN_ADDR`), рандеву для телефонов (`STUN_ADDR`, `MQTT_ADDR`,
`MQTT_CA`) и пары GUID телефонов. Телефонов может не быть — тогда только шлюз. Нужны root,
`kmod-tun`, маршрут по умолчанию в `hp0` только для LAN (сам роутер ходит через WAN) и
перезагрузка firewall после каждого запуска (flowtable) — по шагам в
[`../OpenWRT/Tun.md`](../OpenWRT/Tun.md).

В логе раз в 30 с (при изменении): дыры к VPS, пакеты из TUN и в TUN, по телефонам — число живых
дыр, пересылка к VPS и к телефонам с потерями.

## Проверено

Xiaomi Mi Router 4C (OpenWrt 23.05.4) ↔ VPS: дом ~25 Мбит/с в каждую сторону (TCP, iperf),
телефон на LTE через роутер — 14/7 Мбит/с (speedtest), дыры 10/10 на обоих участках.
Сквозной тест «телефон за роутером → VPS → обратно» без сети — `cargo test -p hp-tun`
(`phone_packets_behind_the_router_reach_the_vps_tun_and_come_back`).

Сборка под OpenWrt — [`../OpenWRT/README.md`](../OpenWRT/README.md).
