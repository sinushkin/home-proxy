@echo off
setlocal enableextensions enabledelayedexpansion
rem Server for vps-client, set up from Windows over ssh (counterpart of setup/vps-server.sh).
rem
rem   setup\win\vps-server.bat <ssh alias of the server>
rem
rem The server (x86_64 Linux with systemd, white IP) gets /opt/hp-vps (vps-server, vps.env,
rem clients.txt) and the service hp-vps-server. Firewall/NAT is separate: vps-prepare.bat.
rem Windows cannot build the Linux binary: build it on Linux/WSL (cargo build --release -p
rem vps-server) and point VPS_SERVER_BIN to it (default: target\release\vps-server).
rem Optional: SERVER_PORT (40600), SERVER_SLOTS (40601-40699), SERVER_TUN (hp-vps),
rem YES=1 (no question when the server is already configured), DRY_RUN=1 (print ssh commands).
set "C=%~dp0common.bat"
call "%C%" :init
set "SRV=%~1"
if "%SRV%"=="" (
  echo Usage: %~nx0 ^<ssh alias of the server^> 1>&2
  exit /b 2
)
if not defined SERVER_PORT set "SERVER_PORT=40600"
if not defined SERVER_SLOTS set "SERVER_SLOTS=40601-40699"
if not defined SERVER_TUN set "SERVER_TUN=hp-vps"
set "SERVER_ADDR=10.94.0.1/24"
if not defined VPS_SERVER_BIN set "VPS_SERVER_BIN=%ROOT%\target\release\vps-server"

call "%C%" :step "Tools"
call "%C%" :need ssh powershell || exit /b 1
if not exist "%VPS_SERVER_BIN%" (
  call "%C%" :die "no Linux vps-server binary at %VPS_SERVER_BIN% - build it on Linux/WSL and set VPS_SERVER_BIN"
  exit /b 1
)
echo ok

call "%C%" :step "Server %SRV%: probe (read-only)"
set "SRV_ARCH=" & set "SRV_GLIBC=" & set "SRV_IP=" & set "SRV_EXISTING_ID=" & set "SRV_ACTIVE="
if "%DRY_RUN%"=="1" (
  set "SRV_ARCH=x86_64" & set "SRV_GLIBC=2.36" & set "SRV_IP=192.0.2.1"
) else (
  for /f "tokens=1,* delims==" %%a in ('%SSH% "%SRV%" "sh -s" ^< "%REMOTE%\server-probe.sh"') do set "SRV_%%a=%%b"
)
if not "!SRV_ARCH!"=="x86_64" (
  call "%C%" :die "cannot reach %SRV% over ssh, or it is not x86_64 (got '!SRV_ARCH!')"
  exit /b 1
)
if not defined SRV_IP (
  call "%C%" :die "could not detect the public IP of the server"
  exit /b 1
)
echo x86_64, glibc !SRV_GLIBC!, public IP !SRV_IP!, service: !SRV_ACTIVE!

call "%C%" :step "Server GUID"
set "SSTATE=%STATE%\%SRV%.server.env"
rem Never invent a new GUID for a server that already has one: every client would lose it.
if exist "%SSTATE%" (
  call "%C%" :ensure_guid "%SSTATE%" SERVER_GUID SERVER_GUID || exit /b 1
  if defined SRV_EXISTING_ID if /i not "!SRV_EXISTING_ID!"=="!SERVER_GUID!" (
    call "%C%" :die "the server already runs with another GUID than %SSTATE% - refusing to replace it"
    exit /b 1
  )
) else if defined SRV_EXISTING_ID (
  set "SERVER_GUID=!SRV_EXISTING_ID!"
  echo adopting the GUID the server already uses
) else (
  call "%C%" :ensure_guid "%SSTATE%" SERVER_GUID SERVER_GUID || exit /b 1
)
if not exist "%STATE%" mkdir "%STATE%"
(echo SERVER_GUID=!SERVER_GUID!)>"%SSTATE%"
(echo SERVER_IP=!SRV_IP!)>>"%SSTATE%"
(echo SERVER_PORT=%SERVER_PORT%)>>"%SSTATE%"
call "%C%" :restrict "%SSTATE%"
echo state: %SSTATE% (not committed)

if defined SRV_EXISTING_ID if not "%YES%"=="1" if not "%DRY_RUN%"=="1" (
  echo.
  echo The server %SRV% is already configured and may be in use.
  echo This run restarts hp-vps-server ONLY if the binary or vps.env differs.
  set /p "ANSWER=Continue? [y/N] "
  if /i not "!ANSWER!"=="y" (
    echo Cancelled, nothing changed.
    exit /b 1
  )
)

call "%C%" :step "Binary"
call "%C%" :md5 "%VPS_SERVER_BIN%" LOCAL_MD5
set "REMOTE_MD5="
if not "%DRY_RUN%"=="1" (
  for /f %%h in ('%SSH% "%SRV%" "md5sum /opt/hp-vps/vps-server 2>/dev/null"') do if not defined REMOTE_MD5 set "REMOTE_MD5=%%h"
)
if /i "!LOCAL_MD5!"=="!REMOTE_MD5!" (
  echo vps-server on the server is the same as !VPS_SERVER_BIN!
) else (
  echo uploading !VPS_SERVER_BIN!
  %SSH% "%SRV%" "mkdir -p /opt/hp-vps && chmod 700 /opt/hp-vps && cat > /opt/hp-vps/vps-server.upload" < "%VPS_SERVER_BIN%" || (
    call "%C%" :die "upload failed"
    exit /b 1
  )
)

call "%C%" :step "Server %SRV%: files and service"
%SSH% "%SRV%" "env SERVER_GUID=!SERVER_GUID! SRV_IP=!SRV_IP! SERVER_PORT=%SERVER_PORT% SERVER_SLOTS=%SERVER_SLOTS% SERVER_TUN=%SERVER_TUN% SERVER_ADDR=%SERVER_ADDR% sh -s" < "%REMOTE%\server-install.sh" || (
  call "%C%" :die "server setup failed (see the messages above)"
  exit /b 1
)

call "%C%" :step "Done"
echo server %SRV%: !SRV_IP!:%SERVER_PORT%, TUN %SERVER_TUN% %SERVER_ADDR%, slot ports %SERVER_SLOTS%
echo add clients: setup\win\vps-client.bat %SRV%
exit /b 0
