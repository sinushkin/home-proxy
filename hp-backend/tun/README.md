# `hp-tun` — TUN-интерфейс

IP-пакеты напрямую из ядра и в ядро — основа режима без WireGuard (пакеты LAN идут по дырам
как есть, маскировка — XOR заголовка своим вектором на каждую дыру).

Своя лёгкая обёртка над `/dev/net/tun` (`libc` + `tokio::io::unix::AsyncFd`) вместо крейтов
`tun` / `tun-rs` / `tokio-tun`: на Linux это один `ioctl(TUNSETIFF)`, на Android TUN создаёт
система, а лишние зависимости пришлось бы проверять на MIPS32 (64-битных атомиков здесь нет).

| Платформа | Как | Проверено |
|---|---|---|
| Linux | `Tun::create` (нужен `CAP_NET_ADMIN`) | тест с ядром в `unshare -rn` |
| OpenWrt (mipsel) | `Tun::create`, пакет `kmod-tun` (`opkg install kmod-tun`) | Xiaomi 4C: `hp-tun-check`, ping 4/4 |
| WSL2 | `Tun::create` (`/dev/net/tun` есть в ядре WSL2) | Alpine в WSL2: `hp-tun-check`, ping 4/4 |
| Android | `Tun::from_fd` с дескриптором `VpnService.Builder.establish()` | сборка armv7; запуск — вместе с приложением |

API: `Tun::create(&TunConfig { name, address: Some((ip, prefix)), mtu, up })`, `Tun::from_fd(fd)`,
`recv(&mut buf)` / `send(&packet)` — по одному IP-пакету за вызов. `packet::inspect` — версия,
протокол, адреса, порты, `flow_hash()` (хэш соединения: пригодится, чтобы ждать порядка только у
TCP и только внутри своего потока), `packet::icmp_echo_reply`, `packet::ipv4_udp`.

Тесты: `cargo test -p hp-tun` (разбор пакетов, `from_fd` на паре сокетов); с настоящим TUN —
`unshare -rn cargo test -p hp-tun --test kernel` (без прав тест пропускается).

Живая проверка: `hp-tun-check <имя> <адрес/префикс> [секунд]` поднимает интерфейс и отвечает на
ping адресов подсети: `hp-tun-check hp0 10.79.0.1/24 30`, затем `ping 10.79.0.2`.
