@echo off
setlocal enableextensions
rem Firewall/NAT on the server (ufw, iptables, unit hp-vps-nat): runs setup/vps-prepare.sh, which
rem only talks to the server over ssh, with the bash from Git for Windows.
rem
rem   setup\win\vps-prepare.bat <ssh alias of the server>
rem   set DRY_RUN=1 & setup\win\vps-prepare.bat <alias>      (prints the commands only)
rem
rem Do not use bash.exe from System32: that is the WSL launcher and sees the Linux, not Windows, ssh config.
set "BASH="
if exist "%ProgramFiles%\Git\bin\bash.exe" set "BASH=%ProgramFiles%\Git\bin\bash.exe"
if not defined BASH if exist "%ProgramFiles(x86)%\Git\bin\bash.exe" set "BASH=%ProgramFiles(x86)%\Git\bin\bash.exe"
if not defined BASH if exist "%LOCALAPPDATA%\Programs\Git\bin\bash.exe" set "BASH=%LOCALAPPDATA%\Programs\Git\bin\bash.exe"
if not defined BASH (
  echo ERROR: Git for Windows not found ^(https://git-scm.com^); vps-prepare.sh needs its bash 1>&2
  exit /b 1
)
if "%~1"=="" (
  echo Usage: %~nx0 ^<ssh alias of the server^> 1>&2
  exit /b 2
)
pushd "%~dp0..\.."
"%BASH%" setup/vps-prepare.sh %*
set "RC=%ERRORLEVEL%"
popd
exit /b %RC%
