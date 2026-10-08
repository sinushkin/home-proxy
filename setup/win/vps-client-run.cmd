@echo off
rem Launcher of vps-client for the scheduled task "home-proxy vps-client" (runs as SYSTEM at boot).
rem Reads %ProgramData%\vps-client\vps-client.conf (KEY=VALUE, like on Linux), restarts the client
rem 5 seconds after it exits (the Windows counterpart of Restart=on-failure / procd respawn).
rem The log is %ProgramData%\vps-client\vps-client.log (rotated at 5 MB). ASCII only on purpose.
setlocal
set "DATA=%ProgramData%\vps-client"
if not exist "%DATA%\vps-client.conf" (
  echo vps-client.conf not found in %DATA% 1>&2
  exit /b 1
)
for /f "usebackq eol=# tokens=1,* delims==" %%a in ("%DATA%\vps-client.conf") do set "%%a=%%b"
set "WINTUN_DLL=%~dp0wintun.dll"
:loop
for %%f in ("%DATA%\vps-client.log") do if %%~zf GTR 5242880 move /y "%DATA%\vps-client.log" "%DATA%\vps-client.log.old" >nul
"%~dp0vps-client.exe" %VPS_SERVER% %VPS_MY_ID% %VPS_PEER_ID% 2>>"%DATA%\vps-client.log"
ping -n 6 127.0.0.1 >nul
goto loop
