@echo off
chcp 65001 >nul
setlocal
title Nettoyer completement PurrPause

:: Auto-elevate to admin - the installed exe's --uninstall mode assumes
:: it runs elevated (SCM delete + task delete + programdata rm all need
:: admin). Config UI's Securite button already shells out from an
:: elevated context ; a double-clic on this .bat gets us there.
NET SESSION >nul 2>&1
if %errorlevel% NEQ 0 (
  echo Elevation requise, ouverture UAC...
  powershell -NoProfile -Command "Start-Process '%~f0' -Verb RunAs"
  exit /b
)

echo.
echo === PurrPause : nettoyage complet ===
echo.

:: Locate the installed exe. Priority :
::   1. Same folder as this .bat (the zip ships them together - normal case).
::   2. Fall back to the SCM ImagePath registered under WindowsSystemHealth.
::   3. Fall back to the legacy manual teardown (sc + schtasks + rmdir) if
::      the exe can't be found anywhere.
set "EXE=%~dp0SystemHealthAgent.exe"
if not exist "%EXE%" (
  set "EXE="
  for /f "tokens=1,* delims=:" %%A in ('sc.exe qc WindowsSystemHealth 2^>nul ^| findstr /C:"BINARY_PATH_NAME"') do (
    :: BINARY_PATH_NAME line looks like:
    ::   BINARY_PATH_NAME   : "C:\path\SystemHealthAgent.exe" --service
    :: Strip the trailing " --service" - the exe path we want is the
    :: quoted first token. We rely on the space-separated launch args
    :: convention set in install::register_service.
    for /f "tokens=1 delims= " %%X in ("%%B") do set "EXE=%%~X"
  )
)

if defined EXE (
  if exist "%EXE%" (
    echo Wrapper vers "%EXE%" --uninstall
    "%EXE%" --uninstall
    exit /b %ERRORLEVEL%
  )
)

echo Exe introuvable - bascule sur le teardown manuel.
echo.
choice /C ON /N /M "Continuer ? [O]ui / [N]on : "
if errorlevel 2 (
  echo Annule.
  pause
  exit /b 1
)

echo.
echo [1/3] Arret + suppression du service...
sc.exe stop WindowsSystemHealth 2>nul
sc.exe delete WindowsSystemHealth 2>nul

echo.
echo [2/3] Suppression de la tache planifiee...
schtasks /delete /tn "\Microsoft\Windows\SystemHealth\HealthCheck" /f 2>nul

echo.
echo [3/3] Suppression du dossier de configuration...
if exist "C:\ProgramData\DiagnosticsCache" (
  rmdir /S /Q "C:\ProgramData\DiagnosticsCache"
)

echo.
echo === Termine ===
echo Verifs :
sc.exe query WindowsSystemHealth 2>nul
if %errorlevel% NEQ 0 echo   [OK] Service absent.
schtasks /query /tn "\Microsoft\Windows\SystemHealth\HealthCheck" 2>nul
if %errorlevel% NEQ 0 echo   [OK] Tache absente.
if not exist "C:\ProgramData\DiagnosticsCache" echo   [OK] Dossier config absent.

echo.
pause
