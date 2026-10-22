#!/usr/bin/env bash
# Launch the vrc-photo-manager UI in WSL.
#
# Usage:
#   launch.sh [--dzn] [--release] [-- <app args>]
#
#   --dzn       Use the locally extracted Mesa dzn driver and the
#               WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER opt-in.
#               Requires the gpui-pre-wgpu patch (see SKILL.md).
#   --release   Use target/release instead of target/debug.
#   --demo      Pass through to the app for generated demo data.
#
# Run this in a background shell; it execs the app so the PID is the app.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$SCRIPT_DIR/../../../.." && pwd)

PROFILE=debug
USE_DZN=0
ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dzn)
      USE_DZN=1
      shift
      ;;
    --release)
      PROFILE=release
      shift
      ;;
    --)
      shift
      ARGS+=("$@")
      break
      ;;
    *)
      ARGS+=("$1")
      shift
      ;;
  esac
done

BIN="${VRC_BIN:-$ROOT/target/$PROFILE/vrc-photo-manager}"
if [[ ! -x "$BIN" ]]; then
  echo "binary not found: $BIN" >&2
  echo "build it first: CARGO_TARGET_DIR=$ROOT/target cargo build -p vrc-photo-ui" >&2
  exit 1
fi

if [[ "$USE_DZN" -eq 1 ]]; then
  DZN_DIR="${VRC_DZN_DIR:-$HOME/.local/share/vulkan-dzn}"
  ICD="$DZN_DIR/usr/share/vulkan/icd.d/dzn_icd.json"
  if [[ ! -f "$ICD" ]]; then
    echo "dzn ICD not found: $ICD" >&2
    echo "run: $SCRIPT_DIR/prepare-dzn.sh" >&2
    exit 1
  fi
  export VK_ICD_FILENAMES="$ICD"
  export WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1
  export GALLIUM_DRIVER=d3d12
  if [[ -n "${MESA_D3D12_DEFAULT_ADAPTER_NAME:-}" ]]; then
    export MESA_D3D12_DEFAULT_ADAPTER_NAME
  fi
fi

has_demo=0
for arg in "${ARGS[@]}"; do
  [[ "$arg" == "--demo" ]] && has_demo=1
done

echo "launching $BIN (dzn=$USE_DZN, profile=$PROFILE)" >&2
if [[ "$has_demo" -eq 0 ]]; then
  echo "note: non-demo launch starts vrc-photo-daemon from the binary directory" >&2
fi

exec "$BIN" "${ARGS[@]}"
