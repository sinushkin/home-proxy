# luci-app-homeproxy — строка подключения трея в LuCI

Маленькая страница LuCI (JS, без Lua и без сборки) для роутера с `hp-router`: **Службы → Home
Proxy**. Показывает строку подключения трея `homeproxy-control://<адрес LAN>:47001/<ключ>`,
копирует её в буфер (на http — через выделение), выпускает новую («Новая строка подключения»:
новый ключ, прежние строки перестают работать сразу). Ключ хранит сам `hp-router`
(`/etc/hp-router/control.key`, 600); страница только вызывает его.

| Файл | Что |
|---|---|
| `htdocs/luci-static/resources/view/homeproxy/control.js` | сама страница |
| `root/usr/share/luci/menu.d/luci-app-homeproxy.json` | пункт меню «Службы → Home Proxy» |
| `root/usr/share/rpcd/acl.d/luci-app-homeproxy.json` | права: чтение — только `hp-router --config /etc/hp-router/router.env --connection-string`, запись — только `--new-connection-string` |
| `root/etc/init.d/hp-router` | служба procd: `/usr/bin/hp-router --config /etc/hp-router/router.env` |
| `Makefile` | сборка ipk в фиде LuCI (OpenWrt SDK) |
| `install.sh` | установка без SDK: файлы по ssh, сброс кэша LuCI, `rpcd reload` |

Установка без SDK:

```sh
OpenWRT/luci-app-homeproxy/install.sh root@192.168.1.1
```

Нужно: `hp-router` в `/usr/bin`, настройки в `/etc/hp-router/router.env` с
`CONTROL_ADDR=<адрес LAN>:47001` (иначе страница покажет «управление выключено»). Порядок и
маршруты — [`../Tun.md`](../Tun.md).

Проверено на OpenWrt 23.05 (Xiaomi 4C): права — сессией rpcd с раскрытым ACL (показать можно;
сменить ключ без права записи, чужой конфиг или чужую команду — нельзя), команды страницы — с
трея на ПК (новая строка принята, старая отвергнута). Страница в браузере не открывалась.
