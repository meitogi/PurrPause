@echo off
chcp 65001 >nul
setlocal EnableDelayedExpansion
title Reset complet PurrPause

:: Auto-elevate to admin - same pattern as Nettoyer.bat. Every step
:: below (sc, schtasks, taskkill, rmdir under ProgramData, PendingFile
:: scrub) requires elevation.
NET SESSION >nul 2>&1
if %errorlevel% NEQ 0 (
  echo Elevation requise, ouverture UAC...
  powershell -NoProfile -Command "Start-Process '%~f0' -Verb RunAs"
  exit /b
)

echo.
echo === PurrPause : reset complet (etat corrompu / iteration smoke) ===
echo.
echo Ce script fait un wipe TOTAL :
echo   - Stoppe le service et tue les processus
echo   - Balaie les orphans msedgewebview2.exe (workers WebView2)
echo   - Supprime service + tache planifiee + registre
echo   - Supprime C:\ProgramData\DiagnosticsCache
echo   - Supprime %%~dp0Data (WebView2 + Animations)
echo   - Nettoie PendingFileRenameOperations
echo.
echo Reboot recommande a la fin pour completer les MoveFileEx.
echo.
choice /C ON /N /M "Continuer ? [O]ui / [N]on : "
if errorlevel 2 (
  echo Annule.
  pause
  exit /b 1
)

echo.
echo [1/9] Arret du service WindowsSystemHealth...
sc.exe stop WindowsSystemHealth >nul 2>&1
set /a __wait=0
:wait_stopped
sc.exe query WindowsSystemHealth 2>nul | findstr /C:"STOPPED" >nul
if %errorlevel% EQU 0 goto stopped
if %__wait% GEQ 5 (
  echo   [!] service pas STOPPED apres 5s ^(continue quand meme^)
  goto stopped
)
timeout /t 1 /nobreak >nul
set /a __wait=%__wait% + 1
goto wait_stopped
:stopped
echo   OK.

echo.
echo [2/9] Kill des processus SystemHealthAgent.exe...
taskkill /F /IM SystemHealthAgent.exe /T >nul 2>&1
echo   OK.

echo.
echo [3/9] Balayage des orphans msedgewebview2.exe...
powershell -NoProfile -Command "$ours = @(); Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | ForEach-Object { if ($_.CommandLine -like '*\Data\WebView2\*') { try { Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop; $ours += $_.ProcessId } catch {} } }; if ($ours.Count -gt 0) { Write-Host ('  ' + $ours.Count + ' worker(s) termine(s) : ' + ($ours -join ', ')) } else { Write-Host '  Aucun orphan detecte.' }"

echo.
echo [4/9] Suppression du service dans SCM...
sc.exe delete WindowsSystemHealth >nul 2>&1
if %errorlevel% EQU 0 (
  echo   OK.
) else (
  echo   [!] echec ^(peut-etre deja marque pour suppression - le reboot finalisera^)
)

echo.
echo [5/9] Suppression de la tache planifiee HealthCheck...
schtasks /delete /tn "\Microsoft\Windows\SystemHealth\HealthCheck" /f >nul 2>&1
if %errorlevel% EQU 0 (
  echo   OK.
) else (
  echo   Deja absente.
)

echo.
echo [6/9] Suppression de C:\ProgramData\DiagnosticsCache...
if exist "C:\ProgramData\DiagnosticsCache" (
  rmdir /S /Q "C:\ProgramData\DiagnosticsCache" >nul 2>&1
  if not exist "C:\ProgramData\DiagnosticsCache" (
    echo   OK.
  ) else (
    echo   [!] rmdir a laisse des residus ^(fichiers verrouilles ?^)
  )
) else (
  echo   Deja absent.
)

echo.
echo [7/9] Suppression de %~dp0Data (WebView2 + Animations)...
if exist "%~dp0Data" (
  rmdir /S /Q "%~dp0Data" >nul 2>&1
  if not exist "%~dp0Data" (
    echo   OK.
  ) else (
    echo   [!] rmdir a laisse des residus
  )
) else (
  echo   Deja absent.
)

echo.
echo [8/9] Delete-on-reboot des exe residents...
powershell -NoProfile -Command "Add-Type -Name MoveFileEx -Namespace Win32 -MemberDefinition '[DllImport(\"kernel32.dll\", CharSet = CharSet.Unicode, SetLastError = true)] public static extern bool MoveFileEx(string src, string dst, int flags);'; $count = 0; foreach ($name in @('SystemHealthAgent.exe', 'SystemHealthAgent.exe.manifest')) { $p = Join-Path '%~dp0' $name; if (Test-Path $p) { if ([Win32.MoveFileEx]::MoveFileEx($p, $null, 4)) { Write-Host ('  planifie : ' + $p); $count++ } else { Write-Host ('  [!] MoveFileEx echec : ' + $p) } } }; if ($count -eq 0) { Write-Host '  Rien a planifier.' }"

echo.
echo [9/9] Scrub PendingFileRenameOperations pour nos chemins...
powershell -NoProfile -Command "$key = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager'; $val = 'PendingFileRenameOperations'; try { $current = (Get-ItemProperty -Path $key -Name $val -ErrorAction Stop).$val } catch { Write-Host '  Aucune entree PendingFileRenameOperations.'; exit }; $needle1 = 'SystemHealthAgent.exe'; $needle2 = '%~dp0' -replace '\\','\\'; $filtered = @(); $removed = 0; for ($i = 0; $i -lt $current.Count; $i += 2) { $src = $current[$i]; $dst = if ($i + 1 -lt $current.Count) { $current[$i + 1] } else { '' }; if ($src -like ('*' + $needle1 + '*') -or $src -like ('*%~dp0*')) { $removed++ } else { $filtered += $src; $filtered += $dst } }; if ($removed -gt 0) { Set-ItemProperty -Path $key -Name $val -Value $filtered -Type MultiString; Write-Host ('  ' + $removed + ' entree(s) supprimee(s)') } else { Write-Host '  Rien a scrubber.' }"

echo.
echo === Reset termine ===
echo.
echo Verifs finales :
sc.exe query WindowsSystemHealth >nul 2>&1
if %errorlevel% NEQ 0 (echo   [OK] Service absent.) else (echo   [!] Service encore present dans SCM.)
schtasks /query /tn "\Microsoft\Windows\SystemHealth\HealthCheck" >nul 2>&1
if %errorlevel% NEQ 0 (echo   [OK] Tache absente.) else (echo   [!] Tache encore presente.)
if not exist "C:\ProgramData\DiagnosticsCache" (echo   [OK] DiagnosticsCache absent.) else (echo   [!] DiagnosticsCache encore present.)

echo.
choice /C ON /N /M "Reboot maintenant pour finaliser les MoveFileEx ? [O]ui / [N]on : "
if errorlevel 2 (
  echo.
  echo Rappel : les fichiers planifies pour delete-on-reboot ne seront
  echo effaces qu'apres le prochain redemarrage.
  pause
  exit /b 0
)

echo.
echo Reboot dans 5 secondes...
shutdown /r /t 5 /c "Reset PurrPause : reboot pour finaliser MoveFileEx"
exit /b 0
