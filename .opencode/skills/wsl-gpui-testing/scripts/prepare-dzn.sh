#!/usr/bin/env bash
# Extract the Mesa Dozen (dzn) Vulkan driver for WSL without touching the
# system Mesa installation. Intended for development-time GPU testing only.
#
# Usage: prepare-dzn.sh [version]
#   version defaults to $VRC_DZN_VERSION or 26.2.5~kisak1~r
#   destination defaults to $VRC_DZN_DIR or ~/.local/share/vulkan-dzn
set -euo pipefail

VERSION="${1:-${VRC_DZN_VERSION:-26.2.5~kisak1~r}}"
DEST="${VRC_DZN_DIR:-$HOME/.local/share/vulkan-dzn}"
SO="$DEST/usr/lib/x86_64-linux-gnu/libvulkan_dzn.so"
ICD="$DEST/usr/share/vulkan/icd.d/dzn_icd.json"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing dependency: $1" >&2
    exit 1
  }
}

need curl
need dpkg-deb
need python3

if [[ -f "$SO" && -f "$ICD" ]]; then
  echo "dzn already extracted: $DEST"
else
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  url="https://launchpad.net/~kisak/+archive/ubuntu/kisak-mesa/+files/mesa-vulkan-drivers_${VERSION}_amd64.deb"
  echo "downloading $url"
  if ! curl -L --fail --retry 2 --silent --show-error -o "$tmp/mesa-vulkan-drivers.deb" "$url"; then
    echo "download failed. Check the version or set VRC_DZN_VERSION." >&2
    echo "Available packages: https://launchpad.net/~kisak/+archive/ubuntu/kisak-mesa/+packages" >&2
    exit 1
  fi
  mkdir -p "$DEST"
  dpkg-deb -x "$tmp/mesa-vulkan-drivers.deb" "$DEST"
fi

# The packaged ICD uses a relative library_path; make it absolute for the
# per-user extraction.
python3 - "$ICD" "$SO" <<'PY'
import json
import sys

path, so = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as handle:
    data = json.load(handle)
data["ICD"]["library_path"] = so
with open(path, "w", encoding="utf-8") as handle:
    json.dump(data, handle, indent=4)
    handle.write("\n")
PY

export VK_ICD_FILENAMES="$ICD"
echo "VK_ICD_FILENAMES=$ICD"

if command -v vulkaninfo >/dev/null 2>&1; then
  output=$(vulkaninfo --summary 2>&1 || true)
  echo "$output" | sed -n '/Devices:/,$p'
  if ! echo "$output" | grep -qi 'dozen\|dzn'; then
    echo "dzn device not found. The extraction may be incomplete." >&2
    exit 1
  fi
else
  echo "vulkaninfo not found; install vulkan-tools to verify the device." >&2
fi

cat <<EOF

Use it with:
  export VK_ICD_FILENAMES=$ICD
  export WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1
  export GALLIUM_DRIVER=d3d12

The WGPU_ALLOW... variable only takes effect after applying the
gpui-pre-wgpu patch (see SKILL.md).
EOF
