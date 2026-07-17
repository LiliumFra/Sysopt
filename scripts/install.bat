@echo off
rem sysopt — doble click para instalar en Windows.
rem Este .bat solo delega en install.ps1 con la política de ejecución
rem necesaria para esta sola corrida (no cambia tu configuración global).
setlocal
set SCRIPT_DIR=%~dp0
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT_DIR%install.ps1" %*
pause
