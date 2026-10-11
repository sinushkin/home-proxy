# DNS роутера o1: только runtime

Документ описывает переход DNS на роутере `o1` (OpenWrt, `hp-router`) с записи в
`/etc/config/dhcp` на runtime-файл в `/tmp`, и изменения в `setup/vps-client-hooks/`.

## Зачем

- До туннеля DNS должен быть от провайдера (DHCP на WAN), а не статический `8.8.8.8`.
- Хук `on-tun-up.sh` при каждом подъёме туннеля писал DNS VPS через `uci commit` в
  `/etc/config/dhcp` (флеш). `on-tun-down.sh` это откатывал только при штатной остановке
  службы. После перезагрузки без туннеля в конфиге оставался DNS VPS, и до туннеля работал
  он, а не DNS провайдера.
- Решение: DNS туннеля живёт только в `/tmp` (очищается при перезагрузке), UCI не трогаем.

## Как работает dnsmasq на o1

- Сгенерированный конфиг `/var/etc/dnsmasq.conf.cfg01411c` содержит `conf-dir=/tmp/dnsmasq.d`:
  все файлы из этого каталога читаются при старте dnsmasq.
- Без `no-resolv` dnsmasq использует и `server=`, и resolv от WAN
  (`/tmp/resolv.conf.d/resolv.conf.auto`, его пишет netifd из DHCP). Поэтому при
  включённом туннеле runtime-файл обязан содержать `no-resolv`, иначе запросы
  уйдут и к провайдеру.
- dnsmasq версии 2.86 (`dnsmasq --version`).

## Изменения на o1

### 1. Хуки `/etc/vps-client/` (копии на роутере)

Заменены на версии из `setup/vps-client-hooks/` (совпадают побайтно, проверено `md5sum`).

- `on-tun-up.sh`: вместо `uci add_list` + `uci commit` пишет `/tmp/dnsmasq.d/hp-tun.conf`:
  ```
  no-resolv
  server=<адрес из DNS>
  ...
  ```
  и перезапускает dnsmasq. Файл создаётся через `.new` и `mv`, чтобы не читать обрезанный.
- `on-tun-down.sh`: удаляет `/tmp/dnsmasq.d/hp-tun.conf` и перезапускает dnsmasq. Без
  файла dnsmasq читает resolv от WAN.
- Обвязка `udp-direct/` и `direct/` (openwrt-k-router) не менялась.

### 2. UCI `/etc/config/dhcp`

Удалены `dhcp.@dnsmasq[0].server` и `dhcp.@dnsmasq[0].noresolv`, `uci commit dhcp`.
После этого в UCI нет DNS VPS, и после перезагрузки с пустым `/tmp` без туннеля dnsmasq
берёт DNS провайдера.

### 3. Ранее (до этого документа)

- `network.wan.dns` (статический `8.8.8.8`) удалён, `network.wan.peerdns='1'`: DNS от
  DHCP провайдера попадает в `resolv.conf.auto` (`192.168.17.1`).
- Эти изменения были сделаны раньше, см. резервную копию ниже.

### 4. Внештатное завершение `hp-router` (падение, `kill -9`, OOM)

При таком завершении `on-tun-down.sh` не запускается, и runtime-файл остался бы, то есть
DNS VPS без туннеля. Поэтому init-скрипт запускает службу через обёртку
`/usr/libexec/hp-router-run` (в репозитории `OpenWRT/luci-app-homeproxy/root/usr/libexec/`):
- запускает `hp-router` дочерним процессом, пересылает ему `SIGTERM` от procd;
- после завершения службы по любой причине, если `/tmp/dnsmasq.d/hp-tun.conf` остался,
  удаляет его и перезапускает dnsmasq (DNS провайдера);
- код возврата сохраняет, procd респавнит службу (`respawn 3600 5 0`), и хук подъёма снова
  записывает DNS VPS.

Изменены `/etc/init.d/hp-router` (строка `procd_set_param command`) и `install.sh`
(обёртка не ставится на роутер без `hp-router`). Бэкап прежнего init на `o1`:
`/root/hp-router-run-backup-20261011-122759/`.

Питание выключено из розетки: `/tmp` пуст, в `/etc/config` DNS VPS нет, поэтому после
включения работает DNS провайдера, пока туннель не поднимется и хук не запишет DNS VPS.

Проверка на `o1`: `kill -9` `hp-router` — через 2 с runtime-файл снят, dnsmasq на
`192.168.17.1`; через ~10 с служба поднялась сама, `hp0` на месте, DNS VPS записан.

### 5. Отчёт `/root/dns-info.sh`

Только чтение. Показывает пять блоков: DNS провайдера (WAN), DNS через туннель (VPS),
что реально использует dnsmasq (режим и апстримы), проверку резолва, и есть ли в UCI
остатки старого хука. Запуск: `ssh o1 /root/dns-info.sh`.

## Изменения в репозитории

- `setup/vps-client-hooks/on-tun-up.sh` и `on-tun-down.sh` — те же версии, что на `o1`.
  Ветка Debian/Ubuntu (`resolvectl`) по логике не менялась; правки только в кавычках
  (`"~."`, `tr "," " "`) и в комментарии.
- `setup/vps-client.sh` не менялся: он копирует хуки из `setup/vps-client-hooks/` при
  установке, так что новые установки получат runtime-версию сразу.
- Коммит не делался.

## Что проверено

- На `o1` с поднятым туннелем: `/tmp/dnsmasq.d/hp-tun.conf` содержит три сервера VPS,
  в UCI `server`/`noresolv` нет, резолв `openwrt.org` через dnsmasq работает.
- Остановка `hp-router`: хук `on-tun-down` снимает runtime-файл, dnsmasq перезапускается,
  `resolv.conf.auto` содержит `192.168.17.1`. Запуск `hp-router` возвращает DNS VPS.
- Перезагрузка `o1` (~12:06, инициатор не известен, не через эти тесты): по наблюдению
  пользователя после загрузки использовался DNS провайдера до подъёма туннеля.

Не проверено до конца: какой именно сервер отвечает в режиме без туннеля (через
`SIGUSR1` статистику dnsmasq не снял: SSH оборвался). Дополнительной перезагрузкой
с новыми хуками не проверялось.

## Откат

Резервные копии на `o1`:
- `/root/runtime-dns-backup-20261011-114641/` — прежние `on-tun-up.sh`, `on-tun-down.sh` и
  `dhcp` (с `server=` и `noresolv`).
- `/root/dns-backup-20261011-112618/` — `network`, `dhcp`, `resolv.conf.auto` до удаления
  статического DNS на WAN.

Откат хуков: `cp -p /root/runtime-dns-backup-20261011-114641/on-tun-*.sh /etc/vps-client/`,
затем `/etc/init.d/hp-router restart`.

Откат обёртки: `cp -p /root/hp-router-run-backup-20261011-122759/hp-router /etc/init.d/hp-router`,
`rm /usr/libexec/hp-router-run`, `/etc/init.d/hp-router restart`.

Откат UCI: `cp -p /root/runtime-dns-backup-20261011-114641/dhcp /etc/config/dhcp`,
`/etc/init.d/dnsmasq restart`. Возвращать `server=` в UCI — только если действительно
нужен DNS VPS после перезагрузки без туннеля: тогда снова будет потеряно поведение из
раздела «Зачем».

Откат `network.wan`: `uci set network.wan.peerdns='0'`, `uci set network.wan.dns='8.8.8.8'`,
`uci commit network`, `ifdown wan; ifup wan`.

## Открытые вопросы

- Адрес `77.8.8.8` в списке DNS VPS: сервер его присылает, в репозитории не проверяли,
  что это ожидаемо.
- Нужна ли проверка SIGUSR1 статистики dnsmasq в режиме без туннеля (подтвердить, что
  запросы идут к `192.168.17.1`).
