#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Prefix = "$env:LOCALAPPDATA\Programs\SysOpt",
    [switch]$RemoveData
)
$ErrorActionPreference = 'SilentlyContinue'

# Evita que un valor Prefix vacío, raíz o directorio de perfil convierta la
# desinstalación recursiva en una eliminación amplia accidental.
$fullPrefix = [IO.Path]::GetFullPath($Prefix).TrimEnd('\')
$forbidden = @(
    [IO.Path]::GetPathRoot($fullPrefix).TrimEnd('\'),
    ([IO.Path]::GetFullPath($env:USERPROFILE)).TrimEnd('\'),
    ([IO.Path]::GetFullPath($env:LOCALAPPDATA)).TrimEnd('\'),
    ([IO.Path]::GetFullPath($env:APPDATA)).TrimEnd('\')
)
if ([string]::IsNullOrWhiteSpace($fullPrefix) -or $forbidden -contains $fullPrefix) {
    throw "Prefijo de desinstalación inseguro: $Prefix"
}
$Prefix = $fullPrefix
$bin = Join-Path $Prefix 'sysopt.exe'
if (Test-Path $bin) { & $bin --shutdown | Out-Null }
Stop-ScheduledTask -TaskName 'sysopt' -ErrorAction SilentlyContinue
Stop-ScheduledTask -TaskName 'SysOpt' -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName 'sysopt' -Confirm:$false -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName 'SysOpt' -Confirm:$false -ErrorAction SilentlyContinue

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath) {
    $parts = $userPath.Split(';') | Where-Object { $_ -and $_.TrimEnd('\\') -ne $Prefix.TrimEnd('\\') }
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}
Remove-Item -Recurse -Force $Prefix -ErrorAction SilentlyContinue
$startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\SysOpt'
Remove-Item -Recurse -Force $startMenu -ErrorAction SilentlyContinue
if ($RemoveData) {
    Remove-Item -Recurse -Force (Join-Path $env:APPDATA 'sysopt') -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA 'sysopt') -ErrorAction SilentlyContinue
}
Write-Host 'SysOpt desinstalado.'
