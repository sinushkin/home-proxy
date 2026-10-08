# Hook vps-client for Windows: tunnel is up, routes are set. Variables: DNS (comma separated),
# TUN_DEV, TUN_ADDR, VPS_IP, UPLINK_DEV, ... (see hp_tun::routes::hook_env).
# Sets DNS on the tunnel interface only (the interface metric of the tunnel is 1, so the system
# asks these servers first); other adapters are not touched. Messages are ASCII on purpose:
# the console encoding differs between Windows installations.
if (-not $env:DNS) { 'on-tun-up: DNS is empty, nothing to do'; exit 0 }
Set-DnsClientServerAddress -InterfaceAlias $env:TUN_DEV -ServerAddresses ($env:DNS -split ',')
"on-tun-up: Windows, $env:TUN_DEV -> $env:DNS"
exit 0
