#!/bin/sh
# Снимает набор адресов мимо туннеля.
PATH=/usr/sbin:/usr/bin:/sbin:/bin
nft delete table inet hp_direct 2>/dev/null
echo "direct: снято"
