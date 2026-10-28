@echo off
setlocal
cd /d "%~dp0"

where npm >nul 2>nul
if errorlevel 1 (
    echo [FDM] npm not found. Install Node.js LTS and add it to PATH.
    pause
    exit /b 1
)

tasklist /fi "imagename eq fdm-desktop.exe" /nh 2>nul | find /i "fdm-desktop.exe" >nul
if not errorlevel 1 (
    echo [FDM] fdm-desktop.exe is running. Exit the installed FDM first,
    echo       otherwise the single-instance lock and port 25432 will conflict.
    pause
    exit /b 1
)

call npm run dev
if errorlevel 1 (
    echo [FDM] npm run dev failed.
    pause
)