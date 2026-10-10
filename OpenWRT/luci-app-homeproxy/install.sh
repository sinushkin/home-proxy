#!/bin/sh
# Ставит luci-app-homeproxy на роутер без SDK: файлы по ssh, сброс кэша LuCI, перечитать права rpcd.
#   ./install.sh root@192.168.1.1
set -eu
router=${1:?использование: $0 root@<адрес роутера>}
cd "$(dirname "$0")"
tar -C htdocs -cf - . | ssh "$router" 'tar -C /www -xf -'
# Служба procd hp-router ставится, только если на роутере есть hp-router (на роутере с одним
# vps-client её быть не должно).
if ssh "$router" 'test -x /usr/bin/hp-router'; then
	tar -C root -cf - . | ssh "$router" 'tar -C / -xf -'
else
	tar -C root --exclude=./etc/init.d -cf - . | ssh "$router" 'tar -C / -xf -'
fi
ssh "$router" 'rm -rf /tmp/luci-indexcache* /tmp/luci-modulecache; /etc/init.d/rpcd reload'
echo "готово: LuCI → Службы → Home Proxy"
