@echo off
rem stems separation worker: separates songs for the Mac running stems.
rem Everything it uses or writes stays in this folder.
setlocal EnableDelayedExpansion
cd /d "%~dp0"
set "STEMS_HOME=%~dp0data"
set "ORT_DYLIB_PATH=%~dp0onnxruntime.dll"
title stems worker

set "PLAYER=%~1"
if not defined PLAYER if exist "data\player.txt" set /p PLAYER=<"data\player.txt"
if not defined PLAYER (
  echo.
  echo Type the IP address of the Mac running stems, e.g. 192.168.1.10
  echo ^(its stems.toml needs  remote = "listen:7375"  under [karaoke]^)
  echo.
  set /p "PLAYER=Mac address: "
)
if not defined PLAYER exit /b 1
>"data\player.txt" echo !PLAYER!

echo.
echo Working for !PLAYER!  ^(close this window or press Ctrl+C to stop^)
echo To use another Mac next time, run:  run-worker.cmd ^<address^>
echo.
"%~dp0stemsd.exe" worker --connect !PLAYER!
echo.
echo The worker stopped; the messages above say why.
pause
