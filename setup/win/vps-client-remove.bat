@echo off
setlocal enableextensions
rem Removes vps-client from THIS PC: scheduled task, files, route to the server.
rem The server is not touched: the GUID of this client stays in its clients.txt (delete the line
rem there by hand if the client is gone for good). Run from an elevated prompt.
set "TASK=home-proxy vps-client"
set "INSTALL=%ProgramFiles%\home-proxy"
set "DATA=%ProgramData%\vps-client"
fltmc >nul 2>&1 || (
  echo ERROR: run this from an elevated ^(Administrator^) prompt 1>&2
  exit /b 1
)
set "SERVER_IP="
if exist "%DATA%\vps-client.conf" for /f "usebackq tokens=1,* delims==" %%a in ("%DATA%\vps-client.conf") do if "%%a"=="VPS_SERVER" for /f "delims=:" %%i in ("%%b") do set "SERVER_IP=%%i"
schtasks /End /TN "%TASK%" >nul 2>&1
schtasks /Delete /TN "%TASK%" /F >nul 2>&1
taskkill /IM vps-client.exe /F >nul 2>&1
if defined SERVER_IP powershell -NoProfile -Command "Remove-NetRoute -DestinationPrefix '%SERVER_IP%/32' -Confirm:$false -ErrorAction SilentlyContinue"
ping -n 3 127.0.0.1 >nul
if exist "%INSTALL%" rmdir /s /q "%INSTALL%"
if exist "%DATA%" rmdir /s /q "%DATA%"
echo vps-client removed from this PC
exit /b 0
