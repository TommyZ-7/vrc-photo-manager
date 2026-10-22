---
name: Hyprland Visual QA
description: Use when visually verifying or screenshotting a GUI app in a native Hyprland session while the human is using the PC for other work, or when app windows must not appear on the current workspace/monitor or steal focus/cursor (e.g. 「Hyprlandで画面検証」「別作業中にスクショ」「画面を出さないで」「ワークスペース9に固定」). Not for WSL/WSLg — use the wsl-gpui-testing skill there.
---

# Hyprland Visual QA

## Overview

Verify a GUI app in a native Hyprland session without showing it on the workspace the human is using. Use `scripts/hypr-verify.sh` (repo root): it launches the app on workspace 9 silently, moves that workspace to DP-3, captures with `grim -T`, and restores focus/cursor after key presses.

## Quick reference

Build first: `cargo build -p vrc-photo-ui`

| Step | Command |
|---|---|
| Launch (hidden) | `bash scripts/hypr-verify.sh start --demo --demo-reset --window-size 1280x820` |
| Capture | `bash scripts/hypr-verify.sh capture /tmp/opencode/ui-1280-expanded.png` |
| Press + capture | `bash scripts/hypr-verify.sh press ctrl+b --capture /tmp/opencode/ui-1280-collapsed.png` |
| Status | `bash scripts/hypr-verify.sh status` |
| Reveal for manual viewing | `bash scripts/hypr-verify.sh show` |
| Stop + cleanup | `bash scripts/hypr-verify.sh stop` |

`bash scripts/hypr-verify.sh help` lists all flags (`--release`, `--monitor`, `--workspace`, `--hidden`, `--settle`).

ws9 defaults to DP-3. If the human is working on DP-3, pass `--monitor DP-2` so the brief `press` focus lands on their other monitor.

## Why these commands

- Hyprland 0.56 is Lua-config era: `hyprctl dispatch exec '[workspace 9 silent] ...'` is a syntax error. The script uses `hyprctl eval` + `hl.dsp.exec_cmd`.
- `grim -g` needs a visible window; `grim -T <stableId>` captures the toplevel even on a hidden workspace, so screenshots never require switching or stealing focus. `stableId` comes from `hyprctl clients -j`.
- GPUI sets no `app_id`, so the Hyprland class is empty — never match by class. The script matches the title `VRC Photo Manager`.
- Keys go to the window via `hl.dsp.send_shortcut` / `send_key_state`. Hidden GPUI windows may render state changes late, so `press` briefly focuses ws9, sends the key, captures, then restores the previous window/workspace/cursor. Use `--hidden` only when a delayed update is acceptable.
- `--window-size WxH` makes the script float and resize the window so `grim -T` yields the exact requested size.

## Verification checklist

- The script aborts if ws9 has windows or is visible on any monitor — do not bypass this.
- After `start` / `press`, confirm the human's focused monitor, workspace, active window, and cursor are unchanged (`hyprctl monitors -j`, `hyprctl activewindow -j`, `hyprctl cursorpos`).
- Compare the before/after captures (e.g. `cmp -s` or an image diff) so a missed state change cannot pass as success.
- Always finish with `stop`; ws9 should disappear and no `VRC Photo Manager` client should remain.
- Keep screenshots in `/tmp/opencode`; never commit them.

## Common mistakes

| Mistake | Fix |
|---|---|
| `hyprctl dispatch exec '[workspace 9 silent] ...'` | Use the script (Lua-era dispatch syntax). |
| Launching on the current or "free" workspace | Launch on ws9 with the script; nothing appears. |
| `grim -g` while the window is hidden/obscured | Use `capture` (`grim -T`). |
| Matching the window by class | Class is empty; match by title. |
| Sending keys before the app settles | The script waits on start; raise `--settle` if a state change looks stale. |
| Leaving ws9/app running | Run `stop`. |

Requirements: Hyprland 0.56+, grim 1.5+ with `-T`, jq. For WSL/WSLg use the `wsl-gpui-testing` skill instead.
