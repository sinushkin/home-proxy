@echo off
rem Shared helpers for setup\win\*.bat. Usage: call "%~dp0common.bat" :label args...
rem Not meant to be run on its own. ASCII only on purpose: cmd with UTF-8 breaks goto/call labels.
rem No setlocal here: variables set by :init and :ensure_guid stay visible to the caller.
if "%~1"=="" (
  echo common.bat is a library, see setup\win\README.md 1>&2
  exit /b 1
)
goto %~1

:init
rem ROOT = repository root, STATE = setup\state (secrets: GUIDs, not committed).
for %%i in ("%~dp0..\..") do set "ROOT=%%~fi"
if not defined STATE set "STATE=%ROOT%\setup\state"
if not defined DRY_RUN set "DRY_RUN=0"
set "REMOTE=%~dp0remote"
rem SSH: like ssh_to in common.sh (no ProxyJump, no prompts). DRY_RUN=1 only prints the commands.
set "SSH=ssh -o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR -o ProxyJump=none"
if "%DRY_RUN%"=="1" set "SSH=echo [dry-run] ssh"
exit /b 0

:step
echo.
echo == %~2
exit /b 0

:die
echo ERROR: %~2 1>&2
exit /b 1

:need
rem :need tool1 tool2 ... - every tool must be in PATH.
for %%t in (%2 %3 %4 %5 %6) do (
  where %%t >nul 2>&1 || (
    echo ERROR: %%t not found in PATH 1>&2
    exit /b 1
  )
)
exit /b 0

:ensure_guid
rem :ensure_guid <state file> <FIELD> <result variable>
rem Creates the file with FIELD=<new GUID> if it is missing, then reads FIELD from it.
if not exist "%~2" (
  if not exist "%STATE%" mkdir "%STATE%"
  powershell -NoProfile -Command "[IO.File]::WriteAllText('%~2', '%~3=' + [guid]::NewGuid().ToString() + [Environment]::NewLine)" || exit /b 1
  call "%~f0" :restrict "%~2"
)
set "%~4="
for /f "usebackq tokens=1,* delims==" %%a in ("%~2") do if "%%a"=="%~3" set "%~4=%%b"
if not defined %~4 (
  echo ERROR: broken file %~2 ^(no %~3^) 1>&2
  exit /b 1
)
exit /b 0

:restrict
rem :restrict <file> - only the current user (and SYSTEM) may read it, like chmod 600.
icacls "%~2" /inheritance:r /grant:r "%USERNAME%:(F)" "SYSTEM:(F)" >nul
exit /b 0

:md5
rem :md5 <file> <result variable>
set "%~3="
for /f %%h in ('powershell -NoProfile -Command "(Get-FileHash -Algorithm MD5 -LiteralPath '%~2').Hash.ToLower()"') do set "%~3=%%h"
exit /b 0
