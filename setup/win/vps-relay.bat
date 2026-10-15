@echo off
setlocal enableextensions enabledelayedexpansion
rem Relay vps-relay for vps-server, set up from Windows over ssh (counterpart of setup/vps-relay.sh).
rem
rem   setup\win\vps-relay.bat <ssh alias of the relay> <ssh alias of the server>
rem
rem For clients the relay is one more server: part of their holes go through it. The relay comes to
rem the server by itself (its GUID is added to relays.txt of the server, which is then restarted -
rem only if the relay is new). The relay gets /opt/hp-relay (vps-relay, relay.env) and the service
rem hp-vps-relay; ports are opened only via an active ufw (otherwise a warning).
rem Windows cannot build the Linux binary: build it on Linux/WSL (cargo build --release -p
rem vps-relay) and point VPS_RELAY_BIN to it (default: target\release\vps-relay).
rem Optional: SERVER_DIR (/opt/hp-vps), SERVER_UNIT (hp-vps-server), RELAY_PORT and RELAY_SLOTS
rem (default: the same as the server), RELAY_PUBLIC_IP (default: the relay address; "auto" - the
rem address the relay comes to the server from, for a grey address with forwarded ports),
rem DRY_RUN=1 (print ssh commands).
set "C=%~dp0common.bat"
call "%C%" :init
set "RELAY=%~1"
set "SRV=%~2"
if "%SRV%"=="" (
  echo Usage: %~nx0 ^<ssh alias of the relay^> ^<ssh alias of the server^> 1>&2
  exit /b 2
)
if not defined SERVER_DIR set "SERVER_DIR=/opt/hp-vps"
if not defined SERVER_UNIT set "SERVER_UNIT=hp-vps-server"
if not defined VPS_RELAY_BIN set "VPS_RELAY_BIN=%ROOT%\target\release\vps-relay"

call "%C%" :step "Tools"
call "%C%" :need ssh powershell || exit /b 1
if not exist "%VPS_RELAY_BIN%" (
  call "%C%" :die "no Linux vps-relay binary at %VPS_RELAY_BIN% - build it on Linux/WSL and set VPS_RELAY_BIN"
  exit /b 1
)
echo ok

call "%C%" :step "Server %SRV% (%SERVER_DIR%)"
set "S_MY_ID=" & set "S_VPS_PUBLIC_IP=" & set "S_VPS_BOOTSTRAP_PORT=" & set "S_VPS_PORTS="
if "%DRY_RUN%"=="1" (
  set "S_MY_ID=00000000-0000-0000-0000-000000000000" & set "S_VPS_PUBLIC_IP=192.0.2.1"
) else (
  for /f "tokens=1,* delims==" %%a in ('%SSH% "%SRV%" "cat %SERVER_DIR%/vps.env"') do set "S_%%a=%%b"
)
if not defined S_MY_ID (
  call "%C%" :die "no MY_ID in %SERVER_DIR%/vps.env on %SRV% - run vps-server.bat first"
  exit /b 1
)
if not defined S_VPS_PUBLIC_IP (
  call "%C%" :die "no VPS_PUBLIC_IP in %SERVER_DIR%/vps.env on %SRV%"
  exit /b 1
)
if not defined S_VPS_BOOTSTRAP_PORT set "S_VPS_BOOTSTRAP_PORT=40000"
if not defined S_VPS_PORTS set "S_VPS_PORTS=40001-49999"
if not defined RELAY_PORT set "RELAY_PORT=!S_VPS_BOOTSTRAP_PORT!"
if not defined RELAY_SLOTS set "RELAY_SLOTS=!S_VPS_PORTS!"
echo server !S_VPS_PUBLIC_IP!:!S_VPS_BOOTSTRAP_PORT!, service %SERVER_UNIT%

call "%C%" :step "Relay %RELAY%: probe (read-only)"
set "SRV_ARCH=" & set "SRV_GLIBC=" & set "SRV_IP="
if "%DRY_RUN%"=="1" (
  set "SRV_ARCH=x86_64" & set "SRV_GLIBC=2.36" & set "SRV_IP=192.0.2.2"
) else (
  for /f "tokens=1,* delims==" %%a in ('%SSH% "%RELAY%" "sh -s" ^< "%REMOTE%\server-probe.sh"') do set "SRV_%%a=%%b"
)
if not "!SRV_ARCH!"=="x86_64" (
  call "%C%" :die "cannot reach %RELAY% over ssh, or it is not x86_64 (got '!SRV_ARCH!')"
  exit /b 1
)
if not defined RELAY_PUBLIC_IP set "RELAY_PUBLIC_IP=!SRV_IP!"
if /i "!RELAY_PUBLIC_IP!"=="auto" set "RELAY_PUBLIC_IP="
echo x86_64, glibc !SRV_GLIBC!, address for clients: !RELAY_PUBLIC_IP!, ports !RELAY_PORT!, !RELAY_SLOTS!

call "%C%" :step "Relay GUID"
call "%C%" :ensure_guid "%STATE%\%RELAY%.relay.env" RELAY_GUID RELAY_GUID || exit /b 1
echo state: %STATE%\%RELAY%.relay.env (not committed)

call "%C%" :step "Binary"
call "%C%" :md5 "%VPS_RELAY_BIN%" LOCAL_MD5
set "REMOTE_MD5="
if not "%DRY_RUN%"=="1" (
  for /f %%h in ('%SSH% "%RELAY%" "md5sum /opt/hp-relay/vps-relay 2>/dev/null"') do if not defined REMOTE_MD5 set "REMOTE_MD5=%%h"
)
if /i "!LOCAL_MD5!"=="!REMOTE_MD5!" (
  echo vps-relay on the relay is the same as !VPS_RELAY_BIN!
) else (
  echo uploading !VPS_RELAY_BIN!
  %SSH% "%RELAY%" "mkdir -p /opt/hp-relay && chmod 700 /opt/hp-relay && cat > /opt/hp-relay/vps-relay.upload" < "%VPS_RELAY_BIN%" || (
    call "%C%" :die "upload failed"
    exit /b 1
  )
)

call "%C%" :step "Relay %RELAY%: files and service"
%SSH% "%RELAY%" "env RELAY_GUID=!RELAY_GUID! SERVER_GUID=!S_MY_ID! SERVER_ADDR=!S_VPS_PUBLIC_IP!:!S_VPS_BOOTSTRAP_PORT! RELAY_PUBLIC_IP=!RELAY_PUBLIC_IP! RELAY_PORT=!RELAY_PORT! RELAY_SLOTS=!RELAY_SLOTS! sh -s" < "%REMOTE%\relay-install.sh" || (
  call "%C%" :die "relay setup failed (see the messages above)"
  exit /b 1
)

call "%C%" :step "Server %SRV%: relay in relays.txt"
%SSH% "%SRV%" "env SERVER_DIR=%SERVER_DIR% SERVER_UNIT=%SERVER_UNIT% RELAY_GUID=!RELAY_GUID! sh -s" < "%REMOTE%\server-add-relay.sh" || (
  call "%C%" :die "adding the relay to the server failed (see the messages above)"
  exit /b 1
)

call "%C%" :step "Done"
echo relay %RELAY% is set up for %SRV%: clients learn about it within a minute.
echo check on the server: journalctl -u %SERVER_UNIT% ^| grep "!RELAY_GUID:~0,8!"
exit /b 0
