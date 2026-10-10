#!/usr/bin/env bash
# Ставит собранный APK приложения на телефон по adb (обновление поверх: -r, данные сохраняются).
#
#   setup/android-install.sh [путь к apk] [серийный номер устройства]
#
# По умолчанию — android-vpn/app/build/outputs/apk/debug/app-debug.apk. Если устройств несколько,
# укажите серийный номер (adb devices). Сборка: android-vpn/README.md.
set -euo pipefail
. "$(dirname "$0")/common.sh"

APK="${1:-$ROOT/android-vpn/app/build/outputs/apk/debug/app-debug.apk}"
SERIAL="${2:-}"

command -v adb >/dev/null || die "нет adb: пакет android-tools-adb"
[[ -f "$APK" ]] || die "нет $APK: сначала соберите приложение (android-vpn/README.md)"

ADB=(adb)
[[ -n "$SERIAL" ]] && ADB=(adb -s "$SERIAL")

step "Устройства"
adb devices
step "Установка $APK"
"${ADB[@]}" install -r "$APK"
