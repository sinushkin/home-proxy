@echo off
setlocal enableextensions
rem Wrapper: server + this PC as a client (counterpart of setup/vps-client-router-vps-server.sh).
rem   setup\win\vps-client-router-vps-server.bat <ssh alias of the server>
rem Separate scripts are for adding clients to a server that is already running.
if "%~1"=="" (
  echo Usage: %~nx0 ^<ssh alias of the server^> 1>&2
  exit /b 2
)
call "%~dp0vps-server.bat" %1 || exit /b 1
call "%~dp0vps-client.bat" %1
exit /b %ERRORLEVEL%
