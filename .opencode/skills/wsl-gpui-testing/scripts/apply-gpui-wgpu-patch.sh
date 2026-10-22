#!/usr/bin/env bash
# Apply the gpui-pre-wgpu WSL/dzn opt-in patch to a disposable worktree.
#
# The patch changes only WgpuContext::instance() on native targets:
#   InstanceFlags::default() -> InstanceFlags::from_env_or_default()
# so that WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER exposes Mesa dzn.
#
# Usage: apply-gpui-wgpu-patch.sh <worktree-dir>
set -euo pipefail

WORKTREE="${1:-}"
if [[ -z "$WORKTREE" || ! -d "$WORKTREE" ]]; then
  echo "usage: $0 <worktree-dir>" >&2
  exit 2
fi
WORKTREE=$(cd "$WORKTREE" && pwd)

if [[ ! -f "$WORKTREE/Cargo.toml" ]]; then
  echo "not a repository root: $WORKTREE" >&2
  exit 1
fi

BRANCH=$(git -C "$WORKTREE" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)
if [[ "$BRANCH" == "main" && "${ALLOW_MAIN:-0}" != "1" ]]; then
  echo "refusing to patch main; create a worktree/branch first" >&2
  echo "set ALLOW_MAIN=1 only for a throwaway checkout" >&2
  exit 1
fi

CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
CRATE=$(find "$CARGO_HOME_DIR/registry/src" -maxdepth 2 -type d -name 'gpui-pre-wgpu-0.3.9' 2>/dev/null | head -1)
if [[ -z "$CRATE" ]]; then
  echo "gpui-pre-wgpu-0.3.9 not found in the cargo registry." >&2
  echo "Run 'cargo fetch' or build the app once, then retry." >&2
  exit 1
fi

DEST="$WORKTREE/vendor/gpui-pre-wgpu"
if [[ ! -d "$DEST" ]]; then
  mkdir -p "$WORKTREE/vendor"
  cp -a "$CRATE" "$DEST"
  echo "copied $CRATE -> $DEST"
fi

python3 - "$DEST/src/wgpu_context.rs" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
text = path.read_text(encoding="utf-8")

old = '''    pub fn instance(display: Option<Box<dyn wgpu::wgt::WgpuHasDisplayHandle>>) -> wgpu::Instance {
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),'''

new = '''    pub fn instance(display: Option<Box<dyn wgpu::wgt::WgpuHasDisplayHandle>>) -> wgpu::Instance {
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            // WSL's dzn driver is non-conformant; honor the opt-in env var.
            flags: wgpu::InstanceFlags::from_env_or_default(),'''

if "from_env_or_default" in text:
    print("wgpu_context.rs already patched")
elif old not in text:
    raise SystemExit("expected WgpuContext::instance() block not found; check the crate version")
else:
    path.write_text(text.replace(old, new, 1), encoding="utf-8")
    print("patched WgpuContext::instance()")
PY

python3 - "$WORKTREE/Cargo.toml" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
section = "[patch.crates-io]"
entry = 'gpui-pre-wgpu = { path = "vendor/gpui-pre-wgpu" }'

if section not in text:
    path.write_text(text.rstrip() + "\n\n" + section + "\n" + entry + "\n", encoding="utf-8")
    print("added [patch.crates-io]")
elif entry not in text:
    path.write_text(text.replace(section, section + "\n" + entry, 1), encoding="utf-8")
    print("added gpui-pre-wgpu to [patch.crates-io]")
else:
    print("Cargo.toml already patched")
PY

MAIN_ROOT=$(git -C "$WORKTREE" worktree list --porcelain | awk '/^worktree /{print $2; exit}')
SKILL_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

cat <<EOF

Patch applied. Build and launch:
  cd "$WORKTREE"
  CARGO_TARGET_DIR="$MAIN_ROOT/target" cargo build -p vrc-photo-ui
  "$SKILL_DIR/scripts/launch.sh" --dzn --demo

Do not commit vendor/ or the Cargo.toml/Cargo.lock patch.
EOF
