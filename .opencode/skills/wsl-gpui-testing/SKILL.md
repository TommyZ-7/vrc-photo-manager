---
name: WSL GPUI Testing
description: Launch and visually verify the vrc-photo-manager GPUI app in WSL/WSLg, including the experimental dzn/Dozen GPU path and Windows-side window screenshots. Use whenever the user asks to run the UI in WSL, capture a WSLg window image, compare llvmpipe vs GPU rendering, or debug WSL-only click/performance issues (e.g. 「WSLで起動して」「画面を取得して」「クリックが効かない」).
---

# WSL GPUI Testing

## Choose the mode first

| Goal | Mode | Why |
|---|---|---|
| Functional check only | normal | no patches; llvmpipe is slow but sufficient |
| Click, hover, scroll, animation checks | dzn | GPU through Mesa Dozen; requires the local patch |
| Release / visual QA | Windows build | avoids WSLg entirely; use this for sign-off |

The dzn path is a **development workaround**, not release evidence. dzn reports
`conformanceVersion = 0.0.0.0`; rendering bugs and crashes are possible.

## 1. Normal launch (software rendering)

Use the existing debug or release binary:

```bash
./target/debug/vrc-photo-manager --demo
```

`--demo` avoids starting the daemon and uses 18 generated photos. Without
`--demo`, the app starts `vrc-photo-daemon` from the same directory and reads
the real library; use that only when testing daemon/library behavior.

Run GUI commands in the background (the shell tool's background mode, not a
trailing `&`) so the app does not block the session.

## 2. dzn GPU launch (experimental)

The GPU is usable on WSL, but two things are missing by default:

1. Ubuntu's stock `mesa-vulkan-drivers` does not include dzn (Dozen).
2. `gpui-pre-wgpu 0.3.9` builds the wgpu instance with
   `InstanceFlags::default()`, so wgpu hides dzn as a non-conformant adapter.
   The `WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER` variable is only read by
   `InstanceFlags::from_env_or_default()`.

### 2.1 Extract dzn locally (no system install)

```bash
.opencode/skills/wsl-gpui-testing/scripts/prepare-dzn.sh
```

The script downloads the kisak-mesa `mesa-vulkan-drivers` package, extracts it
under `~/.local/share/vulkan-dzn`, rewrites the ICD JSON to an absolute
`library_path`, and prints the `vulkaninfo` devices. It does **not** replace the
system Mesa.

Expected device:

```text
deviceName = Microsoft Direct3D12 (Intel(R) Iris(R) Xe Graphics)
driverID   = DRIVER_ID_MESA_DOZEN
driverName = Dozen
```

If no dzn device appears, stop here and use the normal mode or the Windows
build. Do not chase the `dxgkio_query_adapter_info` dmesg errors; they also
appear on working WSL GPU setups.

### 2.2 Patch gpui-pre-wgpu in a throwaway worktree

Never patch `main` in place. Create a worktree first:

```bash
REPO=$(basename "$(git rev-parse --show-toplevel)")
git worktree add ../${REPO}-wsl-dzn -b chore/wsl-dzn-poc main
.opencode/skills/wsl-gpui-testing/scripts/apply-gpui-wgpu-patch.sh ../${REPO}-wsl-dzn
```

The script copies `gpui-pre-wgpu-0.3.9` into `vendor/`, changes the native
`WgpuContext::instance()` flags to `wgpu::InstanceFlags::from_env_or_default()`,
and adds the `[patch.crates-io]` entry. Review the diff before building.

Build with the main worktree's target directory so dependencies are reused:

```bash
cd ../${REPO}-wsl-dzn
CARGO_TARGET_DIR=../${REPO}/target cargo build -p vrc-photo-ui
```

### 2.3 Launch and verify

```bash
.opencode/skills/wsl-gpui-testing/scripts/launch.sh --dzn --demo
```

The script sets:

- `VK_ICD_FILENAMES` to the local dzn ICD
- `WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1`
- `GALLIUM_DRIVER=d3d12` (also fixes OpenGL fallbacks)

Run it in the background and capture the PID. Verify the renderer:

```bash
PID=<app pid>
grep -c llvmpipe /proc/$PID/task/*/comm   # expect 0
grep -q libvulkan_dzn /proc/$PID/maps && echo "dzn loaded"
ls -l /proc/$PID/fd | grep dxg            # expect /dev/dxg
```

The `WARNING: dzn is not a conformant Vulkan implementation` line is expected.
The absence of `llvmpipe-*` threads is the key success signal; idle CPU should
also drop to a few percent.

## 3. Capture the GUI

The app is a Wayland client on WSLg, so Linux-side tools (`grim`, `xwd`,
`scrot`) cannot see its window. Capture it from Windows instead:

```bash
.opencode/skills/wsl-gpui-testing/scripts/capture-window.sh /tmp/opencode/vrc-window.png
```

Then read the PNG with the `read` tool. The script calls the PowerShell helper,
which:

1. Finds the Windows process `msrdc` whose title matches `VRC Photo Manager*`
   (WSLg appends `(Ubuntu)` to the title).
2. Restores it and temporarily makes it topmost.
3. Captures the client area with `PrintWindow` (falling back to
   `CopyFromScreen` if the window rejects it). `PrintWindow` avoids the WSLg
   shadow/desktop strips around the client rect.
4. Saves a PNG and resets the topmost state.

The script prints `method=PrintWindow` or `method=screen`; both are valid. A
clean capture is 1280x820 for the default demo window.

For the full desktop instead of one window, call the PowerShell helper
directly with `-FullScreen`:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass \
  -File 'C:\...\capture-window.ps1' -FullScreen -OutFile 'C:\...\full.png'
```

## 4. Troubleshooting

### Window not found

The app may not be running, or the Windows title changed. List windows:

```bash
/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile \
  -Command "Get-Process | Where-Object MainWindowTitle | Select-Object ProcessName,MainWindowTitle"
```

The WSLg window is owned by `msrdc`, not by the Linux process name.

### Screenshot shows another application

The window was behind another window and foreground activation failed. The
helper uses `ShowWindow(SW_RESTORE)` and `HWND_TOPMOST`; if it still fails,
click the window once and capture again. `CopyFromScreen` captures whatever is
visible, so the window must be unobscured.

### Black or empty image

The Windows session is locked or the RDP client is disconnected. Reconnect and
retry. Also check that the window is not minimized.

### dzn is installed but the app still uses llvmpipe

Check all three conditions:

```bash
grep -n "from_env_or_default" ../vrc-photo-manager-wsl-dzn/vendor/gpui-pre-wgpu/src/wgpu_context.rs
echo "$WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER"
echo "$VK_ICD_FILENAMES"
```

Then confirm `vulkaninfo --summary` shows `DRIVER_ID_MESA_DOZEN`. wgpu hides
non-conformant adapters unless the patch and the env var are both present.

### The patch does not compile

The vendored copy must be `gpui-pre-wgpu 0.3.9`. If the registry was updated,
remove `vendor/gpui-pre-wgpu` and rerun the patch script after `cargo fetch`.
Keep the worktree disposable; do not commit the vendor copy.

## 5. Cleanup and safety

- Stop the app before removing the worktree (`kill <pid>`).
- Remove the throwaway worktree with `git worktree remove <path>`.
- The local dzn files live in `~/.local/share/vulkan-dzn`; delete them if no
  longer needed.
- Keep screenshots in `/tmp/opencode` or another temporary path. They can
  contain unrelated desktop content and must not be committed.
- Do not install the PPA system-wide unless the user explicitly asks. The local
  extraction is enough for testing and is easy to undo.
- Do not present the dzn path as release validation. Use the Windows build or a
  real Linux GPU for sign-off.

Technical background and sources: `references/dzn-notes.md`.
