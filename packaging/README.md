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

## License notices

`Packager.toml` bundles these files through `resources`:

- `LICENSE` (application, MIT)
- `packaging/licenses/LICENSE-LUCIDE` (Lucide ISC; Feather-derived icons MIT)

Linux packages place them in `/usr/lib/vrc-photo-manager/`. The Windows
installer places them next to the executable. See `docs/licenses.md` for
provenance and update steps.

## systemd user unit

The deb and pacman packages also install
`/usr/lib/systemd/user/vrc-photo-daemon.service`. It is not enabled by the
package manager because installation runs as root and cannot safely enable a
service for each user.

Turn off the in-app autostart before enabling the service. If both paths run
at login, the XDG-autostarted daemon can win the race and leave the systemd
service inactive.

```bash
systemctl --user daemon-reload
systemctl --user enable --now vrc-photo-daemon.service
systemctl --user is-active vrc-photo-daemon.service
journalctl --user -u vrc-photo-daemon.service -f
```

On a desktop environment that does not manage `graphical-session.target`, link
the unit into `default.target` instead:

```bash
systemctl --user add-wants default.target vrc-photo-daemon.service
```

Disable it before removing the package so the enable symlink in the user's
home directory is cleaned up:

```bash
systemctl --user disable --now vrc-photo-daemon.service
systemctl --user remove-wants default.target vrc-photo-daemon.service
systemctl --user daemon-reload
```

The AppImage does not use this unit; it keeps the app-managed autostart
(`$APPIMAGE --background`). The daemon's `single-instance` guard prevents two
copies from running when the service and app-managed autostart overlap.

Runtime expectations:

- Vulkan-capable GPU/driver
- Wayland or X11
- `libxkbcommon`, `libwayland-client`, `libvulkan`, `libx11`, `libxcb`,
  `fontconfig`, `freetype2`
