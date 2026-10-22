# WSL GPU and capture notes

## Why the normal launch uses llvmpipe

WSLg exposes the Windows GPU through `/dev/dxg` and the D3D12 libraries in
`/usr/lib/wsl/lib`. Mesa can translate D3D12 to OpenGL (Gallium `d3d12`) and
Vulkan (`dzn`, also called Dozen).

On Ubuntu 26.04 the stock `mesa-vulkan-drivers` package does **not** contain
`libvulkan_dzn.so` or `dzn_icd.json`. Without a Vulkan GPU driver, wgpu falls
back to the Vulkan CPU driver (`llvmpipe`). The `dxgkio_query_adapter_info:
Ioctl failed` lines in `dmesg` also appear on working GPU setups and are not a
reliable failure signal by themselves.

## The two blockers for gpui/wgpu

1. **dzn is not installed.** The kisak-mesa PPA builds Mesa with `Enable dozen`;
   the 26.2.5 package was verified locally.
2. **wgpu hides non-conformant adapters.** dzn reports
   `conformanceVersion = 0.0.0.0`. `wgpu-hal` returns `None` for an adapter whose
   driver conformance major version is 0 unless
   `InstanceFlags::ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER` is set.

`gpui-pre-wgpu 0.3.9` creates every instance with
`wgpu::InstanceFlags::default()`. The environment variable
`WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER` is only read by
`InstanceFlags::from_env_or_default()` (or `.with_env()`), so setting the
variable alone has no effect. The one-line patch in
`scripts/apply-gpui-wgpu-patch.sh` changes the native `WgpuContext::instance()`
to `from_env_or_default()`.

## Verified result

With a locally extracted dzn + the patch + the env var:

```text
adapters: 2
- name="Microsoft Direct3D12 (Intel(R) Iris(R) Xe Graphics)"
  backend=Vulkan type=IntegratedGpu driver="Dozen"
  device: OK
- name="llvmpipe (LLVM 21.1.8, 256 bits)"
  backend=Gl type=Cpu
```

The running app had `/dev/dxg` open, `libvulkan_dzn.so` mapped, and **no
`llvmpipe-*` threads**. That is the expected dzn state.

## Why the screenshot uses Windows

The app is a Wayland client on WSLg. It does not appear in the Xwayland root
window, so `xwd`, `scrot`, and `grim` cannot capture it. WSLg maps each Linux
window to a Windows RAIL window; the Windows process is `msrdc` and the title
gets a ` (Ubuntu)` suffix. The PowerShell helper captures that window rect.

If the app is run with `WAYLAND_DISPLAY` unset, it uses X11 and Linux capture
could work, but Xwayland is software-rendered and is not the recommended test
path.

## Safety and scope

- dzn is marked “not a conformant Vulkan implementation”. Use it for
  development checks only; do not use it as release evidence.
- The helper extracts the `.deb` under `~/.local/share/vulkan-dzn`; it does not
  replace the system Mesa.
- The PPA route (`add-apt-repository ppa:kisak/kisak-mesa`) replaces Mesa
  system-wide. It is supported only preliminarily on Ubuntu 26.04 and should be
  reverted with `ppa-purge` if used.
- Keep the `vendor/` patch in a throwaway worktree. Do not commit it to `main`.
  Upstreaming `from_env_or_default()` to gpui-pre is the clean long-term fix.

## Sources

- Microsoft WSL GUI apps (GPU driver prerequisite):
  https://learn.microsoft.com/en-us/windows/wsl/tutorials/gui-apps
- Mesa D3D12 driver and environment variables:
  https://docs.mesa3d.org/drivers/d3d12.html
- wgpu `InstanceFlags`:
  https://docs.rs/wgpu/latest/wgpu/struct.InstanceFlags.html
- kisak-mesa PPA (Mesa 26.2.5 changelog includes “Enable dozen”):
  https://launchpad.net/~kisak/+archive/ubuntu/kisak-mesa
- Bevy/WSL wgpu workaround (community):
  https://github.com/bevyengine/bevy/discussions/22851
- Zed/gpui WSL GPU history (community):
  https://github.com/zed-industries/zed/discussions/33802
