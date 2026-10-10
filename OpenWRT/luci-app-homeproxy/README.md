# luci-app-homeproxy — строка подключения трея в LuCI

Маленькая страница LuCI (JS, без Lua и без сборки) для роутера с `hp-router` или `vps-client`
(страница сама находит, какая служба стоит): **Службы → Home Proxy**. Показывает строку подключения
трея `homeproxy-control://<адрес LAN>:47001/<ключ>` — трей и `hpctl` на ПК подключаются по ней
напрямую, ssh-туннель не нужен,
копирует её в буфер (на http — через выделение), выпускает новую («Новая строка подключения»:
новый ключ, прежние строки перестают работать сразу). Ключ хранит сам `hp-router`
(`/etc/hp-router/control.key` или `/etc/vps-client/control.key`, 600); страница только вызывает
`<служба> --config <настройки> --connection-string`.

| Файл | Что |
|---|---|
| `htdocs/luci-static/resources/view/homeproxy/control.js` | сама страница |
| `root/usr/share/luci/menu.d/luci-app-homeproxy.json` | пункт меню «Службы → Home Proxy» |
| `root/usr/share/rpcd/acl.d/luci-app-homeproxy.json` | права: чтение — только `--connection-string`, запись — только `--new-connection-string`, для каждой из двух служб с её файлом настроек (`/etc/hp-router/router.env`, `/etc/vps-client/vps-client.conf`) |
| `test/find.test.js` | проверка выбора службы на заглушках: `node test/find.test.js` |
| `root/etc/init.d/hp-router` | служба procd `hp-router` (`install.sh` кладёт её, только если на роутере есть `/usr/bin/hp-router`) |
| `Makefile` | сборка ipk в фиде LuCI (OpenWrt SDK) |
| `install.sh` | установка без SDK: файлы по ssh, сброс кэша LuCI, `rpcd reload` |

Установка без SDK:

```sh
OpenWRT/luci-app-homeproxy/install.sh root@192.168.1.1
```

Нужно: `hp-router` в `/usr/bin` и в `/etc/hp-router/router.env` строка
`CONTROL_ADDR=<адрес LAN>:47001`, либо `vps-client` в `/usr/bin` и `CONTROL_ADDR=<адрес LAN>:47001` в
`/etc/vps-client/vps-client.conf` (иначе страница покажет «управление выключено»); init-скрипт
`vps-client` должен передавать `CONTROL_ADDR` и `CONTROL_KEY_FILE` процессу (`setup/vps-client.sh`
делает это сам). Адрес — именно LAN роутера, не `0.0.0.0`. Порядок и
маршруты — [`../Tun.md`](../Tun.md).

Проверено на OpenWrt 22.03 (MT7621, `vps-client`): та же команда через rpcd с выданным ACL —
строка приходит, `--new-connection-string` и чужие аргументы отказ (код 6); страница ищет службу
по порядку (проверка `test/find.test.js`). Проверено на OpenWrt 23.05 (Xiaomi 4C, `hp-router`): права — сессией rpcd с раскрытым ACL (показать можно;
сменить ключ без права записи, чужой конфиг или чужую команду — нельзя), команды страницы — с
трея на ПК (новая строка принята, старая отвергнута). Страница в браузере не открывалась.
