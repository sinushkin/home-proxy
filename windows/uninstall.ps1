<#
.SYNOPSIS
  Убирает то, что поставил install.ps1: службу, WireGuard-туннель, NAT, правило
  брандмауэра, маршруты STUN и (если не указан -KeepFiles) каталог установки.
  Пересылка (Forwarding) на интерфейсах и сам WireGuard остаются.
#>
#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:ProgramData 'homeproxy'),
    [switch]$KeepFiles
)
$ErrorActionPreference = 'Stop'
$wireguard = Join-Path $env:ProgramFiles 'WireGuard\wireguard.exe'
$tunnel = 'wghp'
$serviceName = 'homeproxy-server'

function Remove-HomeproxyService {
    $service = Get-Service $serviceName -ErrorAction SilentlyContinue
    if (-not $service) { return }
    if ($service.Status -ne 'Stopped') { Stop-Service $serviceName -Force }
    & sc.exe delete $serviceName | Out-Null
}

if (Test-Path (Join-Path $InstallDir 'stun-bypass.ps1')) {
    & (Join-Path $InstallDir 'stun-bypass.ps1') -EnvFile (Join-Path $InstallDir 'server.env') -Remove
}
Remove-HomeproxyService
if (Get-Service "WireGuardTunnel`$$tunnel" -ErrorAction SilentlyContinue) {
    & $wireguard /uninstalltunnelservice $tunnel
}
Get-NetNat -Name homeproxy -ErrorAction SilentlyContinue | Remove-NetNat -Confirm:$false
Get-NetFirewallRule -DisplayName $serviceName -ErrorAction SilentlyContinue | Remove-NetFirewallRule
if (-not $KeepFiles -and (Test-Path $InstallDir)) { Remove-Item $InstallDir -Recurse -Force }
Write-Host 'Удалено.'
