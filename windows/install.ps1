<#
.SYNOPSIS
  Ставит на Windows службу home-proxy: WireGuard-туннель wghp, NAT для его подсети,
  прокси-службу homeproxy-server (дыры -> WireGuard) и обход VPN для STUN.

.DESCRIPTION
  В каталоге -SourceDir должны лежать: hp-server.exe, server.env (настройки службы),
  wghp.conf (конфиг WireGuard, его делает wireguard/gen.sh) и CA-сертификат брокера,
  на который указывает MQTT_CA в server.env (относительный путь — от server.env).
  Всё копируется в -InstallDir и закрывается правами только для SYSTEM и администраторов
  (там приватный ключ). PostUp/PostDown из wghp.conf отбрасываются: WireGuard для Windows
  скрипты не запускает, NAT настраивает этот скрипт (New-NetNat).

  NAT: -Nat Auto (по умолчанию) берёт New-NetNat, а если он недоступен (на некоторых
  системах нет WMI-провайдера NetNat), общий доступ к интернету (ICS: подсеть туннеля
  должна быть /24). -Nat None (или -SkipNat) — NAT настроен иначе.

  Требования: права администратора, WireGuard для Windows (wireguard.exe).
  Скрипт можно запускать повторно: старая установка заменяется.
#>
#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [string]$SourceDir = $PSScriptRoot,
    [string]$InstallDir = (Join-Path $env:ProgramData 'homeproxy'),
    [ValidateSet('Auto', 'NetNat', 'Ics', 'None')][string]$Nat = 'Auto',
    [switch]$SkipNat,
    [switch]$SkipStunBypass
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

# 1. Проверки до любых изменений.
foreach ($name in 'hp-server.exe', 'server.env', 'wghp.conf') {
    if (-not (Test-Path (Join-Path $SourceDir $name))) { throw "Нет $name в $SourceDir" }
}
if (-not (Test-Path $wireguard)) {
    throw "Нет WireGuard для Windows ($wireguard). Установите: winget install WireGuard.WireGuard (или MSI с wireguard.com)"
}
if ($SkipNat) { $Nat = 'None' }
if ($Nat -in 'Auto', 'NetNat') {
    $netnat = $true
    try { Get-NetNat -ErrorAction Stop | Out-Null } catch { $netnat = $false }
    if ($netnat) { $Nat = 'NetNat' }
    elseif ($Nat -eq 'Auto') {
        $Nat = 'Ics'
        Write-Warning 'New-NetNat недоступен (нет WMI-провайдера NetNat), использую общий доступ к интернету (ICS)'
    }
    else { throw 'NAT недоступен (Get-NetNat не работает). Используйте -Nat Ics или -Nat None.' }
}
$envText = Get-Content (Join-Path $SourceDir 'server.env') -Raw
$caLine = [regex]::Match($envText, '(?m)^\s*(?:export\s+)?MQTT_CA\s*=\s*(.+?)\s*$')
if (-not $caLine.Success) { throw 'В server.env нет MQTT_CA' }
$ca = $caLine.Groups[1].Value.Trim('"', "'")
if (-not [IO.Path]::IsPathRooted($ca) -and -not (Test-Path (Join-Path $SourceDir $ca))) {
    throw "Нет CA-сертификата $ca рядом с server.env (MQTT_CA)"
}

# 2. Убираем прежнюю установку.
Remove-HomeproxyService
Disable-HomeproxyIcs $tunnel
if (Get-Service "WireGuardTunnel`$$tunnel" -ErrorAction SilentlyContinue) {
    & $wireguard /uninstalltunnelservice $tunnel
}
Get-NetNat -Name homeproxy -ErrorAction SilentlyContinue | Remove-NetNat -Confirm:$false

# 3. Файлы и права.
New-Item -ItemType Directory -Force $InstallDir | Out-Null
& icacls $InstallDir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
foreach ($name in 'hp-server.exe', 'server.env', 'stun-bypass.ps1') {
    $from = Join-Path $SourceDir $name
    if (Test-Path $from) { Copy-Item $from $InstallDir -Force }
}
if (-not [IO.Path]::IsPathRooted($ca)) {
    $to = Join-Path $InstallDir $ca
    New-Item -ItemType Directory -Force (Split-Path $to) | Out-Null
    Copy-Item (Join-Path $SourceDir $ca) $to -Force
}
$conf = Get-Content (Join-Path $SourceDir 'wghp.conf') |
    Where-Object { $_ -notmatch '^\s*(PostUp|PostDown|PreUp|PreDown)\s*=' }
$confPath = Join-Path $InstallDir "$tunnel.conf"
Set-Content -Path $confPath -Value $conf -Encoding ascii

# 4. WireGuard-туннель.
& $wireguard /installtunnelservice $confPath
$adapter = $null
for ($i = 0; $i -lt 30 -and -not $adapter; $i++) {
    $adapter = Get-NetAdapter -Name $tunnel -ErrorAction SilentlyContinue
    if (-not $adapter) { Start-Sleep -Milliseconds 500 }
}
if (-not $adapter) { throw "Адаптер $tunnel не появился: проверьте конфиг $confPath и журнал службы WireGuardTunnel`$$tunnel" }

# 5. Пересылка и NAT для подсети туннеля.
if ($Nat -ne 'None') {
    $address = [regex]::Match(($conf -join "`n"), '(?m)^\s*Address\s*=\s*(\d+\.\d+\.\d+\.\d+)/(\d+)')
    if (-not $address.Success) { throw 'В wghp.conf нет Address = ip/маска' }
    $prefixLength = [int]$address.Groups[2].Value
    $bytes = [Net.IPAddress]::Parse($address.Groups[1].Value).GetAddressBytes()
    $masked = foreach ($i in 0..3) {
        $bits = [Math]::Max(0, [Math]::Min(8, $prefixLength - 8 * $i))
        $bytes[$i] -band ((0xFF -shl (8 - $bits)) -band 0xFF)
    }
    $network = [Net.IPAddress]::new([byte[]]$masked)
    $exit = Get-NetRoute -DestinationPrefix '0.0.0.0/0' -AddressFamily IPv4 |
        Where-Object { $_.ifIndex -ne $adapter.ifIndex -and $_.NextHop -ne '0.0.0.0' } |
        Sort-Object RouteMetric | Select-Object -First 1
    if ($Nat -eq 'NetNat') {
        foreach ($index in $adapter.ifIndex, $exit.ifIndex) {
            Set-NetIPInterface -InterfaceIndex $index -AddressFamily IPv4 -Forwarding Enabled
        }
        New-NetNat -Name homeproxy -InternalIPInterfaceAddressPrefix "$network/$prefixLength" | Out-Null
        Write-Host "NAT (NetNat): $network/$prefixLength наружу через интерфейс $($exit.ifIndex)"
    }
    else {
        if ($prefixLength -ne 24) { throw "ICS работает только с подсетью /24, а в wghp.conf /$prefixLength" }
        $publicName = (Get-NetAdapter -InterfaceIndex $exit.ifIndex).Name
        Enable-HomeproxyIcs $publicName $tunnel $address.Groups[1].Value
        Write-Host "NAT (ICS): '$tunnel' -> '$publicName', адрес $($address.Groups[1].Value)"
    }
}

# 6. Брандмауэр: входящий UDP для hp-server.exe (ответы пира на дыры).
Get-NetFirewallRule -DisplayName $serviceName -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName $serviceName -Direction Inbound -Action Allow -Protocol UDP -Profile Any `
    -Program (Join-Path $InstallDir 'hp-server.exe') | Out-Null

# 7. STUN мимо VPN.
if (-not $SkipStunBypass) {
    & (Join-Path $InstallDir 'stun-bypass.ps1') -EnvFile (Join-Path $InstallDir 'server.env')
}

# 8. Служба: после WireGuard-туннеля, автозапуск, перезапуск при сбое.
& (Join-Path $InstallDir 'hp-server.exe') install --config (Join-Path $InstallDir 'server.env')
& sc.exe config $serviceName depend= "WireGuardTunnel`$$tunnel" | Out-Null
Start-Service $serviceName
Write-Host "Готово. Логи: $InstallDir\server.log (LOG_FILE в server.env), статус: Get-Service $serviceName"
