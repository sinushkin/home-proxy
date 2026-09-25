<#
.SYNOPSIS
  Убирает то, что поставил install.ps1: службу, WireGuard-туннель, NAT (NetNat или ICS), правило
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

$icsKey = 'HKLM:\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters'

function Get-IcsConnections {
    $share = New-Object -ComObject HNetCfg.HNetShare
    foreach ($connection in @($share.EnumEveryConnection)) {
        [pscustomobject]@{
            Name   = $share.NetConnectionProps.Invoke($connection).Name
            Config = $share.INetSharingConfigurationForINetConnection.Invoke($connection)
        }
    }
}

# Общий доступ к интернету (ICS) как NAT, когда New-NetNat недоступен. Адрес ICS
# (ScopeAddress) задаём равным адресу туннеля, подсеть у ICS всегда /24.
function Enable-HomeproxyIcs([string]$publicName, [string]$privateName, [string]$address) {
    Set-ItemProperty $icsKey -Name ScopeAddress -Value $address
    Set-ItemProperty $icsKey -Name ScopeAddressBackup -Value $address
    $connections = @(Get-IcsConnections)
    foreach ($c in $connections | Where-Object { $_.Name -in $publicName, $privateName -and $_.Config.SharingEnabled }) {
        $c.Config.DisableSharing()
    }
    $public = $connections | Where-Object Name -eq $publicName
    $private = $connections | Where-Object Name -eq $privateName
    if (-not $public -or -not $private) { throw "ICS: не нашёл подключения '$publicName' и '$privateName'" }
    try {
        $public.Config.EnableSharing(0)
        $private.Config.EnableSharing(1)
    } catch {
        throw "ICS не включился ($($_.Exception.Message)). Возможно, общий доступ уже настроен на другом подключении."
    }
}

function Disable-HomeproxyIcs([string]$privateName) {
    $connections = @(Get-IcsConnections)
    $private = $connections | Where-Object { $_.Name -eq $privateName -and $_.Config.SharingEnabled }
    if (-not $private) { return }
    foreach ($c in $connections | Where-Object { $_.Config.SharingEnabled }) { $c.Config.DisableSharing() }
    Set-ItemProperty $icsKey -Name ScopeAddress -Value '192.168.137.1' -ErrorAction SilentlyContinue
    Set-ItemProperty $icsKey -Name ScopeAddressBackup -Value '192.168.137.1' -ErrorAction SilentlyContinue
}

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
Disable-HomeproxyIcs $tunnel
if (Get-Service "WireGuardTunnel`$$tunnel" -ErrorAction SilentlyContinue) {
    & $wireguard /uninstalltunnelservice $tunnel
}
Get-NetNat -Name homeproxy -ErrorAction SilentlyContinue | Remove-NetNat -Confirm:$false
Get-NetFirewallRule -DisplayName $serviceName -ErrorAction SilentlyContinue | Remove-NetFirewallRule
if (-not $KeepFiles -and (Test-Path $InstallDir)) { Remove-Item $InstallDir -Recurse -Force }
Write-Host 'Удалено.'
