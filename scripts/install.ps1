#Requires -Version 5.1
<#
.SYNOPSIS
    Instalador de un click de sysopt para Windows.

.DESCRIPTION
    1. Descarga el binario prebuilt de la última release de GitHub (rápido,
       no necesita Rust ni Visual Studio).
    2. Si no hay binario precompilado x86_64 (o usás -FromSource),
       puede instalar Rust con el toolchain GNU y compilar sin Visual Studio.
       En ARM64 se exige el binario nativo publicado o un toolchain C/Visual Studio
       ya disponible; el instalador no oculta esa dependencia ni emula x86_64.
    3. Instala el binario en tu carpeta de usuario y lo agrega al PATH.
    4. Por defecto, crea una Tarea Programada de usuario que corre
       "sysopt --apply --auto --profile smart" al iniciar sesión. No requiere
       elevar el instalador ni responder un prompt de UAC. Pasá -NoAutoStart
       si no la querés.

    "sysopt --apply --auto --profile smart" sí aplica cambios reales de prioridad de proceso; es lo
    que hace la Tarea Programada instalada por defecto. Si preferís revisarlo
    antes en modo dry-run, instalá con -NoAutoStart y corré "sysopt" a mano.

.EXAMPLE
    # Descargá sysopt-install-windows.ps1 y su .sha256 desde una release,
    # verificá el hash y luego ejecutá:
    .\sysopt-install-windows.ps1

.EXAMPLE
    .\install.ps1 -NoAutoStart
#>
[CmdletBinding()]
param(
    [string]$Repo = $(if ($env:SYSOPT_REPO) { $env:SYSOPT_REPO } else { "" }),
    [string]$Version = "latest",
    [string]$Prefix = "$env:LOCALAPPDATA\Programs\SysOpt",
    [string]$OfflineAssetDir = $(if ($env:SYSOPT_OFFLINE_ASSET_DIR) { $env:SYSOPT_OFFLINE_ASSET_DIR } else { "" }),
    [ValidateRange(5, 900)][int]$TimeoutSec = $(if ($env:SYSOPT_DOWNLOAD_TIMEOUT) { [int]$env:SYSOPT_DOWNLOAD_TIMEOUT } else { 300 }),
    [switch]$RequireAttestation,
    [switch]$RequireCodeSignature,
    [switch]$FromSource,
    [switch]$InstallTask,
    [switch]$NoAutoStart
)

$ErrorActionPreference = "Stop"
$RustToolchain = "1.97.1"

function Write-Info($msg)  { Write-Host "[sysopt-install] $msg" -ForegroundColor Cyan }
function Write-Warn2($msg) { Write-Host "[sysopt-install] $msg" -ForegroundColor Yellow }
function Write-Err2($msg)  { Write-Host "[sysopt-install] $msg" -ForegroundColor Red }

function Test-Truthy([string]$Value) {
    return $Value -match '^(?i:1|true|yes|on)$'
}
if (Test-Truthy $env:SYSOPT_REQUIRE_ATTESTATION) { $RequireAttestation = $true }
if (Test-Truthy $env:SYSOPT_REQUIRE_CODE_SIGNATURE) { $RequireCodeSignature = $true }
if ($FromSource -and ($RequireAttestation -or $RequireCodeSignature)) {
    throw '-FromSource no puede combinarse con requisitos de firma/attestation del binario publicado.'
}

if (-not [string]::IsNullOrWhiteSpace($Repo) -and $Repo -notmatch '^[^/\s]+/[^/\s]+$') {
    throw "Repositorio inválido '$Repo'; usá el formato owner/repo."
}

if (-not [string]::IsNullOrWhiteSpace($OfflineAssetDir)) {
    $OfflineAssetDir = [IO.Path]::GetFullPath($OfflineAssetDir)
    if (-not (Test-Path -LiteralPath $OfflineAssetDir -PathType Container)) {
        throw "El directorio offline no existe: $OfflineAssetDir"
    }
    $offlineItem = Get-Item -LiteralPath $OfflineAssetDir -Force
    if (($offlineItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "El directorio offline no puede ser un enlace: $OfflineAssetDir"
    }
}

function Assert-SafeDirectory([string]$Path, [string]$Purpose) {
    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $forbidden = @(
        [IO.Path]::GetPathRoot($full).TrimEnd('\'),
        ([IO.Path]::GetFullPath($env:USERPROFILE)).TrimEnd('\'),
        ([IO.Path]::GetFullPath($env:LOCALAPPDATA)).TrimEnd('\'),
        ([IO.Path]::GetFullPath($env:APPDATA)).TrimEnd('\')
    )
    if ([string]::IsNullOrWhiteSpace($full) -or $forbidden -contains $full) {
        throw "Ruta insegura para $Purpose`: $Path"
    }
    if (Test-Path -LiteralPath $full) {
        $item = Get-Item -LiteralPath $full -Force
        if (-not $item.PSIsContainer -or (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
            throw "Ruta enlazada o no-directorio rechazada para $Purpose`: $full"
        }
    }
    return $full
}
$script:ResolvedSourceRef = $null
function Resolve-SourceRef {
    if ($script:ResolvedSourceRef) { return $script:ResolvedSourceRef }
    if ($Version -ne 'latest') {
        $script:ResolvedSourceRef = $Version
    } else {
        if ([string]::IsNullOrWhiteSpace($Repo)) { throw 'No se configuró un repositorio para resolver la última release.' }
        Write-Info "Resolviendo el tag exacto de la última release..."
        $release = Invoke-RestMethod -UseBasicParsing -TimeoutSec $TimeoutSec -Uri "https://api.github.com/repos/$Repo/releases/latest"
        $script:ResolvedSourceRef = [string]$release.tag_name
    }
    if ([string]::IsNullOrWhiteSpace($script:ResolvedSourceRef) -or $script:ResolvedSourceRef -eq 'latest' -or $script:ResolvedSourceRef -notmatch '^[A-Za-z0-9._/-]+$') {
        throw "GitHub no devolvió un tag de release válido."
    }
    return $script:ResolvedSourceRef
}

function Get-ReleaseBaseUrl {
    $ref = Resolve-SourceRef
    return "https://github.com/$Repo/releases/download/$ref"
}

function Get-AssetFile([string]$AssetName, [string]$Destination) {
    if (-not [string]::IsNullOrWhiteSpace($OfflineAssetDir)) {
        $source = Join-Path $OfflineAssetDir $AssetName
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Falta el asset offline: $AssetName" }
        $item = Get-Item -LiteralPath $source -Force
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Se rechazó un asset offline enlazado: $source"
        }
        Copy-Item -LiteralPath $source -Destination $Destination -Force
        return
    }
    if ([string]::IsNullOrWhiteSpace($Repo)) { throw 'No se configuró un repositorio.' }
    $base = Get-ReleaseBaseUrl
    Invoke-WebRequest -UseBasicParsing -TimeoutSec $TimeoutSec -Uri "$base/$AssetName" -OutFile $Destination -ErrorAction Stop
}

function Verify-ArtifactAttestation([string]$File, [string]$AssetName) {
    if (-not $RequireAttestation) { return }
    if ([string]::IsNullOrWhiteSpace($Repo)) { throw 'La verificación de attestation exige -Repo owner/repo.' }
    $gh = Get-Command gh -ErrorAction SilentlyContinue
    if (-not $gh) { throw '-RequireAttestation exige GitHub CLI (gh).' }
    if (-not [string]::IsNullOrWhiteSpace($OfflineAssetDir)) {
        $bundle = Join-Path $OfflineAssetDir "$AssetName.sigstore.json"
        if (-not (Test-Path -LiteralPath $bundle -PathType Leaf)) {
            $bundle = Join-Path $OfflineAssetDir 'sysopt-provenance.sigstore.json'
        }
        $trustedRoot = Join-Path $OfflineAssetDir 'trusted_root.jsonl'
        foreach ($path in @($bundle, $trustedRoot)) {
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Falta evidencia offline: $path" }
            $item = Get-Item -LiteralPath $path -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Evidencia enlazada rechazada: $path" }
        }
        & $gh.Source attestation verify $File --repo $Repo --bundle $bundle --custom-trusted-root $trustedRoot | Out-Null
    } else {
        $ref = Resolve-SourceRef
        & $gh.Source attestation verify $File --repo $Repo --source-ref "refs/tags/$ref" | Out-Null
    }
    if ($LASTEXITCODE -ne 0) { throw "Falló la verificación de attestation para $AssetName." }
    Write-Info "Attestation verificada: $AssetName"
}

function Verify-NativeCodeSignature([string]$File, [bool]$RequiredForAsset) {
    if (-not $RequireCodeSignature -or -not $RequiredForAsset) { return }
    $signature = Get-AuthenticodeSignature -LiteralPath $File
    if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
        throw "Firma Authenticode inválida: $($signature.Status)"
    }
    Write-Info "Firma Authenticode verificada: $(Split-Path -Leaf $File)"
}

function Get-VerifiedReleaseAsset([string]$AssetName, [string]$Destination, [bool]$RequireNativeSignature = $false) {
    if ([string]::IsNullOrWhiteSpace($Repo) -and [string]::IsNullOrWhiteSpace($OfflineAssetDir)) { return $false }
    $tmp = Join-Path $env:TEMP ("sysopt-asset-" + [System.Guid]::NewGuid())
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        $assetPath = Join-Path $tmp $AssetName
        $checksumPath = "$assetPath.sha256"
        Get-AssetFile $AssetName $assetPath
        Get-AssetFile "$AssetName.sha256" $checksumPath
        $expected = ((Get-Content $checksumPath -Raw) -split '\s+')[0]
        if ($expected -notmatch '^[0-9a-fA-F]{64}$') { throw 'Formato de checksum inválido.' }
        $actual = (Get-FileHash -Algorithm SHA256 $assetPath).Hash
        if ($actual.ToLowerInvariant() -ne $expected.ToLowerInvariant()) {
            throw "Checksum incorrecto para $AssetName."
        }
        Verify-ArtifactAttestation $assetPath $AssetName
        Verify-NativeCodeSignature $assetPath $RequireNativeSignature
        if ([IO.Path]::GetFullPath($Destination) -eq [IO.Path]::GetFullPath($BinPath)) {
            Stop-ExistingSysOpt
        }
        Install-FileAtomically $assetPath $Destination
        return $true
    } catch {
        Write-Warn2 "No se pudo obtener y verificar $AssetName`: $_"
        return $false
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

# El arranque automático ahora es el comportamiento por defecto; -InstallTask
# se mantiene aceptada por compatibilidad (ya no cambia nada) y -NoAutoStart
# es la forma de desactivarlo.
$SetupAutostart = -not $NoAutoStart

# El arranque automático se registra en el contexto del usuario y no requiere
# privilegios administrativos. Las acciones no permitidas por el SO se omiten
# de forma segura por el motor de enforcement.

# ---- Detección de arquitectura ------------------------------------------
$archRaw = $env:PROCESSOR_ARCHITECTURE
$Arch = switch -Regex ($archRaw) {
    "AMD64" { "x86_64" }
    "ARM64" { "arm64" }
    default { "x86_64" }
}
$AssetName = "sysopt-windows-$Arch.exe"
Write-Info "Plataforma detectada: windows/$Arch"

$Prefix = Assert-SafeDirectory $Prefix 'instalación'
New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
$Prefix = Assert-SafeDirectory $Prefix 'instalación'
$BinPath = Join-Path $Prefix "sysopt.exe"

function Stop-ExistingSysOpt {
    if (Test-Path -LiteralPath $BinPath) {
        try { & $BinPath --shutdown | Out-Null } catch {}
    }
    foreach ($taskName in @('sysopt', 'SysOpt')) {
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    }
    if (-not (Test-Path -LiteralPath $BinPath)) { return }

    # Windows no permite reemplazar de forma fiable un ejecutable que sigue
    # mapeado. Se espera de manera acotada y se falla conservadoramente en vez
    # de borrar o dejar un binario parcial.
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        try {
            $stream = [IO.File]::Open($BinPath, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
            $stream.Dispose()
            return
        } catch {
            Start-Sleep -Milliseconds 250
        }
    }
    throw "La instancia anterior de SysOpt no liberó $BinPath después de 10 segundos."
}

function Install-FileAtomically([string]$Source, [string]$Destination) {
    $parent = Split-Path -Parent $Destination
    if (-not (Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    $candidate = Join-Path $parent ((Split-Path -Leaf $Destination) + '.new-' + [System.Guid]::NewGuid().ToString('N'))
    try {
        Copy-Item -LiteralPath $Source -Destination $candidate -Force
        $sourceHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $Source).Hash
        $candidateHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $candidate).Hash
        if ($sourceHash -ne $candidateHash) { throw "La copia temporal de $Destination no coincide con el origen." }
        if (Test-Path -LiteralPath $Destination) {
            [IO.File]::Replace($candidate, $Destination, $null, $true)
        } else {
            [IO.File]::Move($candidate, $Destination)
        }
    } finally {
        Remove-Item -LiteralPath $candidate -Force -ErrorAction SilentlyContinue
    }
}

# ---- Paso 1: binario prebuilt desde GitHub Releases ---------------------
function Try-DownloadPrebuilt {
    if ($FromSource) { return $false }
    if ([string]::IsNullOrWhiteSpace($Repo) -and [string]::IsNullOrWhiteSpace($OfflineAssetDir)) {
        Write-Warn2 "SYSOPT_REPO no configurado; salteando descarga de binario prebuilt."
        return $false
    }
    Write-Info "Buscando binario prebuilt: $AssetName ($Version)..."
    if (-not (Get-VerifiedReleaseAsset $AssetName $BinPath $true)) {
        Write-Warn2 'No se encontró o no superó la verificación el binario prebuilt.'
        return $false
    }
    Write-Info "Binario prebuilt instalado atómicamente en $BinPath"
    return $true
}

# ---- Paso 2: fallback — instalar Rust (toolchain GNU) y compilar --------
function Ensure-Rust {
    if (Get-Command cargo -ErrorAction SilentlyContinue) {
        if (Get-Command rustup -ErrorAction SilentlyContinue) {
            rustup toolchain install $RustToolchain --profile minimal -q
            if ($LASTEXITCODE -ne 0) { throw "No se pudo instalar Rust $RustToolchain." }
        }
        Write-Info "Rust ya está instalado ($(cargo --version))."
        return
    }

    if ($Arch -eq "arm64") {
        throw "La compilación automática desde fuente en Windows ARM64 requiere un toolchain C/Visual Studio ARM64 existente. Usa el binario prebuilt firmado por checksum o instala ese toolchain antes de pasar -FromSource."
    }

    if (-not [string]::IsNullOrWhiteSpace($OfflineAssetDir)) {
        throw 'Rust no está instalado y el modo offline prohíbe descargar rustup.'
    }
    Write-Info "Rust no está instalado. Instalando con rustup (toolchain GNU, sin Visual Studio)..."
    $rustupInit = Join-Path $env:TEMP "rustup-init.exe"
    $rustupUrl = "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"
    $rustupSha = "$rustupInit.sha256"
    Invoke-WebRequest -UseBasicParsing -TimeoutSec $TimeoutSec -Uri $rustupUrl -OutFile $rustupInit
    Invoke-WebRequest -UseBasicParsing -TimeoutSec $TimeoutSec -Uri "$rustupUrl.sha256" -OutFile $rustupSha
    $expected = ((Get-Content $rustupSha -Raw) -split '\s+')[0]
    if ($expected -notmatch '^[0-9a-fA-F]{64}$') { throw "Checksum oficial de rustup-init inválido." }
    $actual = (Get-FileHash -Algorithm SHA256 $rustupInit).Hash
    if ($actual.ToLowerInvariant() -ne $expected.ToLowerInvariant()) {
        throw "El checksum de rustup-init no coincide."
    }

    # Toolchain GNU: rustup instala el host GNU solicitado y el proyecto fija
    # Rust 1.97.1 para coincidir con el MSRV de sysinfo 0.39.
    & $rustupInit -y --default-host x86_64-pc-windows-gnu --profile minimal --default-toolchain $RustToolchain -q
    if ($LASTEXITCODE -ne 0) { throw "rustup-init falló con código $LASTEXITCODE" }
    Remove-Item -Force $rustupInit, $rustupSha -ErrorAction SilentlyContinue

    $cargoBin = "$env:USERPROFILE\.cargo\bin"
    $env:Path = "$cargoBin;$env:Path"
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "La instalación de Rust no terminó correctamente. Abrí una terminal nueva e intentá de nuevo."
    }
    Write-Info "Rust instalado ($(cargo --version))."
}

function Build-FromSource {
    Ensure-Rust

    $srcDir = $null
    if ((Test-Path "Cargo.toml") -and (Test-Path "crates/app")) {
        $srcDir = (Resolve-Path ".").Path
        Write-Info "Compilando desde el checkout local en $srcDir..."
    } else {
        if (-not [string]::IsNullOrWhiteSpace($OfflineAssetDir)) {
            throw 'El modo offline solo puede compilar desde un checkout local.'
        }
        if ([string]::IsNullOrWhiteSpace($Repo)) {
            throw "No estoy dentro del repo y -Repo/SYSOPT_REPO no está configurado."
        }
        if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
            if (Get-Command winget -ErrorAction SilentlyContinue) {
                Write-Info "Git no está instalado; instalando con winget..."
                winget install --id Git.Git -e --silent --accept-source-agreements --accept-package-agreements
                $env:Path = "$env:ProgramFiles\Git\cmd;$env:Path"
            }
            if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
                throw "Necesito 'git' para clonar el repositorio y no pude instalarlo automáticamente. Instalá Git para Windows y reintentá."
            }
        }
        $srcDir = Join-Path $env:TEMP ("sysopt-src-" + [System.Guid]::NewGuid())
        $sourceRef = Resolve-SourceRef
        Write-Info "Clonando $Repo en la release $sourceRef..."
        git -c http.lowSpeedLimit=1024 -c http.lowSpeedTime=30 clone --depth 1 --single-branch --branch $sourceRef "https://github.com/$Repo.git" $srcDir | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "No se pudo clonar la release fijada $sourceRef de $Repo." }
    }

    if (-not (Test-Path (Join-Path $srcDir "Cargo.lock"))) {
        throw "La fuente no incluye Cargo.lock; se rechaza una compilación no reproducible."
    }
    Write-Info "Compilando en modo release con dependencias bloqueadas..."
    Push-Location $srcDir
    try {
        if (Get-Command rustup -ErrorAction SilentlyContinue) {
            cargo "+$RustToolchain" build --locked --release --package sysopt
        } else {
            cargo build --locked --release --package sysopt
        }
        if ($LASTEXITCODE -ne 0) { throw "cargo build falló con código $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
    $builtBinary = Join-Path $srcDir "target\release\sysopt.exe"
    Stop-ExistingSysOpt
    Install-FileAtomically $builtBinary $BinPath
    Write-Info "Binario compilado e instalado en $BinPath"
}

if (-not (Try-DownloadPrebuilt)) {
    if ($RequireAttestation -or $RequireCodeSignature) {
        throw 'No se instalará desde fuente porque se solicitó verificar el artefacto publicado.'
    }
    Build-FromSource
}

# ---- Paso 3: agregar $Prefix al PATH del usuario ------------------------
function Ensure-Path {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$Prefix*") {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$Prefix", "User")
        Write-Info "Agregado $Prefix al PATH del usuario (abrí una terminal nueva para que tenga efecto)."
    }
    if ($env:Path -notlike "*$Prefix*") { $env:Path = "$env:Path;$Prefix" }
}
Ensure-Path

# ---- Paso 4: configuración inicial, Control Center y desinstalador -------
$ConfigDir = Join-Path $env:APPDATA 'sysopt'
$ConfigDir = Assert-SafeDirectory $ConfigDir 'configuración'
$ConfigFile = Join-Path $ConfigDir 'config.toml'
New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
$ConfigDir = Assert-SafeDirectory $ConfigDir 'configuración'
if (Test-Path -LiteralPath $ConfigFile) {
    $configItem = Get-Item -LiteralPath $ConfigFile -Force
    if (($configItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $configItem.PSIsContainer) {
        throw "Archivo de configuración enlazado o inválido: $ConfigFile"
    }
}
if (-not (Test-Path -LiteralPath $ConfigFile)) {
@'
apply = true

[automation]
enabled = true
profile = "smart"

[intelligence]
enabled = true
persist_state = true
semantic_enabled = true
semantic_auto_download = true
semantic_retry_initial_secs = 30
semantic_retry_max_secs = 3600
semantic_download_timeout_secs = 300
semantic_model_id = "auto"

[cache]
enabled = true
adaptive_budget = true

[resources]
enabled = false

[runtime]
enabled = true
'@ | Set-Content -Encoding UTF8 $ConfigFile
    Write-Info "Configuración inicial creada en $ConfigFile"
}

# No descargamos la IA dentro del instalador: hf-hub 0.4.x no expone un
# timeout de solicitud configurable y una red defectuosa podría bloquear el
# proceso. El servicio arranca con reglas seguras y descarga/valida el modelo
# automáticamente en segundo plano con reintentos exponenciales.
Write-Info "La IA se descargará y validará automáticamente en segundo plano al iniciar SysOpt."

$ControlPath = Join-Path $Prefix 'control-center.ps1'
$UninstallPath = Join-Path $Prefix 'uninstall.ps1'
$repoRoot = if ($PSScriptRoot) { Split-Path -Parent $PSScriptRoot } else { $null }
$localControl = if ($repoRoot) { Join-Path $repoRoot 'packaging\windows\control-center.ps1' } else { $null }
$localUninstall = if ($PSScriptRoot) { Join-Path $PSScriptRoot 'uninstall.ps1' } else { $null }

if ($localControl -and (Test-Path $localControl)) {
    Install-FileAtomically $localControl $ControlPath
} elseif (-not [string]::IsNullOrWhiteSpace($Repo)) {
    [void](Get-VerifiedReleaseAsset "sysopt-control-windows-$Arch.ps1" $ControlPath)
}
if ($localUninstall -and (Test-Path $localUninstall)) {
    Install-FileAtomically $localUninstall $UninstallPath
} elseif (-not [string]::IsNullOrWhiteSpace($Repo)) {
    [void](Get-VerifiedReleaseAsset "sysopt-uninstall-windows-$Arch.ps1" $UninstallPath)
}

if (Test-Path $ControlPath) {
    $startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\SysOpt'
    New-Item -ItemType Directory -Force -Path $startMenu | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut((Join-Path $startMenu 'SysOpt Control Center.lnk'))
    $shortcut.TargetPath = 'powershell.exe'
    $shortcut.Arguments = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$ControlPath`" -BinPath `"$BinPath`""
    $shortcut.WorkingDirectory = $Prefix
    $shortcut.Save()
    if (Test-Path $UninstallPath) {
        $uninstallShortcut = $shell.CreateShortcut((Join-Path $startMenu 'Desinstalar SysOpt.lnk'))
        $uninstallShortcut.TargetPath = 'powershell.exe'
        $uninstallShortcut.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$UninstallPath`" -Prefix `"$Prefix`""
        $uninstallShortcut.WorkingDirectory = $Prefix
        $uninstallShortcut.Save()
    }
    Write-Info "SysOpt Control Center agregado al menú Inicio."
}

# ---- Paso 5: Tarea Programada al iniciar sesión (por defecto) -----------
function Remove-Autostart {
    foreach ($taskName in @('sysopt', 'SysOpt')) {
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    }
    $startup = [Environment]::GetFolderPath('Startup')
    Remove-Item -LiteralPath (Join-Path $startup 'SysOpt.lnk') -Force -ErrorAction SilentlyContinue
}

function Setup-ScheduledTask {
    Write-Info "Creando arranque automático de usuario para SysOpt..."
    $arguments = "--config `"$ConfigFile`" --apply --auto --profile smart"
    try {
        Remove-Autostart
        $action = New-ScheduledTaskAction -Execute $BinPath -Argument $arguments
        $trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
        $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -RunLevel Limited -LogonType Interactive
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -Priority 7 -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)

        Register-ScheduledTask -TaskName "sysopt" -Action $action -Trigger $trigger `
            -Principal $principal -Settings $settings -Force | Out-Null
        Start-ScheduledTask -TaskName "sysopt" -ErrorAction SilentlyContinue
        Write-Info "Tarea Programada de usuario creada e iniciada sin elevación."
    } catch {
        Write-Warn2 "La Tarea Programada no estuvo disponible; usando la carpeta Inicio como fallback: $_"
        $startup = [Environment]::GetFolderPath('Startup')
        $shell = New-Object -ComObject WScript.Shell
        $shortcut = $shell.CreateShortcut((Join-Path $startup 'SysOpt.lnk'))
        $shortcut.TargetPath = $BinPath
        $shortcut.Arguments = $arguments
        $shortcut.WorkingDirectory = $Prefix
        $shortcut.Save()
        Start-Process -FilePath $BinPath -ArgumentList $arguments -WindowStyle Hidden
        Write-Info "Acceso de inicio automático creado e instancia iniciada."
    }
}
if ($SetupAutostart) {
    Setup-ScheduledTask
} else {
    Remove-Autostart
}

# ---- Resumen -------------------------------------------------------------
Write-Host ""
Write-Info "¡Listo! sysopt instalado en: $BinPath"
Write-Info "Probalo con:   sysopt --help"
Write-Info "Modo dry-run:  sysopt"
Write-Info "Modo real:     sysopt --apply --auto --profile smart"
if (Test-Path $ControlPath) { Write-Info "Control Center disponible en el menú Inicio." }
if ($SetupAutostart) {
    Write-Info "Arranque automático activo: sysopt ya corre solo en modo --apply desde el próximo inicio de sesión."
} else {
    Write-Info "Arranque automático desactivado (-NoAutoStart). Volvé a correr el instalador sin esa flag para activarlo."
}
