#!/usr/bin/env bash
# sysopt — instalador de un click para Linux y macOS.
#
# Uso recomendado:
#   1. Descargá sysopt-install-unix.sh y su .sha256 desde una release.
#   2. Verificá SHA-256.
#   3. Ejecutá: chmod +x sysopt-install-unix.sh && ./sysopt-install-unix.sh
#
# En un checkout local también podés ejecutar este archivo directamente.
#
# Qué hace:
#   1. Detecta tu SO/arquitectura y descarga el binario prebuilt de la
#      última release en GitHub (no necesita Rust ni nada más).
#   2. Si no hay un binario prebuilt para tu plataforma (o pasás
#      --from-source), instala Rust (rustup) y compila desde el código
#      fuente automáticamente — sin pasos manuales.
#   3. Instala el binario en tu PATH y deja todo funcionando solo:
#      registra el arranque automático al iniciar sesión (systemd --user
#      en Linux, LaunchAgent en macOS) corriendo "sysopt --apply --auto --profile smart" — nada
#      que activar a mano. Pasá --no-service si no lo querés.
#
# El único cambio real al sistema que hace este instalador por sí solo es
# justamente ese: dejar sysopt corriendo en modo --apply en segundo plano.
# Si preferís revisarlo primero en modo dry-run, instalá con --no-service
# y corré "sysopt" manualmente.
set -euo pipefail
umask 077

# ---- Configuración ---------------------------------------------------
# La distribución publicada puede completar este valor. En un checkout local
# no hace falta: el instalador compila esa fuente. Para releases remotas usá
# --repo owner/repo o la variable SYSOPT_REPO.
DEFAULT_REPO=""
RUST_TOOLCHAIN="1.97.1"

REPO="${SYSOPT_REPO:-$DEFAULT_REPO}"
VERSION="latest"
PREFIX="${SYSOPT_PREFIX:-$HOME/.local/bin}"
FORCE_SOURCE=0
SETUP_SERVICE=1
GRANT_CAP_SYS_NICE=0
SERVICE_ACTIVE=0
SOURCE_REF=""
OFFLINE_ASSET_DIR="${SYSOPT_OFFLINE_ASSET_DIR:-}"
DOWNLOAD_TIMEOUT="${SYSOPT_DOWNLOAD_TIMEOUT:-300}"
REQUIRE_ATTESTATION="${SYSOPT_REQUIRE_ATTESTATION:-0}"
REQUIRE_CODE_SIGNATURE="${SYSOPT_REQUIRE_CODE_SIGNATURE:-0}"

# ---- Utilidades --------------------------------------------------------
log()  { printf '\033[1;34m[sysopt-install]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[sysopt-install]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31m[sysopt-install]\033[0m %s\n' "$*" >&2; }
die()  { err "$*"; exit 1; }

normalize_bool() {
  case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
    1|true|yes|on) printf '1' ;;
    0|false|no|off|'') printf '0' ;;
    *) die "Valor booleano inválido: $1" ;;
  esac
}

lower_hex() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

reject_multiline_path() {
  case "$1" in
    *$'\n'*|*$'\r'*) die "La ruta contiene saltos de línea y se rechaza por seguridad: $1" ;;
  esac
}

xml_escape() {
  local value="$1"
  value="${value//&/\&amp;}"
  value="${value//</\&lt;}"
  value="${value//>/\&gt;}"
  value="${value//\"/\&quot;}"
  value="${value//\'/\&apos;}"
  printf '%s' "$value"
}

systemd_quote_arg() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//%/%%}"
  value="${value//\$/\$\$}"
  printf '"%s"' "$value"
}

desktop_quote_exec() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//\`/\\\`}"
  value="${value//\$/\\\$}"
  value="${value//%/%%}"
  printf '"%s"' "$value"
}

print_help() {
  cat <<'HELP'
Uso: install.sh [opciones]

  --repo <owner/repo>     Repositorio de GitHub del que descargar releases
                          (default: variable SYSOPT_REPO o el valor embebido)
  --version <tag>         Tag de release a instalar (default: latest)
  --prefix <ruta>         Carpeta de instalación (default: ~/.local/bin)
  --from-source           Fuerza compilar desde código fuente (ignora binarios prebuilt)
  --offline-asset-dir <d> Instala exclusivamente desde assets locales verificados
  --timeout <segundos>    Timeout de red entre 5 y 900 s (default: 300)
  --require-attestation   Exige attestation GitHub/Sigstore válida del artefacto
  --require-code-signature Exige firma nativa válida (macOS; en Linux no aplica)
  --no-service            No instalar el arranque automático (por defecto SÍ se instala:
                          systemd --user en Linux, LaunchAgent en macOS, corriendo
                          "sysopt --apply --auto --profile smart" al iniciar sesión)
  --service               Acepta la flag por compatibilidad; no hace falta pasarla,
                          el arranque automático ya se instala por defecto
  --grant-cap-sys-nice    (Linux, requiere sudo) Instala un helper mínimo, propiedad
                          de root y con CAP_SYS_NICE. El proceso principal sigue
                          ejecutándose sin privilegios
  -h, --help              Muestra esta ayuda
HELP
}

while [ $# -gt 0 ]; do
  case "$1" in
    --repo)
      [ $# -ge 2 ] || die "--repo requiere un valor owner/repo"
      REPO="$2"; shift 2 ;;
    --version)
      [ $# -ge 2 ] || die "--version requiere un tag"
      VERSION="$2"; shift 2 ;;
    --prefix)
      [ $# -ge 2 ] || die "--prefix requiere una ruta"
      PREFIX="$2"; shift 2 ;;
    --from-source) FORCE_SOURCE=1; shift ;;
    --offline-asset-dir)
      [ $# -ge 2 ] || die "--offline-asset-dir requiere una ruta"
      OFFLINE_ASSET_DIR="$2"; shift 2 ;;
    --timeout)
      [ $# -ge 2 ] || die "--timeout requiere segundos"
      DOWNLOAD_TIMEOUT="$2"; shift 2 ;;
    --require-attestation) REQUIRE_ATTESTATION=1; shift ;;
    --require-code-signature) REQUIRE_CODE_SIGNATURE=1; shift ;;
    --service) SETUP_SERVICE=1; shift ;;
    --no-service) SETUP_SERVICE=0; shift ;;
    --grant-cap-sys-nice) GRANT_CAP_SYS_NICE=1; shift ;;
    -h|--help) print_help; exit 0 ;;
    *) die "Opción desconocida: $1 (usá --help)" ;;
  esac
done

[ -n "$PREFIX" ] || die "--prefix no puede estar vacío"
reject_multiline_path "$PREFIX"
reject_multiline_path "$HOME"
if [ -n "$REPO" ] && [[ ! "$REPO" =~ ^[^/]+/[^/]+$ ]]; then
  die "Repositorio inválido '$REPO'; usá el formato owner/repo"
fi

REQUIRE_ATTESTATION="$(normalize_bool "$REQUIRE_ATTESTATION")"
REQUIRE_CODE_SIGNATURE="$(normalize_bool "$REQUIRE_CODE_SIGNATURE")"
case "$DOWNLOAD_TIMEOUT" in
  ''|*[!0-9]*) die "--timeout debe ser un entero entre 5 y 900" ;;
esac
[ "$DOWNLOAD_TIMEOUT" -ge 5 ] && [ "$DOWNLOAD_TIMEOUT" -le 900 ]   || die "--timeout debe estar entre 5 y 900 segundos"
if [ -n "$OFFLINE_ASSET_DIR" ]; then
  reject_multiline_path "$OFFLINE_ASSET_DIR"
  [ -d "$OFFLINE_ASSET_DIR" ] && [ ! -L "$OFFLINE_ASSET_DIR" ]     || die "El directorio offline no existe o es un enlace simbólico: $OFFLINE_ASSET_DIR"
  OFFLINE_ASSET_DIR="$(cd "$OFFLINE_ASSET_DIR" && pwd -P)"
fi
if [ "$FORCE_SOURCE" -eq 1 ] && { [ "$REQUIRE_ATTESTATION" -eq 1 ] || [ "$REQUIRE_CODE_SIGNATURE" -eq 1 ]; }; then
  die "--from-source no puede combinarse con requisitos de firma/attestation del binario publicado"
fi

resolve_source_ref() {
  [ -n "$SOURCE_REF" ] && return 0
  if [ "$VERSION" != "latest" ]; then
    SOURCE_REF="$VERSION"
  else
    command -v curl >/dev/null 2>&1 || die "Necesito curl para resolver el tag de la última release."
    local latest_url
    latest_url="$(
      curl -fsSL --retry 3 --connect-timeout 15 --max-time "$DOWNLOAD_TIMEOUT" \
        -o /dev/null -w '%{url_effective}' \
        "https://github.com/${REPO}/releases/latest"
    )" || die "No se pudo resolver la última release de ${REPO}."
    latest_url="${latest_url%/}"
    SOURCE_REF="${latest_url##*/}"
  fi
  [[ "$SOURCE_REF" =~ ^[A-Za-z0-9._/-]+$ ]]     || die "Tag de release inválido: $SOURCE_REF"
  [ "$SOURCE_REF" != "latest" ]     || die "GitHub no devolvió un tag concreto para la última release."
}

# ---- Detección de plataforma -------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  PLATFORM="linux" ;;
  Darwin) PLATFORM="macos" ;;
  *) die "SO no soportado por este instalador: $OS. Usá scripts/install.ps1 en Windows." ;;
esac

if [ "$PLATFORM" = "macos" ]; then
  CONFIG_DIR="$HOME/Library/Application Support/SysOpt"
else
  CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/sysopt"
fi
AI_CONFIG="$CONFIG_DIR/config.toml"

case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  arm64|aarch64) ARCH="arm64" ;;
  *) warn "Arquitectura '$ARCH' no tiene binario prebuilt; se compilará desde código fuente." ; FORCE_SOURCE=1 ;;
esac

ASSET_NAME="sysopt-${PLATFORM}-${ARCH}"
HELPER_ASSET_NAME="sysopt-priority-helper-linux-${ARCH}"
log "Plataforma detectada: ${PLATFORM}/${ARCH}"

mkdir -p "$PREFIX"

release_base_url() {
  resolve_source_ref
  printf 'https://github.com/%s/releases/download/%s' "$REPO" "$SOURCE_REF"
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    return 127
  fi
}

install_file_atomically() {
  local source="$1" destination="$2" mode="${3:-0755}"
  local parent candidate
  parent="$(dirname "$destination")"
  mkdir -p "$parent"
  [ ! -L "$parent" ] || die "El destino contiene un directorio enlazado: $parent"
  candidate="$parent/.sysopt-install-$$-$(basename "$destination").new"
  rm -f "$candidate"
  install -m "$mode" "$source" "$candidate"
  [ "$(lower_hex "$(sha256_file "$source")")" = "$(lower_hex "$(sha256_file "$candidate")")" ] \
    || { rm -f "$candidate"; die "La copia temporal no coincide con el origen: $destination"; }
  mv -f "$candidate" "$destination"
}

fetch_release_asset() {
  local asset="$1" destination="$2"
  if [ -n "$OFFLINE_ASSET_DIR" ]; then
    local source="$OFFLINE_ASSET_DIR/$asset"
    [ -f "$source" ] && [ ! -L "$source" ] || return 1
    cp "$source" "$destination"
    return 0
  fi
  [ -n "$REPO" ] || return 1
  command -v curl >/dev/null 2>&1 || return 1
  local base
  base="$(release_base_url)"
  curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --connect-timeout 15 \
    --max-time "$DOWNLOAD_TIMEOUT" -o "$destination" "$base/$asset"
}

verify_attestation() {
  local file="$1" asset="$2"
  [ "$REQUIRE_ATTESTATION" -eq 1 ] || return 0
  [ -n "$REPO" ] || die "La verificación de attestation exige --repo owner/repo"
  command -v gh >/dev/null 2>&1 \
    || die "--require-attestation exige GitHub CLI (gh) instalado"
  if [ -n "$OFFLINE_ASSET_DIR" ]; then
    local bundle="$OFFLINE_ASSET_DIR/$asset.sigstore.json"
    [ -f "$bundle" ] || bundle="$OFFLINE_ASSET_DIR/sysopt-provenance.sigstore.json"
    local trusted_root="$OFFLINE_ASSET_DIR/trusted_root.jsonl"
    [ -f "$bundle" ] && [ ! -L "$bundle" ] \
      || die "Falta el bundle de attestation offline para $asset"
    [ -f "$trusted_root" ] && [ ! -L "$trusted_root" ] \
      || die "Falta trusted_root.jsonl para verificar offline"
    gh attestation verify "$file" --repo "$REPO" --bundle "$bundle" \
      --custom-trusted-root "$trusted_root" >/dev/null
  else
    resolve_source_ref
    gh attestation verify "$file" --repo "$REPO" \
      --source-ref "refs/tags/$SOURCE_REF" >/dev/null
  fi
  log "Attestation verificada: $asset"
}

verify_native_signature() {
  local file="$1" require_for_asset="${2:-0}"
  [ "$REQUIRE_CODE_SIGNATURE" -eq 1 ] || return 0
  [ "$require_for_asset" -eq 1 ] || return 0
  if [ "$PLATFORM" = "macos" ]; then
    command -v codesign >/dev/null 2>&1 || die "codesign no está disponible"
    codesign --verify --strict --verbose=2 "$file"
    log "Firma nativa verificada: $(basename "$file")"
  else
    die "Linux no posee una firma ejecutable nativa uniforme; usá --require-attestation"
  fi
}

download_verified_release_asset() {
  local asset="$1" destination="$2" mode="${3:-0755}" require_native_signature="${4:-0}"
  local tmp expected actual
  tmp="$(mktemp -d)"
  if ! fetch_release_asset "$asset" "$tmp/$asset" \
      || ! fetch_release_asset "$asset.sha256" "$tmp/$asset.sha256"; then
    rm -rf "$tmp"
    return 1
  fi
  expected="$(awk 'NR == 1 { print $1 }' "$tmp/$asset.sha256")"
  if [[ ! "$expected" =~ ^[0-9a-fA-F]{64}$ ]]; then
    rm -rf "$tmp"
    return 1
  fi
  actual="$(sha256_file "$tmp/$asset")" || { rm -rf "$tmp"; return 1; }
  if [ "$(lower_hex "$actual")" != "$(lower_hex "$expected")" ]; then
    rm -rf "$tmp"
    return 1
  fi
  verify_attestation "$tmp/$asset" "$asset"
  verify_native_signature "$tmp/$asset" "$require_native_signature"
  install_file_atomically "$tmp/$asset" "$destination" "$mode"
  rm -rf "$tmp"
}

# ---- Paso 1: intentar binario prebuilt desde GitHub Releases -----------
try_download_prebuilt() {
  [ "$FORCE_SOURCE" -eq 1 ] && return 1
  if [ -z "$OFFLINE_ASSET_DIR" ] && [ -z "$REPO" ]; then
    warn "SYSOPT_REPO no configurado; salteando descarga de binario prebuilt."
    return 1
  fi
  log "Buscando binario prebuilt: ${ASSET_NAME} (${VERSION})..."
  if ! download_verified_release_asset "$ASSET_NAME" "$PREFIX/sysopt" 0755 1; then
    warn "No se encontró o no superó la verificación el binario prebuilt para tu plataforma."
    return 1
  fi
  if [ "$PLATFORM" = "linux" ]; then
    if download_verified_release_asset "$HELPER_ASSET_NAME" "$PREFIX/sysopt-priority-helper" 0755 0; then
      log "Helper de prioridades verificado e instalado sin capacidades en $PREFIX/sysopt-priority-helper"
    else
      warn "No se pudo instalar el helper de prioridades; SysOpt seguirá funcionando sin elevaciones."
    fi
  fi
  log "Binario prebuilt instalado atómicamente en $PREFIX/sysopt"
  return 0
}

# ---- Paso 2: fallback — instalar Rust (si falta) y compilar ------------
ensure_rust() {
  if command -v cargo >/dev/null 2>&1; then
    if command -v rustup >/dev/null 2>&1; then
      rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal -q
    fi
    log "Rust ya está instalado ($(cargo --version))."
    return 0
  fi

  [ -z "$OFFLINE_ASSET_DIR" ] || die "Rust no está instalado y el modo offline prohíbe descargar rustup"
  log "Rust no está instalado. Instalando con rustup (no interactivo)..."
  command -v curl >/dev/null 2>&1 || die "Necesito 'curl' para instalar Rust y no está disponible. Instalalo manualmente y reintentá."
  local rust_host
  case "${PLATFORM}/${ARCH}" in
    linux/x86_64) rust_host="x86_64-unknown-linux-gnu" ;;
    linux/arm64) rust_host="aarch64-unknown-linux-gnu" ;;
    macos/x86_64) rust_host="x86_64-apple-darwin" ;;
    macos/arm64) rust_host="aarch64-apple-darwin" ;;
    *) die "No hay rustup-init soportado para ${PLATFORM}/${ARCH}." ;;
  esac
  local rustup_tmp rustup_url expected actual
  rustup_tmp="$(mktemp -d)"
  rustup_url="https://static.rust-lang.org/rustup/dist/${rust_host}/rustup-init"
  curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --connect-timeout 15 --max-time "$DOWNLOAD_TIMEOUT" \
    "$rustup_url" -o "$rustup_tmp/rustup-init"
  curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --connect-timeout 15 --max-time "$DOWNLOAD_TIMEOUT" \
    "$rustup_url.sha256" -o "$rustup_tmp/rustup-init.sha256"
  expected="$(awk '{print $1}' "$rustup_tmp/rustup-init.sha256")"
  [[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || die "Checksum oficial de rustup-init inválido."
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$rustup_tmp/rustup-init" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$rustup_tmp/rustup-init" | awk '{print $1}')"
  else
    die "No hay una herramienta SHA-256 disponible para verificar rustup-init."
  fi
  [ "$(lower_hex "$actual")" = "$(lower_hex "$expected")" ] || die "El checksum de rustup-init no coincide."
  chmod +x "$rustup_tmp/rustup-init"
  "$rustup_tmp/rustup-init" -y --profile minimal --default-toolchain "$RUST_TOOLCHAIN" -q
  rm -rf "$rustup_tmp"
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
  log "Rust instalado ($(cargo --version))."
}

ensure_c_toolchain() {
  # Todo target *-linux-gnu necesita un linker de sistema (cc). En macOS,
  # las Command Line Tools de Xcode lo proveen.
  if [ "$PLATFORM" = "macos" ]; then
    if ! xcode-select -p >/dev/null 2>&1; then
      warn "Faltan las Command Line Tools de Xcode (necesarias para compilar)."
      warn "Se abrirá el instalador gráfico de Apple; aceptá y volvé a correr este script."
      xcode-select --install || true
      die "Instalá las Command Line Tools y volvé a ejecutar este instalador."
    fi
    return 0
  fi

  command -v cc >/dev/null 2>&1 && return 0

  warn "No se encontró un compilador de C (cc); intentando instalarlo con el gestor de paquetes del sistema..."
  if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update -qq && sudo apt-get install -y -qq build-essential
  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y gcc
  elif command -v yum >/dev/null 2>&1; then
    sudo yum install -y gcc
  elif command -v pacman >/dev/null 2>&1; then
    sudo pacman -Sy --noconfirm base-devel
  elif command -v apk >/dev/null 2>&1; then
    sudo apk add --no-cache build-base
  elif command -v zypper >/dev/null 2>&1; then
    sudo zypper install -y gcc
  else
    die "No pude detectar tu gestor de paquetes. Instalá manualmente un compilador de C (gcc/clang) y reintentá."
  fi
}

build_from_source() {
  ensure_c_toolchain
  ensure_rust

  local src_dir
  if [ -f "Cargo.toml" ] && [ -d "crates/app" ]; then
    # Ya estamos parados dentro del repo (ej. lo clonaste vos mismo).
    src_dir="$(pwd)"
    log "Compilando desde el checkout local en $src_dir..."
  else
    [ -z "$OFFLINE_ASSET_DIR" ] || die "El modo offline solo puede compilar desde un checkout local"
    [ -z "$REPO" ] && die "No estoy dentro del repo y SYSOPT_REPO no está configurado; pasá --repo owner/repo."
    command -v git >/dev/null 2>&1 || die "Necesito 'git' para clonar el repositorio."
    src_dir="$(mktemp -d)"
    resolve_source_ref
    log "Clonando ${REPO} en la release ${SOURCE_REF}..."
    GIT_TERMINAL_PROMPT=0 git \
      -c http.lowSpeedLimit=1024 -c http.lowSpeedTime=30 \
      clone --depth 1 --single-branch --branch "$SOURCE_REF" \
      "https://github.com/${REPO}.git" "$src_dir" \
      || die "No se pudo clonar la release fijada $SOURCE_REF de $REPO."
  fi

  log "Compilando en modo release (puede tardar unos minutos la primera vez)..."
  [ -f "$src_dir/Cargo.lock" ] || die "La fuente no incluye Cargo.lock; se rechaza una compilación no reproducible."
  if command -v rustup >/dev/null 2>&1; then
    ( cd "$src_dir" && cargo +"$RUST_TOOLCHAIN" build --locked --release --package sysopt )
  else
    ( cd "$src_dir" && cargo build --locked --release --package sysopt )
  fi
  install_file_atomically "$src_dir/target/release/sysopt" "$PREFIX/sysopt" 0755
  if [ "$PLATFORM" = "linux" ] && [ -x "$src_dir/target/release/sysopt-priority-helper" ]; then
    install_file_atomically "$src_dir/target/release/sysopt-priority-helper" "$PREFIX/sysopt-priority-helper" 0755
  fi
  log "Binario compilado e instalado en $PREFIX/sysopt"
}

if ! try_download_prebuilt; then
  if [ "$REQUIRE_ATTESTATION" -eq 1 ] || [ "$REQUIRE_CODE_SIGNATURE" -eq 1 ]; then
    die "No se instalará desde fuente porque se solicitó verificar el artefacto publicado"
  fi
  build_from_source
fi

# ---- Paso 3: asegurar que $PREFIX esté en el PATH ----------------------
ensure_path() {
  case ":$PATH:" in
    *":$PREFIX:"*) return 0 ;;
  esac

  warn "$PREFIX no está en tu PATH todavía."
  local rc_file=""
  case "${SHELL:-}" in
    */zsh)  rc_file="$HOME/.zshrc" ;;
    */bash) rc_file="$HOME/.bashrc" ;;
    *)      rc_file="$HOME/.profile" ;;
  esac

  local path_export_line
  path_export_line="$(printf 'export PATH=%q:$PATH' "$PREFIX")"
  if [ -n "$rc_file" ] && ! grep -Fqs "$path_export_line" "$rc_file" 2>/dev/null; then
    { echo ''; echo "# agregado por el instalador de sysopt"; printf '%s\n' "$path_export_line"; } >> "$rc_file"
    log "Agregado $PREFIX al PATH en $rc_file (abrí una terminal nueva o hacé 'source $rc_file')."
  fi
  export PATH="$PREFIX:$PATH"
}
ensure_path

# ---- Paso 4: configuración inicial y Control Center -------------------
setup_user_files() {
  local config_dir="$CONFIG_DIR"
  [ ! -L "$config_dir" ] || die "La ruta de configuración es un enlace simbólico y se rechaza por seguridad: $config_dir"
  mkdir -p "$config_dir"
  [ -d "$config_dir" ] && [ ! -L "$config_dir" ] || die "Ruta de configuración insegura: $config_dir"
  chmod 700 "$config_dir"
  [ ! -L "$config_dir/config.toml" ] || die "El archivo de configuración es un enlace simbólico y se rechaza por seguridad."
  if [ ! -f "$config_dir/config.toml" ]; then
    cat > "$config_dir/config.toml" <<'EOF'
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
EOF
    chmod 600 "$config_dir/config.toml"
    log "Configuración inicial creada en $config_dir/config.toml"
  fi

  local control_source=""
  if [ -f "scripts/control.sh" ]; then
    control_source="scripts/control.sh"
  elif [ -f "$(dirname "$0")/control.sh" ]; then
    control_source="$(dirname "$0")/control.sh"
  fi
  if [ -n "$control_source" ]; then
    cp "$control_source" "$PREFIX/sysopt-control"
  elif [ -n "$REPO" ]; then
    local control_asset="sysopt-control-${PLATFORM}-${ARCH}"
    download_verified_release_asset "$control_asset" "$PREFIX/sysopt-control" 0755 \
      || warn "No se pudo descargar y verificar el Control Center; el binario quedó instalado igualmente."
  fi
  if [ -f "$PREFIX/sysopt-control" ]; then
    chmod +x "$PREFIX/sysopt-control"
    if [ "$PLATFORM" = "linux" ]; then
      local desktop_dir="$HOME/.local/share/applications"
      mkdir -p "$desktop_dir"
      local desktop_exec
      desktop_exec="$(desktop_quote_exec "$PREFIX/sysopt-control")"
      cat > "$desktop_dir/sysopt-control.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=SysOpt Control Center
Comment=Controlar SysOpt
Exec=$desktop_exec
Icon=utilities-system-monitor
Terminal=true
Categories=System;Utility;
EOF
      chmod +x "$desktop_dir/sysopt-control.desktop"
    else
      mkdir -p "$HOME/Applications"
      ln -sfn "$PREFIX/sysopt-control" "$HOME/Applications/SysOpt Control.command"
    fi
    log "Control Center instalado: $PREFIX/sysopt-control"
  fi

  local uninstall_source=""
  if [ -f "scripts/uninstall.sh" ]; then
    uninstall_source="scripts/uninstall.sh"
  elif [ -f "$(dirname "$0")/uninstall.sh" ]; then
    uninstall_source="$(dirname "$0")/uninstall.sh"
  fi
  if [ -n "$uninstall_source" ]; then
    install -m 0755 "$uninstall_source" "$PREFIX/sysopt-uninstall"
  elif [ -n "$REPO" ]; then
    local uninstall_asset="sysopt-uninstall-${PLATFORM}-${ARCH}"
    download_verified_release_asset "$uninstall_asset" "$PREFIX/sysopt-uninstall" 0755 \
      || warn "No se pudo descargar y verificar el desinstalador; podés borrar SysOpt manualmente."
  fi
}
setup_user_files

# ---- Paso 5: modelo semántico local -----------------------------------
# La descarga no se ejecuta dentro del instalador: hf-hub 0.4.x no expone
# un timeout de solicitud configurable y una red defectuosa podría bloquear
# la instalación. El servicio arranca inmediatamente con reglas seguras y
# descarga/valida el modelo en segundo plano con reintentos automáticos.
log "La IA se descargará y validará automáticamente en segundo plano al iniciar SysOpt."

# ---- Paso 6: arranque automático (systemd --user / launchd) -----------
setup_systemd_service() {
  command -v systemctl >/dev/null 2>&1 || { warn "No se encontró systemd; no se pudo instalar el arranque automático. Corré 'sysopt --apply --auto --profile smart' manualmente o desde tu propio init system."; return; }

  local unit_dir="$HOME/.config/systemd/user"
  local systemd_binary systemd_config
  systemd_binary="$(systemd_quote_arg "$PREFIX/sysopt")"
  systemd_config="$(systemd_quote_arg "$AI_CONFIG")"
  mkdir -p "$unit_dir"
  cat > "$unit_dir/sysopt.service" <<EOF
[Unit]
Description=sysopt - orquestador de prioridades de proceso

[Service]
ExecStart=$systemd_binary --config $systemd_config --apply --auto --profile smart
Restart=on-failure
RestartSec=5
TimeoutStopSec=15
KillSignal=SIGTERM
UMask=0077
Nice=5
IOSchedulingClass=idle

[Install]
WantedBy=default.target
EOF

  systemctl --user daemon-reload || { warn "No se pudo recargar systemd --user (¿sin sesión de bus activa? ej. SSH sin login gráfico). El binario ya está instalado; reintentá esto más tarde con: systemctl --user daemon-reload && systemctl --user enable --now sysopt.service"; return 1; }
  systemctl --user enable sysopt.service || { warn "No se pudo habilitar el servicio automáticamente. Reintentá con: systemctl --user enable sysopt.service"; return 1; }
  systemctl --user restart sysopt.service || { warn "El servicio quedó habilitado, pero no se pudo iniciar/reiniciar. Reintentá con: systemctl --user restart sysopt.service"; return 1; }
  SERVICE_ACTIVE=1
  log "Servicio systemd --user 'sysopt' creado, habilitado y reiniciado con el binario nuevo."
  log "Ver logs con: journalctl --user -u sysopt -f"

  # Sin "linger", el servicio --user se cae en cuanto cerrás sesión (ej. una
  # PC que dejás prendida conectada por SSH sin sesión gráfica activa).
  # loginctl enable-linger lo deja corriendo con el sistema, no con la sesión.
  if command -v loginctl >/dev/null 2>&1; then
    if loginctl enable-linger "$USER" 2>/dev/null; then
      log "Habilitado 'linger': sysopt sigue corriendo aunque cierres sesión."
    else
      warn "No pude habilitar 'linger' automáticamente (puede necesitar sudo/polkit)."
      warn "Para que sysopt siga corriendo sin sesión activa: sudo loginctl enable-linger $USER"
    fi
  fi
}

setup_launchd_agent() {
  command -v launchctl >/dev/null 2>&1 || { warn "No se encontró launchctl; no se pudo instalar el arranque automático."; return; }

  local agents_dir="$HOME/Library/LaunchAgents"
  local plist="$agents_dir/io.sysopt.agent.plist"
  local logs_dir="$HOME/Library/Logs"
  local binary_xml config_xml stdout_xml stderr_xml
  binary_xml="$(xml_escape "$PREFIX/sysopt")"
  config_xml="$(xml_escape "$AI_CONFIG")"
  stdout_xml="$(xml_escape "$logs_dir/sysopt.log")"
  stderr_xml="$(xml_escape "$logs_dir/sysopt.err.log")"
  mkdir -p "$agents_dir" "$logs_dir"

  cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.sysopt.agent</string>
    <key>ProgramArguments</key>
    <array>
        <string>$binary_xml</string>
        <string>--config</string>
        <string>$config_xml</string>
        <string>--apply</string>
        <string>--auto</string>
        <string>--profile</string>
        <string>smart</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>Umask</key>
    <integer>63</integer>
    <key>StandardOutPath</key>
    <string>$stdout_xml</string>
    <key>StandardErrorPath</key>
    <string>$stderr_xml</string>
</dict>
</plist>
EOF

  # Migra el identificador usado por versiones antiguas y evita dos agentes
  # ejecutándose al mismo tiempo. Se usa la interfaz bootstrap/bootout de
  # launchctl; load/unload queda solo como fallback para sistemas antiguos.
  local legacy_plist="$agents_dir/com.sysopt.agent.plist"
  local uid label service_target
  uid="$(id -u)"
  label="io.sysopt.agent"
  service_target="gui/$uid/$label"
  launchctl bootout "$service_target" >/dev/null 2>&1 || true
  launchctl bootout "gui/$uid/com.sysopt.agent" >/dev/null 2>&1 || true
  launchctl unload "$legacy_plist" >/dev/null 2>&1 || true
  rm -f "$legacy_plist"

  if launchctl bootstrap "gui/$uid" "$plist" 2>/dev/null; then
    launchctl enable "$service_target" >/dev/null 2>&1 || true
    launchctl kickstart -k "$service_target" >/dev/null 2>&1 || true
    SERVICE_ACTIVE=1
    log "LaunchAgent '$label' creado y cargado (corre en modo --apply al iniciar sesión)."
    log "Ver logs en: $logs_dir/sysopt.log"
  elif launchctl load -w "$plist" 2>/dev/null; then
    SERVICE_ACTIVE=1
    warn "El sistema usó la interfaz launchctl heredada como fallback."
  else
    warn "No se pudo cargar el LaunchAgent automáticamente. Cerrá sesión y volvé a entrar, o corré:"
    warn "  launchctl bootstrap gui/$uid \"$plist\""
  fi
}

setup_autostart() {
  case "$PLATFORM" in
    linux) setup_systemd_service ;;
    macos) setup_launchd_agent ;;
  esac
}
# ---- Paso 7 (opcional): CAP_SYS_NICE antes de iniciar el servicio -------
grant_cap_sys_nice() {
  [ "$PLATFORM" = "linux" ] || { warn "--grant-cap-sys-nice solo aplica en Linux."; return 1; }
  [ -x "$PREFIX/sysopt-priority-helper" ] || { warn "No se encontró el helper compilado/verificado; se omiten capacidades."; return 1; }
  command -v setcap >/dev/null 2>&1 || { warn "'setcap' no está disponible (paquete libcap2-bin/libcap); instalalo e intentá de nuevo."; return 1; }
  local system_helper="/usr/local/libexec/sysopt-priority-helper"
  local -a elevate=()
  if [ "${EUID:-$(id -u)}" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || { warn "Se necesita sudo para instalar el helper como root."; return 1; }
    elevate=(sudo)
  fi
  "${elevate[@]}" install -d -m 0755 /usr/local/libexec
  "${elevate[@]}" install -o root -g root -m 0755 "$PREFIX/sysopt-priority-helper" "$system_helper"
  if "${elevate[@]}" setcap 'cap_sys_nice=ep' "$system_helper" \
    && "$system_helper" --probe >/dev/null 2>&1; then
    log "Helper mínimo activado con CAP_SYS_NICE; el proceso principal continúa sin privilegios."
    return 0
  fi
  warn "El sistema de archivos o la política del sistema impidieron activar CAP_SYS_NICE; SysOpt seguirá funcionando sin elevaciones."
  return 1
}
[ "$GRANT_CAP_SYS_NICE" -eq 0 ] || grant_cap_sys_nice || true
if [ "$SETUP_SERVICE" -eq 1 ]; then setup_autostart || true; fi

# ---- Resumen ------------------------------------------------------------
echo
log "¡Listo! sysopt instalado en: $PREFIX/sysopt"
log "Probalo con:   sysopt --help"
log "Modo dry-run:  sysopt"
log "Modo real:     sysopt --apply --auto --profile smart"
log "Control Center: sysopt-control"
if [ "$SETUP_SERVICE" -eq 1 ] && [ "$SERVICE_ACTIVE" -eq 1 ]; then
  log "Arranque automático activo: sysopt ya corre con el binario instalado."
elif [ "$SETUP_SERVICE" -eq 1 ]; then
  warn "Arranque automático configurado, pero no se pudo confirmar que esté activo en esta sesión."
else
  log "Arranque automático desactivado (--no-service). Corré este instalador sin esa flag para activarlo."
fi
[ "$GRANT_CAP_SYS_NICE" -eq 0 ] && [ "$PLATFORM" = "linux" ] && log "Tip: pasá --grant-cap-sys-nice para poder subir prioridades sin sudo en cada corrida."
