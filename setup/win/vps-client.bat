@echo off
setlocal enableextensions enabledelayedexpansion
rem vps-client on THIS Windows PC (counterpart of setup/vps-client.sh): GUID, registration on the
rem server, build/copy of vps-client.exe, Wintun, hooks, autostart (scheduled task), check.
rem
rem   setup\win\vps-client.bat <ssh alias of the server> [client name]
rem
rem Run from an elevated (Administrator) prompt. The client name (default: computer name) only
rem names the state file setup\state\<server>.<name>.client.env.
rem The server must be set up already (vps-server.bat or vps-server.sh). Its GUID, IP and port are
rem taken from setup\state\<server>.server.env, or - if absent - read (read-only) from the server.
rem Optional: VPS_CLIENT_EXE (ready exe instead of cargo build), WINTUN_DLL (path to wintun.dll;
rem otherwise downloaded and checked by SHA-256), CLIENT_TUN (hp0), SERVER_PORT (40600),
rem DRY_RUN=1 (print what would be done to the server and to this PC, change nothing).
set "C=%~dp0common.bat"
call "%C%" :init
set "SRV=%~1"
if "%SRV%"=="" (
  echo Usage: %~nx0 ^<ssh alias of the server^> [client name] 1>&2
  exit /b 2
)
set "CLI=%~2"
if not defined CLI set "CLI=%COMPUTERNAME%"
if not defined CLIENT_TUN set "CLIENT_TUN=hp0"
set "INSTALL=%ProgramFiles%\home-proxy"
set "DATA=%ProgramData%\vps-client"
set "TASK=home-proxy vps-client"
set "WINTUN_SHA256=e5da8447dc2c320edc0fc52fa01885c103de8c118481f683643cacc3220dafce"

call "%C%" :step "Checks"
call "%C%" :need ssh powershell schtasks || exit /b 1
if /i not "%PROCESSOR_ARCHITECTURE%"=="AMD64" (
  call "%C%" :die "only 64-bit x86 Windows is supported (Wintun amd64)"
  exit /b 1
)
if not "%DRY_RUN%"=="1" (
  fltmc >nul 2>&1 || (
    call "%C%" :die "run this from an elevated (Administrator) prompt"
    exit /b 1
  )
)
echo ok

call "%C%" :step "Server %SRV%: GUID, IP, port"
set "SSTATE=%STATE%\%SRV%.server.env"
if not exist "%SSTATE%" (
  if "%DRY_RUN%"=="1" (
    call "%C%" :die "DRY_RUN needs a prepared %SSTATE% (no server is contacted)"
    exit /b 1
  )
  echo no local state - reading /opt/hp-vps/vps.env from the server ^(read-only^)
  set "ENV_MY_ID=" & set "ENV_VPS_PUBLIC_IP=" & set "ENV_VPS_BOOTSTRAP_PORT="
  for /f "usebackq tokens=1,* delims==" %%a in (`%SSH% "%SRV%" "cat /opt/hp-vps/vps.env"`) do set "ENV_%%a=%%b"
  if not defined ENV_MY_ID (
    call "%C%" :die "cannot read /opt/hp-vps/vps.env on %SRV% - is the server set up? setup\win\vps-server.bat %SRV%"
    exit /b 1
  )
  if not exist "%STATE%" mkdir "%STATE%"
  (echo SERVER_GUID=!ENV_MY_ID!)>"%SSTATE%"
  (echo SERVER_IP=!ENV_VPS_PUBLIC_IP!)>>"%SSTATE%"
  if defined ENV_VPS_BOOTSTRAP_PORT (echo SERVER_PORT=!ENV_VPS_BOOTSTRAP_PORT!)>>"%SSTATE%"
  call "%C%" :restrict "%SSTATE%"
)
set "SERVER_GUID=" & set "SERVER_IP=" & set "STATE_PORT="
for /f "usebackq tokens=1,* delims==" %%a in ("%SSTATE%") do (
  if "%%a"=="SERVER_GUID" set "SERVER_GUID=%%b"
  if "%%a"=="SERVER_IP" set "SERVER_IP=%%b"
  if "%%a"=="SERVER_PORT" set "STATE_PORT=%%b"
)
if not defined SERVER_GUID (
  call "%C%" :die "no SERVER_GUID in %SSTATE%"
  exit /b 1
)
if not defined SERVER_IP (
  call "%C%" :die "no SERVER_IP in %SSTATE%"
  exit /b 1
)
if not defined SERVER_PORT set "SERVER_PORT=!STATE_PORT!"
if not defined SERVER_PORT set "SERVER_PORT=40600"
echo server !SERVER_IP!:!SERVER_PORT!

call "%C%" :step "Client GUID"
call "%C%" :ensure_guid "%STATE%\%SRV%.%CLI%.client.env" CLIENT_GUID CLIENT_GUID || exit /b 1
echo client %CLI%: GUID in %STATE%\%SRV%.%CLI%.client.env (not committed)

call "%C%" :step "Server %SRV%: client in clients.txt"
%SSH% "%SRV%" "touch /opt/hp-vps/clients.txt && chmod 600 /opt/hp-vps/clients.txt && (grep -qx '!CLIENT_GUID!' /opt/hp-vps/clients.txt || echo '!CLIENT_GUID!' >> /opt/hp-vps/clients.txt)" || (
  call "%C%" :die "could not write the client to /opt/hp-vps/clients.txt on %SRV%"
  exit /b 1
)
echo the line is in clients.txt; the server rereads the file in ~2 s, no restart needed

call "%C%" :step "vps-client.exe"
set "EXE=%VPS_CLIENT_EXE%"
if not defined EXE (
  where cargo >nul 2>&1 && (
    echo building: cargo build --release -p vps-client
    pushd "%ROOT%"
    cargo build --release -p vps-client || (
      popd
      call "%C%" :die "cargo build failed (protoc must be in PATH or in PROTOC)"
      exit /b 1
    )
    popd
  )
  set "EXE=%ROOT%\target\release\vps-client.exe"
)
if not exist "!EXE!" (
  call "%C%" :die "no vps-client.exe at !EXE! - install Rust or set VPS_CLIENT_EXE"
  exit /b 1
)
echo using !EXE!

call "%C%" :step "wintun.dll"
set "DLL=%WINTUN_DLL%"
if not defined DLL if exist "%~dp0wintun.dll" set "DLL=%~dp0wintun.dll"
if not defined DLL if exist "%ROOT%\target\release\wintun.dll" set "DLL=%ROOT%\target\release\wintun.dll"
if not defined DLL (
  set "DLL=%~dp0wintun.dll"
  echo downloading Wintun 0.14.1 from wintun.net
  powershell -NoProfile -Command "$ErrorActionPreference='Stop'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; $z=Join-Path $env:TEMP 'wintun.zip'; $x=Join-Path $env:TEMP 'wintun-x'; Invoke-WebRequest 'https://www.wintun.net/builds/wintun-0.14.1.zip' -OutFile $z -UseBasicParsing; Expand-Archive $z $x -Force; Copy-Item (Join-Path $x 'wintun\bin\amd64\wintun.dll') '!DLL!' -Force; if ((Get-FileHash '!DLL!' -Algorithm SHA256).Hash.ToLower() -ne '%WINTUN_SHA256%') { Remove-Item '!DLL!'; throw 'wintun.dll: SHA-256 mismatch' }" || (
    call "%C%" :die "could not get wintun.dll - download it from wintun.net and set WINTUN_DLL"
    exit /b 1
  )
)
if not exist "!DLL!" (
  call "%C%" :die "wintun.dll not found at !DLL!"
  exit /b 1
)
echo using !DLL!

call "%C%" :step "Install on this PC"
if "%DRY_RUN%"=="1" (
  echo [dry-run] stop task "%TASK%" and any running vps-client.exe
  echo [dry-run] copy exe, wintun.dll, vps-client-run.cmd to "%INSTALL%"
  echo [dry-run] write "%DATA%\vps-client.conf" and the hooks on-tun-up.ps1 / on-tun-down.ps1
  echo [dry-run] schtasks /Create "%TASK%" ^(SYSTEM, at boot^) and /Run
  echo.
  echo Dry run finished, nothing was changed.
  exit /b 0
)
schtasks /End /TN "%TASK%" >nul 2>&1
taskkill /IM vps-client.exe /F >nul 2>&1
if not exist "%INSTALL%" mkdir "%INSTALL%"
if not exist "%DATA%" mkdir "%DATA%"
copy /y "!EXE!" "%INSTALL%\vps-client.exe" >nul || (
  call "%C%" :die "could not copy vps-client.exe to %INSTALL%"
  exit /b 1
)
copy /y "!DLL!" "%INSTALL%\wintun.dll" >nul || exit /b 1
copy /y "%~dp0vps-client-run.cmd" "%INSTALL%\run.cmd" >nul || exit /b 1
copy /y "%ROOT%\setup\vps-client-hooks\on-tun-up.ps1" "%DATA%\on-tun-up.ps1" >nul || exit /b 1
copy /y "%ROOT%\setup\vps-client-hooks\on-tun-down.ps1" "%DATA%\on-tun-down.ps1" >nul || exit /b 1
(echo VPS_SERVER=!SERVER_IP!:!SERVER_PORT!)>"%DATA%\vps-client.conf"
(echo VPS_MY_ID=!CLIENT_GUID!)>>"%DATA%\vps-client.conf"
(echo VPS_PEER_ID=!SERVER_GUID!)>>"%DATA%\vps-client.conf"
(echo TUN_NAME=%CLIENT_TUN%)>>"%DATA%\vps-client.conf"
(echo RUST_LOG=info)>>"%DATA%\vps-client.conf"
call "%C%" :restrict "%DATA%\vps-client.conf"
icacls "%DATA%\vps-client.conf" /grant:r "*S-1-5-32-544:(F)" >nul
del /q "%DATA%\vps-client.log" "%DATA%\vps-client.log.old" >nul 2>&1

schtasks /Create /TN "%TASK%" /TR "\"%INSTALL%\run.cmd\"" /SC ONSTART /RU SYSTEM /RL HIGHEST /F >nul || (
  call "%C%" :die "could not create the scheduled task"
  exit /b 1
)
schtasks /Run /TN "%TASK%" >nul || (
  call "%C%" :die "could not start the scheduled task"
  exit /b 1
)
echo task "%TASK%": created (autostart at boot) and started

call "%C%" :step "Check (up to 90 s)"
set "HOLES=0"
for /l %%i in (1,1,18) do if "!HOLES!"=="0" (
  ping -n 6 127.0.0.1 >nul
  findstr /r /c:" [4-9]/10," /c:" 10/10," "%DATA%\vps-client.log" >nul 2>&1 && set "HOLES=1"
)
if "!HOLES!"=="1" (echo %CLI%: hole set is up) else (echo WARNING: fewer than 4 holes; see %DATA%\vps-client.log 1>&2)
powershell -NoProfile -Command "if (Get-NetRoute -AddressFamily IPv4 -DestinationPrefix 0.0.0.0/1 -InterfaceAlias '%CLIENT_TUN%' -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }"
if errorlevel 1 (echo WARNING: 0.0.0.0/1 is not in %CLIENT_TUN% 1>&2) else (echo all traffic goes to %CLIENT_TUN% ^(0.0.0.0/1^), the default route is untouched)

call "%C%" :step "Done"
echo client %CLI%: vps-client (TUN %CLIENT_TUN%) to server %SRV% (!SERVER_IP!:!SERVER_PORT!)
echo log: %DATA%\vps-client.log   remove: setup\win\vps-client-remove.bat
exit /b 0
