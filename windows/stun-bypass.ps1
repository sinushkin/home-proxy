<#
.SYNOPSIS
  Пускает STUN мимо VPN: если маршрут до STUN-сервера идёт через виртуальный адаптер
  (VPN), добавляет к нему постоянный маршрут через физический шлюз.

.DESCRIPTION
  STUN должен видеть тот же внешний адрес, с которого ПК стучится к телефону. Если
  весь трафик ПК идёт через домашний VPN, а STUN-запрос уйдёт мимо него (или наоборот),
  телефон получит адрес, с которого дыры не открываются.
  Серверы берутся из STUN_ADDR в файле настроек службы (server.env), через запятую.
  Скрипт можно запускать повторно (после смены сети или шлюза); -WhatIf ничего не меняет.
  -Remove убирает маршруты, добавленные этим скриптом.

  Ограничение: обходятся только адреса STUN-серверов. Пакеты пробива до телефона идут
  по обычным маршрутам ПК; чтобы они тоже шли мимо VPN, нужны правила самого VPN-клиента
  (исключение UDP или сплит-туннель).
#>
#Requires -RunAsAdministrator
[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory)][string]$EnvFile,
    [switch]$Remove
)
$ErrorActionPreference = 'Stop'
$marker = 1   # RouteMetric наших маршрутов: по нему отличаем их при удалении

function Get-StunAddresses([string]$path) {
    $line = Select-String -Path $path -Pattern '^\s*(?:export\s+)?STUN_ADDR\s*=\s*(.+?)\s*$' | Select-Object -First 1
    if (-not $line) { throw "В $path нет STUN_ADDR" }
    $line.Matches[0].Groups[1].Value.Trim('"', "'") -split ',' |
        ForEach-Object { ($_.Trim() -split ':')[0] } |
        Where-Object { $_ -match '^\d{1,3}(\.\d{1,3}){3}$' }
}

$stun = @(Get-StunAddresses $EnvFile)
$physical = @(Get-NetAdapter -Physical | Where-Object Status -eq 'Up')
$physicalIndex = @($physical.ifIndex)

if ($Remove) {
    foreach ($ip in $stun) {
        Get-NetRoute -DestinationPrefix "$ip/32" -ErrorAction SilentlyContinue |
            Where-Object { $_.RouteMetric -eq $marker -and $_.ifIndex -in $physicalIndex } |
            ForEach-Object {
                if ($PSCmdlet.ShouldProcess("$ip/32", 'удалить маршрут')) {
                    Remove-NetRoute -InputObject $_ -Confirm:$false
                    Write-Host "STUN ${ip}: маршрут удалён"
                }
            }
    }
    return
}

# Шлюз по умолчанию среди физических адаптеров (с учётом метрики интерфейса).
$gateway = Get-NetRoute -DestinationPrefix '0.0.0.0/0' -AddressFamily IPv4 |
    Where-Object { $_.ifIndex -in $physicalIndex -and $_.NextHop -ne '0.0.0.0' } |
    Sort-Object { $_.RouteMetric + (Get-NetIPInterface -InterfaceIndex $_.ifIndex -AddressFamily IPv4).InterfaceMetric } |
    Select-Object -First 1
if (-not $gateway) { throw 'Не нашёл шлюз по умолчанию на физическом адаптере' }

foreach ($ip in $stun) {
    $selected = Find-NetRoute -RemoteIPAddress $ip | Select-Object -First 1
    $adapter = Get-NetAdapter -InterfaceIndex $selected.InterfaceIndex
    if ($selected.InterfaceIndex -in $physicalIndex) {
        Write-Host "STUN ${ip}: идёт напрямую (через '$($adapter.Name)'), правило не нужно"
        continue
    }
    Write-Host "STUN ${ip}: идёт через '$($adapter.Name)' ($($adapter.InterfaceDescription)), добавляю маршрут через $($gateway.NextHop)"
    if ($PSCmdlet.ShouldProcess("$ip/32", "маршрут через $($gateway.NextHop)")) {
        New-NetRoute -DestinationPrefix "$ip/32" -InterfaceIndex $gateway.ifIndex -NextHop $gateway.NextHop `
            -RouteMetric $marker -Confirm:$false | Out-Null
        $now = (Find-NetRoute -RemoteIPAddress $ip | Select-Object -First 1).InterfaceIndex
        if ($now -notin $physicalIndex) { Write-Warning "STUN ${ip}: маршрут добавлен, но трафик всё ещё идёт не через физический адаптер" }
    }
}
