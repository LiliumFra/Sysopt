#!/usr/bin/env bash
set -u
umask 077
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${SYSOPT_BIN:-$SCRIPT_DIR/sysopt}"
[[ -x "$BIN" ]] || BIN="$(command -v sysopt 2>/dev/null || true)"
[[ -n "$BIN" && -x "$BIN" ]] || { echo "No se encontró el binario sysopt."; exit 1; }
OS="$(uname -s)"

start_service() {
  case "$OS" in
    Linux) systemctl --user enable --now sysopt.service ;;
    Darwin)
      local plist="$HOME/Library/LaunchAgents/io.sysopt.agent.plist"
      local uid="$(id -u)"
      local target="gui/$uid/io.sysopt.agent"
      launchctl bootout "$target" >/dev/null 2>&1 || true
      if launchctl bootstrap "gui/$uid" "$plist" 2>/dev/null; then
        launchctl enable "$target" >/dev/null 2>&1 || true
        launchctl kickstart -k "$target"
      else
        launchctl load -w "$plist"
      fi
      ;;
    *) "$BIN" --apply --auto --profile smart >/dev/null 2>&1 & ;;
  esac
}

stop_service() {
  "$BIN" --shutdown >/dev/null 2>&1 || true
  case "$OS" in
    Linux) systemctl --user stop sysopt.service >/dev/null 2>&1 || true ;;
    Darwin)
      launchctl bootout "gui/$(id -u)/io.sysopt.agent" >/dev/null 2>&1 \
        || launchctl unload "$HOME/Library/LaunchAgents/io.sysopt.agent.plist" >/dev/null 2>&1 \
        || true
      ;;
  esac
}

while true; do
  clear
  echo "========================================"
  echo "          SysOpt Control Center"
  echo "========================================"
  "$BIN" --status 2>/dev/null || echo "SysOpt detenido"
  echo
  echo "1) Iniciar"
  echo "2) Pausar"
  echo "3) Reanudar"
  echo "4) Modo Inteligente"
  echo "5) Ahorro"
  echo "6) Equilibrado"
  echo "7) Rendimiento"
  echo "8) Juegos"
  echo "9) Desarrollo"
  echo "10) Creación"
  echo "11) Streaming"
  echo "12) Silencioso"
  echo "13) Diagnóstico"
  echo "14) Detener"
  echo "0) Salir"
  read -r -p "Opción: " choice
  case "$choice" in
    1) start_service ;;
    2) "$BIN" --pause ;;
    3) "$BIN" --resume ;;
    4) "$BIN" --set-profile smart ;;
    5) "$BIN" --set-profile eco ;;
    6) "$BIN" --set-profile balanced ;;
    7) "$BIN" --set-profile performance ;;
    8) "$BIN" --set-profile gaming ;;
    9) "$BIN" --set-profile development ;;
    10) "$BIN" --set-profile creator ;;
    11) "$BIN" --set-profile streaming ;;
    12) "$BIN" --set-profile quiet ;;
    13) "$BIN" --doctor; read -r -p "Enter para continuar..." _ ;;
    14) stop_service ;;
    0) exit 0 ;;
  esac
  sleep 1
done
