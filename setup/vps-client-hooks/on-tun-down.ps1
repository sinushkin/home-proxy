# Hook vps-client for Windows: tunnel is going down (the interface still exists).
Set-DnsClientServerAddress -InterfaceAlias $env:TUN_DEV -ResetServerAddresses -ErrorAction SilentlyContinue
"on-tun-down: Windows, DNS of $env:TUN_DEV reset"
exit 0
