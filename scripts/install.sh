#!/usr/bin/env bash
set -euo pipefail

# Simple user-level installer for mypixels
# - Installs executable, .desktop file, and icon into XDG user dirs
# - Supports install, update (same as install), and remove

APP_NAME="mypixels"
APP_ID="com.github.volca.mypixels"
ICON_NAME="mypixels"

# Resolve repo root (directory containing this script)
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd)"

# Sources within repo
BIN_SRC="${REPO_ROOT}/${APP_NAME}"
ICON_SRC="${REPO_ROOT}/icon.png"

# XDG target locations
BIN_DIR="${XDG_BIN_HOME:-"${HOME}/.local/bin"}"
DATA_HOME="${XDG_DATA_HOME:-"${HOME}/.local/share"}"
APP_DIR="${DATA_HOME}/applications"
ICON_THEME_DIR="${DATA_HOME}/icons/hicolor"
ICON_SIZE_DIR="${ICON_THEME_DIR}/256x256/apps"  # place PNG here
PIXMAPS_DIR="${DATA_HOME}/pixmaps"              # optional fallback

# Targets
BIN_DST="${BIN_DIR}/${APP_NAME}"
DESKTOP_DST="${APP_DIR}/${APP_NAME}.desktop"
ICON_DST="${ICON_SIZE_DIR}/${ICON_NAME}.png"
PIXMAPS_DST="${PIXMAPS_DIR}/${ICON_NAME}.png"

usage() {
  cat <<EOF
Usage: $(basename "$0") <install|update|remove>

Actions:
  install   Install executable, icon, and desktop entry to user directories
  update    Re-install files from the current repo (same as install)
  remove    Remove installed files from user directories

Installs to:
  bin:       ${BIN_DIR}
  desktop:   ${APP_DIR}
  icon:      ${ICON_SIZE_DIR} (and ${PIXMAPS_DIR} as fallback)
EOF
}

ensure_requirements() {
  if [[ ! -x "${BIN_SRC}" ]]; then
    echo "Error: Executable not found or not executable at ${BIN_SRC}" >&2
    exit 1
  fi
  if [[ ! -f "${ICON_SRC}" ]]; then
    echo "Error: Icon not found at ${ICON_SRC}" >&2
    exit 1
  fi
}

write_desktop_file() {
  local path="$1"
  mkdir -p "$(dirname -- "${path}")"
  cat >"${path}" <<DESKTOP
[Desktop Entry]
Type=Application
Version=1.0
Name=Mypixels
GenericName=Terminal Emulator
Comment=Low-clutter GTK-based terminal emulator
Exec=${BIN_DST} %F
TryExec=${BIN_DST}
Icon=${ICON_NAME}
Terminal=false
Categories=Utility;TerminalEmulator;
StartupNotify=true
# Helps dock/taskbar associate the window with this launcher
StartupWMClass=mypixels
# D-Bus aware application ID (used by Gtk.Application)
X-Flatpak-RenamedFrom=${APP_NAME}.desktop;
DESKTOP
}

install_icon() {
  mkdir -p "${ICON_SIZE_DIR}" "${PIXMAPS_DIR}"
  install -m 0644 "${ICON_SRC}" "${ICON_DST}"
  # Also drop into pixmaps as a broad fallback used by some desktops
  install -m 0644 "${ICON_SRC}" "${PIXMAPS_DST}" || true

  # Refresh icon cache if available
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q "${ICON_THEME_DIR}" || true
  fi
}

install_bin() {
  mkdir -p "${BIN_DIR}"
  install -m 0755 "${BIN_SRC}" "${BIN_DST}"
}

install_desktop() {
  write_desktop_file "${DESKTOP_DST}"
  # Refresh desktop database if available (not strictly required)
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "${APP_DIR}" || true
  fi
}

do_install() {
  ensure_requirements
  install_bin
  install_icon
  install_desktop
  echo "Installed ${APP_NAME} to user directories."
  echo "- Binary:   ${BIN_DST}"
  echo "- Desktop:  ${DESKTOP_DST}"
  echo "- Icon:     ${ICON_DST}"
}

do_remove() {
  local removed_any=false
  if [[ -e "${BIN_DST}" ]]; then rm -f "${BIN_DST}" && removed_any=true; fi
  if [[ -e "${DESKTOP_DST}" ]]; then rm -f "${DESKTOP_DST}" && removed_any=true; fi
  if [[ -e "${ICON_DST}" ]]; then rm -f "${ICON_DST}" && removed_any=true; fi
  if [[ -e "${PIXMAPS_DST}" ]]; then rm -f "${PIXMAPS_DST}" && removed_any=true; fi

  # Refresh caches if present
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q "${ICON_THEME_DIR}" || true
  fi
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "${APP_DIR}" || true
  fi

  if [[ "${removed_any}" == true ]]; then
    echo "Removed ${APP_NAME} from user directories."
  else
    echo "No installed files found for ${APP_NAME}."
  fi
}

main() {
  local cmd="${1:-}";
  case "${cmd}" in
    install|update)
      do_install
      ;;
    remove|uninstall)
      do_remove
      ;;
    -h|--help|help|"")
      usage
      ;;
    *)
      echo "Unknown command: ${cmd}" >&2
      echo >&2
      usage >&2
      exit 2
      ;;
  esac
}

main "$@"

