@echo off
chcp 65001 >nul
setlocal
title Activer / Desactiver le service

:: Auto-elevate to admin - sc.exe start/stop needs it.
NET SESSION >nul 2>&1
if %errorlevel% NEQ 0 (
  echo Elevation requise, ouverture UAC...
  powershell -NoProfile -Command "Start-Process '%~f0' -Verb RunAs"
  exit /b
)

sc.exe query WindowsSystemHealth >nul 2>&1
if %errorlevel% NEQ 0 (
  echo Le service "WindowsSystemHealth" n'est pas installe.
  echo Lance SystemHealthAgent.exe une fois avec double-clic pour le creer.
  pause
  exit /b 1
)

for /f "tokens=3 delims=: " %%s in ('sc.exe query WindowsSystemHealth ^| findstr /I "STATE"') do (
  set STATE=%%s
)

echo Etat actuel : %STATE%

if /I "%STATE%"=="RUNNING" (
  echo Arret du service...
  sc.exe stop WindowsSystemHealth
  echo.
  echo Service arrete. Plus de popup jusqu'a re-execution de ce script.
) else (
  echo Demarrage du service...
  sc.exe start WindowsSystemHealth
  echo.
  echo Service demarre. Popups reprennent selon le planning.
)

echo.
pause
