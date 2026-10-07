@echo off
rem Times the separation model on this laptop: is it fast enough to be
rem the party's worker? No network or Mac needed. Takes 2-5 minutes.
setlocal
cd /d "%~dp0"
set "STEMS_HOME=%~dp0data"
set "ORT_DYLIB_PATH=%~dp0onnxruntime.dll"
title stems worker benchmark

echo.
echo ===== GPU (DirectML) =====
"%~dp0stemsd.exe" bench --device directml
echo.
echo ===== CPU only =====
"%~dp0stemsd.exe" bench --device cpu --sessions 1 --windows 3
echo.
echo Send a photo or copy of the two result blocks above.
pause
