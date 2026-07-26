@echo off
chcp 65001 >nul
setlocal EnableDelayedExpansion
title Full reset PurrPause

:: Auto-elevate to admin - same pattern as Uninstall.bat. Every step
:: below (sc, schtasks, taskkill, rmdir under ProgramData, PendingFile
:: scrub) requires elevation.
NET SESSION >nul 2>&1
if %errorlevel% NEQ 0 (
  echo Elevation required, opening UAC prompt...
  powershell -NoProfile -Command "Start-Process '%~f0' -Verb RunAs"
  exit /b
)

:: This .bat lives in <root>\scripts\ ; the exe + Data sit one level up.
:: Resolve that parent once to a canonical path (%~fI collapses the "..").
for %%I in ("%~dp0..") do set "ROOT=%%~fI"

echo.
echo === PurrPause : full reset (corrupt state / smoke iteration) ===
echo.
echo This script does a TOTAL wipe :
echo   - Stops the service and kills the processes
echo   - Sweeps orphan msedgewebview2.exe (WebView2 workers)
echo   - Deletes service + scheduled task + registry
echo   - Deletes C:\ProgramData\DiagnosticsCache
echo   - Deletes %ROOT%\Data (WebView2 + Animations)
echo   - Cleans PendingFileRenameOperations
echo.
echo Reboot recommended at the end to complete the MoveFileEx.
echo.
choice /C YN /N /M "Continue ? [Y]es / [N]o : "
if errorlevel 2 (
  echo Cancelled.
  pause
  exit /b 1
)

echo.
echo [1/9] Stopping the WindowsSystemHealth service...
sc.exe stop WindowsSystemHealth >nul 2>&1
set /a __wait=0
:wait_stopped
sc.exe query WindowsSystemHealth 2>nul | findstr /C:"STOPPED" >nul
if %errorlevel% EQU 0 goto stopped
if %__wait% GEQ 5 (
  echo   [!] service not STOPPED after 5s ^(continuing anyway^)
  goto stopped
)
timeout /t 1 /nobreak >nul
set /a __wait=%__wait% + 1
goto wait_stopped
:stopped
echo   OK.

echo.
echo [2/9] Killing SystemHealthAgent.exe processes...
taskkill /F /IM SystemHealthAgent.exe /T >nul 2>&1
echo   OK.

echo.
echo [3/9] Sweeping orphan msedgewebview2.exe...
powershell -NoProfile -Command "$ours = @(); Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | ForEach-Object { if ($_.CommandLine -like '*\Data\WebView2\*') { try { Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop; $ours += $_.ProcessId } catch {} } }; if ($ours.Count -gt 0) { Write-Host ('  ' + $ours.Count + ' worker(s) terminated : ' + ($ours -join ', ')) } else { Write-Host '  No orphan detected.' }"

echo.
echo [4/9] Deleting the service from SCM...
sc.exe delete WindowsSystemHealth >nul 2>&1
if %errorlevel% EQU 0 (
  echo   OK.
) else (
  echo   [!] failed ^(maybe already marked for deletion - the reboot will finalize^)
)

echo.
echo [5/9] Deleting the HealthCheck scheduled task...
schtasks /delete /tn "\Microsoft\Windows\SystemHealth\HealthCheck" /f >nul 2>&1
if %errorlevel% EQU 0 (
  echo   OK.
) else (
  echo   Already gone.
)

echo.
echo [6/9] Deleting C:\ProgramData\DiagnosticsCache...
if exist "C:\ProgramData\DiagnosticsCache" (
  rmdir /S /Q "C:\ProgramData\DiagnosticsCache" >nul 2>&1
  if not exist "C:\ProgramData\DiagnosticsCache" (
    echo   OK.
  ) else (
    echo   [!] rmdir left residue ^(locked files ?^)
  )
) else (
  echo   Already gone.
)

echo.
echo [7/9] Deleting %ROOT%\Data (WebView2 + Animations)...
if exist "%ROOT%\Data" (
  rmdir /S /Q "%ROOT%\Data" >nul 2>&1
  if not exist "%ROOT%\Data" (
    echo   OK.
  ) else (
    echo   [!] rmdir left residue
  )
) else (
  echo   Already gone.
)

echo.
echo [8/9] Delete-on-reboot of the resident exe...
powershell -NoProfile -Command "Add-Type -Name MoveFileEx -Namespace Win32 -MemberDefinition '[DllImport(\"kernel32.dll\", CharSet = CharSet.Unicode, SetLastError = true)] public static extern bool MoveFileEx(string src, string dst, int flags);'; $count = 0; foreach ($name in @('SystemHealthAgent.exe', 'SystemHealthAgent.exe.manifest')) { $p = Join-Path '%ROOT%' $name; if (Test-Path $p) { if ([Win32.MoveFileEx]::MoveFileEx($p, $null, 4)) { Write-Host ('  scheduled : ' + $p); $count++ } else { Write-Host ('  [!] MoveFileEx failed : ' + $p) } } }; if ($count -eq 0) { Write-Host '  Nothing to schedule.' }"

echo.
echo [9/9] Scrub PendingFileRenameOperations for our paths...
powershell -NoProfile -Command "$key = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager'; $val = 'PendingFileRenameOperations'; try { $current = (Get-ItemProperty -Path $key -Name $val -ErrorAction Stop).$val } catch { Write-Host '  No PendingFileRenameOperations entry.'; exit }; $needle1 = 'SystemHealthAgent.exe'; $needle2 = '%ROOT%\' -replace '\\','\\'; $filtered = @(); $removed = 0; for ($i = 0; $i -lt $current.Count; $i += 2) { $src = $current[$i]; $dst = if ($i + 1 -lt $current.Count) { $current[$i + 1] } else { '' }; if ($src -like ('*' + $needle1 + '*') -or $src -like ('*%ROOT%\*')) { $removed++ } else { $filtered += $src; $filtered += $dst } }; if ($removed -gt 0) { Set-ItemProperty -Path $key -Name $val -Value $filtered -Type MultiString; Write-Host ('  ' + $removed + ' entry(ies) removed') } else { Write-Host '  Nothing to scrub.' }"

echo.
echo === Reset done ===
echo.
echo Final checks :
sc.exe query WindowsSystemHealth >nul 2>&1
if %errorlevel% NEQ 0 (echo   [OK] Service gone.) else (echo   [!] Service still present in SCM.)
schtasks /query /tn "\Microsoft\Windows\SystemHealth\HealthCheck" >nul 2>&1
if %errorlevel% NEQ 0 (echo   [OK] Task gone.) else (echo   [!] Task still present.)
if not exist "C:\ProgramData\DiagnosticsCache" (echo   [OK] DiagnosticsCache gone.) else (echo   [!] DiagnosticsCache still present.)

echo.
choice /C YN /N /M "Reboot now to finalize the MoveFileEx ? [Y]es / [N]o : "
if errorlevel 2 (
  echo.
  echo Reminder : files scheduled for delete-on-reboot will only be
  echo removed after the next restart.
  pause
  exit /b 0
)

echo.
echo Rebooting in 5 seconds...
shutdown /r /t 5 /c "PurrPause reset : reboot to finalize MoveFileEx"
exit /b 0
