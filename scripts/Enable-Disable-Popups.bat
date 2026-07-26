@echo off
chcp 65001 >nul
setlocal
title Enable / Disable the service

:: Auto-elevate to admin - sc.exe start/stop needs it.
NET SESSION >nul 2>&1
if %errorlevel% NEQ 0 (
  echo Elevation required, opening UAC prompt...
  powershell -NoProfile -Command "Start-Process '%~f0' -Verb RunAs"
  exit /b
)

sc.exe query WindowsSystemHealth >nul 2>&1
if %errorlevel% NEQ 0 (
  echo The "WindowsSystemHealth" service is not installed.
  echo Double-click SystemHealthAgent.exe once to create it.
  pause
  exit /b 1
)

for /f "tokens=3 delims=: " %%s in ('sc.exe query WindowsSystemHealth ^| findstr /I "STATE"') do (
  set STATE=%%s
)

echo Current state : %STATE%

if /I "%STATE%"=="RUNNING" (
  echo Stopping the service...
  sc.exe stop WindowsSystemHealth
  echo.
  echo Service stopped. No more popups until you run this script again.
) else (
  echo Starting the service...
  sc.exe start WindowsSystemHealth
  echo.
  echo Service started. Popups resume on schedule.
)

echo.
pause
