<#
.SYNOPSIS
  Ставит на Windows службу home-proxy: WireGuard-туннель wghp, NAT для его подсети,
  прокси-службу homeproxy-server (дыры -> WireGuard) и обход VPN для STUN.

.DESCRIPTION
  В каталоге -SourceDir должны лежать: server.exe, server.env (настройки службы),
  wghp.conf (конфиг WireGuard, его делает wireguard/gen.sh) и CA-сертификат брокера,
  на который указывает MQTT_CA в server.env (относительный путь — от server.env).
  Всё копируется в -InstallDir и закрывается правами только для SYSTEM и администраторов
  (там приватный ключ). PostUp/PostDown из wghp.conf отбрасываются: WireGuard для Windows
  скрипты не запускает, NAT настраивает этот скрипт (New-NetNat).

  Требования: права администратора, WireGuard для Windows (wireguard.exe), NetNat.
  Скрипт можно запускать повторно: старая установка заменяется.
#>
#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [string]$SourceDir = $PSScriptRoot,
    [string]$InstallDir = (Join-Path $env:ProgramData 'homeproxy'),
    [switch]$SkipNat,
    [switch]$SkipStunBypass
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

# 1. Проверки до любых изменений.
foreach ($name in 'server.exe', 'server.env', 'wghp.conf') {
    if (-not (Test-Path (Join-Path $SourceDir $name))) { throw "Нет $name в $SourceDir" }
}
if (-not (Test-Path $wireguard)) {
    throw "Нет WireGuard для Windows ($wireguard). Установите: winget install WireGuard.WireGuard (или MSI с wireguard.com)"
}
if (-not $SkipNat) {
    try { Get-NetNat -ErrorAction Stop | Out-Null }
    catch {
        throw 'NAT недоступен (Get-NetNat не работает). Обычно он есть в Windows 10/11 Pro; если нет, включите компонент Hyper-V. Либо -SkipNat, если NAT для подсети WireGuard настроен иначе.'
    }
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
if (Get-Service "WireGuardTunnel`$$tunnel" -ErrorAction SilentlyContinue) {
    & $wireguard /uninstalltunnelservice $tunnel
}
Get-NetNat -Name homeproxy -ErrorAction SilentlyContinue | Remove-NetNat -Confirm:$false

# 3. Файлы и права.
New-Item -ItemType Directory -Force $InstallDir | Out-Null
& icacls $InstallDir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
foreach ($name in 'server.exe', 'server.env', 'stun-bypass.ps1') {
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
if (-not $SkipNat) {
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
    foreach ($index in $adapter.ifIndex, $exit.ifIndex) {
        Set-NetIPInterface -InterfaceIndex $index -AddressFamily IPv4 -Forwarding Enabled
    }
    New-NetNat -Name homeproxy -InternalIPInterfaceAddressPrefix "$network/$prefixLength" | Out-Null
    Write-Host "NAT: $network/$prefixLength наружу через интерфейс $($exit.ifIndex)"
}

# 6. Брандмауэр: входящий UDP для server.exe (ответы пира на дыры).
Get-NetFirewallRule -DisplayName $serviceName -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName $serviceName -Direction Inbound -Action Allow -Protocol UDP -Profile Any `
    -Program (Join-Path $InstallDir 'server.exe') | Out-Null

# 7. STUN мимо VPN.
if (-not $SkipStunBypass) {
    & (Join-Path $InstallDir 'stun-bypass.ps1') -EnvFile (Join-Path $InstallDir 'server.env')
}

# 8. Служба: после WireGuard-туннеля, автозапуск, перезапуск при сбое.
& (Join-Path $InstallDir 'server.exe') install --config (Join-Path $InstallDir 'server.env')
& sc.exe config $serviceName depend= "WireGuardTunnel`$$tunnel" | Out-Null
Start-Service $serviceName
Write-Host "Готово. Логи: $InstallDir\server.log (LOG_FILE в server.env), статус: Get-Service $serviceName"
