#!/usr/bin/env bash
# Capture a WSLg window (or the whole virtual desktop) as a PNG.
#
# WSLg apps are Wayland clients, so Linux screenshot tools cannot see them.
# This helper runs the Windows-side capture script against the WSLg RAIL
# window, then copies the PNG back into the WSL filesystem.
#
# Usage:
#   capture-window.sh [output.png] [--full-screen]
#
# Defaults:
#   output: /tmp/opencode/wslg-window.png
#   window: Windows process msrdc, title matching "VRC Photo Manager*"
set -euo pipefail

OUT="${1:-/tmp/opencode/wslg-window.png}"
MODE="${2:-}"
POWERSHELL="${POWERSHELL_EXE:-/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe}"
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PS1="$SCRIPT_DIR/capture-window.ps1"

command -v wslpath >/dev/null 2>&1 || {
  echo "wslpath not found; this helper must run inside WSL." >&2
  exit 1
}
[[ -f "$POWERSHELL" ]] || {
  echo "PowerShell not found: $POWERSHELL" >&2
  echo "Set POWERSHELL_EXE to the Windows powershell.exe path if it is installed elsewhere." >&2
  exit 1
}
[[ -f "$PS1" ]] || {
  echo "capture-window.ps1 not found: $PS1" >&2
  exit 1
}

win_temp=$("$POWERSHELL" -NoProfile -Command '$env:TEMP' | tr -d '\r')
[[ -n "$win_temp" ]] || {
  echo "could not resolve the Windows TEMP directory." >&2
  exit 1
}

win_ps1="${win_temp}\\wslg-capture-$$.ps1"
win_png="${win_temp}\\wslg-window-$$.png"
cp "$PS1" "$(wslpath -u "$win_ps1")"

args=(-NoProfile -ExecutionPolicy Bypass -File "$win_ps1" -OutFile "$win_png")
if [[ "$MODE" == "--full-screen" ]]; then
  args+=(-FullScreen)
fi

"$POWERSHELL" "${args[@]}"

mkdir -p "$(dirname "$OUT")"
cp "$(wslpath -u "$win_png")" "$OUT"
rm -f "$(wslpath -u "$win_png")" "$(wslpath -u "$win_ps1")"
echo "$OUT"
