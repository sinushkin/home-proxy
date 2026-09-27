#!/bin/sh
# Ставит luci-app-homeproxy на роутер без SDK: файлы по ssh, сброс кэша LuCI, перечитать права rpcd.
#   ./install.sh root@192.168.1.1
set -eu
router=${1:?использование: $0 root@<адрес роутера>}
cd "$(dirname "$0")"
tar -C htdocs -cf - . | ssh "$router" 'tar -C /www -xf -'
tar -C root -cf - . | ssh "$router" 'tar -C / -xf -'
ssh "$router" 'rm -rf /tmp/luci-indexcache* /tmp/luci-modulecache; /etc/init.d/rpcd reload'
echo "готово: LuCI → Службы → Home Proxy"
