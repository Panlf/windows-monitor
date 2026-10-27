@echo off
REM WinMon one-click packaging: build release and copy the exe into dist\
setlocal
cd /d "%~dp0"

echo === [1/2] Building release ===
cargo build --release
if errorlevel 1 (
    echo [FAILED] Build error. Aborted.
    pause
    exit /b 1
)

echo === [2/2] Copying artifacts to dist\ ===
set "DIST=dist"
if not exist "%DIST%" mkdir "%DIST%"
copy /Y "target\release\winmon.exe" "%DIST%\winmon.exe" >nul
if exist "winmon.manifest" copy /Y "winmon.manifest" "%DIST%\winmon.manifest" >nul

echo.
echo Packaging done: %DIST%\winmon.exe  (single file, ~286KB)
echo  - Copy winmon.exe to another PC and run it directly (no install / no runtime needed).
echo  - First start requests admin rights: allow it to read CPU/disk temperatures.
echo  - GPU temp/frequency: NVIDIA via NVML, AMD via ADL; CPU temp needs admin rights.
echo.
pause
endlocal