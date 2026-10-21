# Linux packaging

`cargo-packager` builds three Linux artifacts into `dist/`:

- Debian package (`.deb`)
- AppImage (`.AppImage`)
- Arch Linux/pacman package (`.pkg.tar.*` plus `PKGBUILD`)

```bash
cargo install cargo-packager --locked --version 0.11.8
cargo packager --release
```

The config lives in `Packager.toml`. The release workflow runs the same command
on tag pushes and uploads every file in `dist/` to the GitHub Release.

Runtime expectations:

- Vulkan-capable GPU/driver
- Wayland or X11
- `libxkbcommon`, `libwayland-client`, `libvulkan`, `libx11`, `libxcb`,
  `fontconfig`, `freetype2`
