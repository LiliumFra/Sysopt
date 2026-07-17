#!/usr/bin/env bash
set -euo pipefail
umask 077

PREFIX="${SYSOPT_PREFIX:-$HOME/.local/bin}"
KEEP_CONFIG=1
[[ "${1:-}" == "--remove-data" ]] && KEEP_CONFIG=0
OS="$(uname -s)"
TARGET_HOME="$HOME"
TARGET_UID="$(id -u)"
TARGET_USER="${USER:-}"
if [[ "$OS" == "Darwin" && "${EUID:-$(id -u)}" -eq 0 ]]; then
  CONSOLE_USER=$(/usr/bin/stat -f '%Su' /dev/console 2>/dev/null || true)
  if [[ -n "$CONSOLE_USER" && "$CONSOLE_USER" != "root" && "$CONSOLE_USER" != "loginwindow" ]]; then
    TARGET_USER="$CONSOLE_USER"
    TARGET_UID=$(/usr/bin/id -u "$CONSOLE_USER")
    TARGET_HOME=$(/usr/bin/dscl . -read "/Users/$CONSOLE_USER" NFSHomeDirectory 2>/dev/null | /usr/bin/awk '{print $2}')
  fi
fi

safe_remove_tree() {
  local path="$1" expected_name="$2"
  [[ -n "$path" && "$path" == /* && "$path" != "/" && "$path" != "$TARGET_HOME" ]] || {
    echo "Se rechazó una ruta de borrado insegura: $path" >&2
    return 1
  }
  [[ "$(basename "$path")" == "$expected_name" ]] || {
    echo "Se rechazó una ruta inesperada: $path" >&2
    return 1
  }
  [[ ! -L "$path" ]] || {
    echo "Se rechazó borrar datos mediante un enlace: $path" >&2
    return 1
  }
  rm -rf -- "$path"
}

stop_user_service() {
  local bin="$1"
  [[ -x "$bin" ]] && "$bin" --shutdown >/dev/null 2>&1 || true
  case "$OS" in
    Linux)
      command -v systemctl >/dev/null 2>&1 && systemctl --user disable --now sysopt.service >/dev/null 2>&1 || true
      ;;
    Darwin)
      local uid target plist legacy
      uid="$TARGET_UID"
      target="gui/$uid/io.sysopt.agent"
      plist="$TARGET_HOME/Library/LaunchAgents/io.sysopt.agent.plist"
      legacy="$TARGET_HOME/Library/LaunchAgents/com.sysopt.agent.plist"
      launchctl bootout "$target" >/dev/null 2>&1 \
        || launchctl unload "$plist" >/dev/null 2>&1 \
        || true
      launchctl bootout "gui/$uid/com.sysopt.agent" >/dev/null 2>&1 || true
      rm -f -- "$plist" "$legacy"
      ;;
  esac
}

stop_user_service "$PREFIX/sysopt"

if [[ "$OS" == "Linux" ]]; then
  rm -f -- "$HOME/.config/systemd/user/sysopt.service"
  command -v systemctl >/dev/null 2>&1 && systemctl --user daemon-reload >/dev/null 2>&1 || true
  rm -f -- "$HOME/.local/share/applications/sysopt-control.desktop"

  SYSTEM_HELPER="/usr/local/libexec/sysopt-priority-helper"
  if [[ -e "$SYSTEM_HELPER" ]]; then
    if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
      rm -f -- "$SYSTEM_HELPER"
    else
      echo "Queda el helper protegido $SYSTEM_HELPER. Para eliminarlo: sudo rm -f '$SYSTEM_HELPER'" >&2
    fi
  fi
elif [[ "$OS" == "Darwin" ]]; then
  rm -f -- "$TARGET_HOME/Applications/SysOpt Control.command"

  # Una instalación PKG usa ubicaciones del sistema. Solo se modifican si el
  # script se ejecuta como root; una instalación de usuario nunca escala sola.
  if [[ -e /Library/LaunchAgents/io.sysopt.agent.plist || -x /usr/local/bin/sysopt || -d "/Applications/SysOpt Control.app" ]]; then
    if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
      if [[ -n "$TARGET_USER" && "$TARGET_USER" != "root" ]]; then
        if [[ -x /usr/local/bin/sysopt ]]; then
          /bin/launchctl asuser "$TARGET_UID" /usr/bin/sudo -u "$TARGET_USER" \
            /usr/local/bin/sysopt --shutdown >/dev/null 2>&1 || true
        fi
        /bin/launchctl bootout "gui/$TARGET_UID/io.sysopt.agent" >/dev/null 2>&1 || true
      fi
      rm -f -- /usr/local/bin/sysopt /usr/local/bin/sysopt-control /Library/LaunchAgents/io.sysopt.agent.plist
      rm -rf -- "/Applications/SysOpt Control.app" "/Library/Application Support/SysOpt"
      /usr/sbin/pkgutil --forget io.sysopt.pkg >/dev/null 2>&1 || true
    else
      echo "Hay una instalación PKG del sistema. Ejecutá este desinstalador con sudo para eliminarla." >&2
    fi
  fi
fi

rm -f -- "$PREFIX/sysopt" "$PREFIX/sysopt-priority-helper" "$PREFIX/sysopt-control" "$PREFIX/sysopt-uninstall"

if [[ "$KEEP_CONFIG" -eq 0 ]]; then
  if [[ "$OS" == "Darwin" ]]; then
    safe_remove_tree "$TARGET_HOME/Library/Application Support/SysOpt" "SysOpt"
    safe_remove_tree "$TARGET_HOME/Library/Caches/SysOpt" "SysOpt"
  else
    XDG_CONFIG_ROOT="${XDG_CONFIG_HOME:-$HOME/.config}"
    XDG_DATA_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}"
    XDG_STATE_ROOT="${XDG_STATE_HOME:-$HOME/.local/state}"
    XDG_CACHE_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}"
    safe_remove_tree "$XDG_CONFIG_ROOT/sysopt" "sysopt"
    safe_remove_tree "$XDG_DATA_ROOT/sysopt" "sysopt"
    safe_remove_tree "$XDG_STATE_ROOT/sysopt" "sysopt"
    safe_remove_tree "$XDG_CACHE_ROOT/sysopt" "sysopt"
  fi
fi

echo "SysOpt desinstalado.$([[ "$KEEP_CONFIG" -eq 1 ]] && echo ' Se conservaron configuración y datos.')"
