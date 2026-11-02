//! Clipboard image writes that do not depend on the GPUI backend.
//!
//! The GPUI clipboard backend does not publish image data on Linux and only
//! registers a PNG format on Windows. Linux and Windows therefore write the
//! rendered PNG themselves; macOS keeps using GPUI.

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::{Read as _, Seek, SeekFrom, Write as _};
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use vrc_photon_i18n::tr;
use vrc_photon_i18n::tr_args;

/// Serializes clipboard writes so two rapid copies cannot interleave.
static CLIPBOARD_WRITE_LOCK: Mutex<()> = Mutex::new(());

fn lock_clipboard_write() -> MutexGuard<'static, ()> {
    CLIPBOARD_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Display server backend used for an image clipboard write on Linux.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinuxSession {
    Wayland,
    X11,
}

/// External command and arguments for a Linux clipboard write.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClipboardCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

/// Chooses the Linux clipboard backend.
///
/// `XDG_SESSION_TYPE` is authoritative. When it is missing or not a graphical
/// session, the display variables decide: Wayland is preferred because a
/// Wayland session also exports `DISPLAY` for XWayland.
#[cfg(target_os = "linux")]
pub(crate) fn linux_session(
    session_type: Option<&str>,
    wayland_display: Option<&str>,
    display: Option<&str>,
) -> Option<LinuxSession> {
    match session_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("wayland") => return Some(LinuxSession::Wayland),
        Some("x11") => return Some(LinuxSession::X11),
        _ => {}
    }
    if wayland_display.is_some_and(|value| !value.trim().is_empty()) {
        Some(LinuxSession::Wayland)
    } else if display.is_some_and(|value| !value.trim().is_empty()) {
        Some(LinuxSession::X11)
    } else {
        None
    }
}

/// Returns the command that publishes `image/png` for the chosen session.
#[cfg(target_os = "linux")]
pub(crate) fn linux_clipboard_command(session: LinuxSession) -> ClipboardCommand {
    match session {
        LinuxSession::Wayland => ClipboardCommand {
            program: "wl-copy",
            args: &["--type", "image/png"],
        },
        LinuxSession::X11 => ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard", "-target", "image/png"],
        },
    }
}

/// Finds the first executable named `program` in a `PATH`-style value.
///
/// Empty entries (the current directory in POSIX) are ignored so a clipboard
/// write never runs a binary from the working directory.
#[cfg(target_os = "linux")]
pub(crate) fn find_executable(
    program: &str,
    path_var: Option<&OsStr>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let path_var = path_var?;
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(program))
        .find(|candidate| is_file(candidate))
}

#[cfg(target_os = "linux")]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Writes PNG bytes to the Linux clipboard through the session's tool.
#[cfg(target_os = "linux")]
pub(crate) fn write_png(png: &[u8]) -> Result<(), String> {
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let display = std::env::var("DISPLAY").ok();
    let session = linux_session(
        session_type.as_deref(),
        wayland_display.as_deref(),
        display.as_deref(),
    )
    .ok_or_else(|| tr!("compression.clipboard_session_unknown"))?;
    let command = linux_clipboard_command(session);
    let path_var = std::env::var_os("PATH");
    let executable = find_executable(command.program, path_var.as_deref(), is_executable_file)
        .ok_or_else(|| {
            tr_args!(
                "compression.clipboard_tool_missing",
                program = command.program
            )
        })?;
    let _guard = lock_clipboard_write();
    write_command(&executable, command.args, png)
}

/// Writes PNG bytes and a matching DIB to the Windows clipboard.
#[cfg(target_os = "windows")]
pub(crate) fn write_png(png: &[u8]) -> Result<(), String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|error| tr_args!("compression.clipboard_decode", error = error))?
        .to_rgba8();
    let dib = dib_from_rgba(image.width(), image.height(), image.as_raw());
    let _guard = lock_clipboard_write();
    windows_clipboard::write(png, &dib)
}

/// Sends `png` to an external tool's standard input and checks its exit status.
#[cfg(target_os = "linux")]
fn write_command(program: &Path, args: &[&str], png: &[u8]) -> Result<(), String> {
    write_command_with_timeout(program, args, png, CLIPBOARD_WRITE_TIMEOUT)
}

/// Upper bound for a clipboard helper to accept the image and exit.
///
/// `wl-copy` and `xclip` fork a background server after taking the selection,
/// so only the foreground process is expected to finish this quickly.
#[cfg(target_os = "linux")]
const CLIPBOARD_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_os = "linux")]
fn write_command_with_timeout(
    program: &Path,
    args: &[&str],
    png: &[u8],
    timeout: Duration,
) -> Result<(), String> {
    // A regular file is used for stdin so a helper that stops reading cannot
    // block the write. A pipe cannot be used for stderr either: the helper's
    // forked server inherits it and keeps it open after the foreground process
    // exits, so waiting for EOF would block until the selection is taken over.
    let mut stdin_file = tempfile::tempfile()
        .map_err(|error| tr_args!("compression.clipboard_tool_tempfile", error = error))?;
    stdin_file
        .write_all(png)
        .and_then(|()| stdin_file.seek(SeekFrom::Start(0)).map(|_| ()))
        .map_err(|error| tr_args!("compression.clipboard_tool_tempfile", error = error))?;
    let mut stderr_file = tempfile::tempfile()
        .map_err(|error| tr_args!("compression.clipboard_tool_tempfile", error = error))?;
    let mut attempts = 0;
    let mut child = loop {
        let stdin = stdin_file
            .try_clone()
            .map_err(|error| tr_args!("compression.clipboard_tool_tempfile", error = error))?;
        let stderr = stderr_file
            .try_clone()
            .map_err(|error| tr_args!("compression.clipboard_tool_tempfile", error = error))?;
        match Command::new(program)
            .args(args)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
        {
            Ok(child) => break child,
            // Another thread can fork while this executable is still open for
            // writing, leaving the child with the write descriptor. Retry
            // briefly instead of surfacing a spurious failure.
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 5 =>
            {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(tr_args!(
                    "compression.clipboard_tool_spawn",
                    program = program.display(),
                    error = error
                ));
            }
        }
    };

    let status = wait_for_child(&mut child, program, timeout)?;
    if status.success() {
        return Ok(());
    }

    let stderr = read_stderr(&mut stderr_file);
    let detail = if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    };
    let status = status
        .code()
        .map_or_else(|| "?".to_string(), |code| code.to_string());
    Err(tr_args!(
        "compression.clipboard_tool_failed",
        program = program.display(),
        status = status,
        detail = detail
    ))
}

#[cfg(target_os = "linux")]
fn wait_for_child(
    child: &mut Child,
    program: &Path,
    timeout: Duration,
) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(tr_args!(
                    "compression.clipboard_tool_wait",
                    program = program.display(),
                    error = error
                ));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(tr_args!(
                "compression.clipboard_tool_timeout",
                program = program.display(),
                millis = timeout.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn read_stderr(file: &mut File) -> String {
    let _ = file.seek(SeekFrom::Start(0));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Builds a `CF_DIB` payload: a 40-byte `BITMAPINFOHEADER` followed by
/// bottom-up 32-bit BGRA rows.
///
/// The alpha byte is forced opaque because legacy `CF_DIB` consumers treat it
/// as reserved. Transparency remains available through the parallel `"PNG"`
/// clipboard format.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn dib_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    const HEADER_LEN: usize = 40;
    let stride = width as usize * 4;
    let image_len = stride * height as usize;
    debug_assert_eq!(rgba.len(), image_len);

    let mut dib = Vec::with_capacity(HEADER_LEN + image_len);
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight (bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&(image_len as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    for row in (0..height as usize).rev() {
        let start = row * stride;
        for pixel in rgba[start..start + stride].as_chunks::<4>().0 {
            dib.extend_from_slice(&[pixel[2], pixel[1], pixel[0], u8::MAX]);
        }
    }
    dib
}

#[cfg(target_os = "windows")]
mod windows_clipboard {
    use super::*;
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_DIB;
    use windows::core::w;

    pub(super) fn write(png: &[u8], dib: &[u8]) -> Result<(), String> {
        let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        if png_format == 0 {
            return Err(windows_error("RegisterClipboardFormatW"));
        }

        open_clipboard()?;
        let _guard = ClipboardGuard;
        unsafe { EmptyClipboard() }
            .map_err(|error| windows_error_with("EmptyClipboard", &error))?;
        set_bytes(CF_DIB.0 as u32, dib)?;
        set_bytes(png_format, png)?;
        Ok(())
    }

    fn open_clipboard() -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(()),
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(windows_error_with("OpenClipboard", &error)),
            }
        }
    }

    fn set_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
                .map_err(|error| windows_error_with("GlobalAlloc", &error))?;
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                let _ = GlobalFree(Some(memory));
                return Err(windows_error("GlobalLock"));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(memory);
            if let Err(error) = SetClipboardData(format, Some(HANDLE(memory.0))) {
                let _ = GlobalFree(Some(memory));
                return Err(windows_error_with("SetClipboardData", &error));
            }
        }
        Ok(())
    }

    /// Closes the clipboard even when a later format fails to be set.
    struct ClipboardGuard;

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    fn windows_error(step: &str) -> String {
        windows_error_with(step, &windows::core::Error::from_thread())
    }

    fn windows_error_with(step: &str, error: &windows::core::Error) -> String {
        tr_args!(
            "compression.clipboard_windows_failed",
            step = step,
            error = error
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;
        use std::path::{Path, PathBuf};

        #[test]
        fn xdg_session_type_selects_the_backend() {
            assert_eq!(
                linux_session(Some("wayland"), None, None),
                Some(LinuxSession::Wayland)
            );
            assert_eq!(
                linux_session(Some("x11"), None, None),
                Some(LinuxSession::X11)
            );
            assert_eq!(
                linux_session(Some(" Wayland "), None, None),
                Some(LinuxSession::Wayland)
            );
        }

        #[test]
        fn missing_session_type_falls_back_to_display_variables() {
            assert_eq!(
                linux_session(None, Some("wayland-0"), Some(":0")),
                Some(LinuxSession::Wayland)
            );
            assert_eq!(
                linux_session(Some("tty"), Some("wayland-0"), Some(":0")),
                Some(LinuxSession::Wayland)
            );
            assert_eq!(
                linux_session(None, None, Some(":0")),
                Some(LinuxSession::X11)
            );
            assert_eq!(
                linux_session(None, Some(""), Some(":0")),
                Some(LinuxSession::X11)
            );
            assert_eq!(linux_session(None, None, None), None);
            assert_eq!(linux_session(Some("tty"), None, None), None);
        }

        #[test]
        fn wayland_uses_wl_copy_with_png_mime_type() {
            let command = linux_clipboard_command(LinuxSession::Wayland);
            assert_eq!(command.program, "wl-copy");
            assert_eq!(command.args, &["--type", "image/png"]);
        }

        #[test]
        fn x11_uses_xclip_with_png_target() {
            let command = linux_clipboard_command(LinuxSession::X11);
            assert_eq!(command.program, "xclip");
            assert_eq!(
                command.args,
                &["-selection", "clipboard", "-target", "image/png"]
            );
        }

        #[test]
        fn executable_lookup_uses_the_first_matching_path_entry() {
            let path =
                std::env::join_paths([Path::new("/usr/bin"), Path::new("/opt/bin")]).unwrap();
            let found = find_executable("wl-copy", Some(path.as_os_str()), |candidate| {
                candidate == Path::new("/usr/bin/wl-copy")
                    || candidate == Path::new("/opt/bin/wl-copy")
            });
            assert_eq!(found, Some(PathBuf::from("/usr/bin/wl-copy")));
        }

        #[test]
        fn executable_lookup_ignores_empty_path_entries() {
            let path = std::env::join_paths([Path::new(""), Path::new("/usr/bin")]).unwrap();
            let found = find_executable("wl-copy", Some(path.as_os_str()), |candidate| {
                candidate == Path::new("wl-copy")
            });
            assert_eq!(found, None);
        }

        #[test]
        fn executable_lookup_skips_a_non_executable_file() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let candidate = dir.path().join("wl-copy");
            std::fs::write(&candidate, "").unwrap();
            let path = std::env::join_paths([dir.path()]).unwrap();
            assert_eq!(
                find_executable("wl-copy", Some(path.as_os_str()), is_executable_file),
                None
            );

            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                find_executable("wl-copy", Some(path.as_os_str()), is_executable_file),
                Some(candidate)
            );
        }

        #[test]
        fn external_writer_does_not_block_on_a_helper_that_stops_reading() {
            use std::os::unix::fs::PermissionsExt;
            use std::time::{Duration, Instant};

            let dir = tempfile::tempdir().unwrap();
            let no_read = dir.path().join("no-read.sh");
            std::fs::write(&no_read, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&no_read, std::fs::Permissions::from_mode(0o755)).unwrap();

            let png = vec![0u8; 1024 * 1024];
            let start = Instant::now();
            write_command(&no_read, &[], &png).unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "blocked writing to a helper that stopped reading"
            );
        }

        #[test]
        fn external_writer_delivers_png_bytes_and_accepts_success() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let capture = dir.path().join("capture.bin");
            let capture_arg = capture.to_string_lossy().into_owned();
            let ok = dir.path().join("ok.sh");
            std::fs::write(&ok, "#!/bin/sh\ncat > \"$1\"\nexit 0\n").unwrap();
            std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();

            write_command(&ok, &[&capture_arg], b"png-bytes").unwrap();
            assert_eq!(std::fs::read(&capture).unwrap(), b"png-bytes");
        }

        #[test]
        fn external_writer_does_not_wait_for_a_forked_server() {
            use std::os::unix::fs::PermissionsExt;
            use std::time::{Duration, Instant};

            let dir = tempfile::tempdir().unwrap();
            let ok = dir.path().join("fork.sh");
            std::fs::write(&ok, "#!/bin/sh\ncat >/dev/null\nsleep 5 &\nexit 0\n").unwrap();
            std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();

            let start = Instant::now();
            write_command(&ok, &[], b"png").unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "waited for the forked clipboard server"
            );
        }

        #[test]
        fn external_writer_reports_stderr_for_a_non_zero_exit() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let fail = dir.path().join("fail.sh");
            std::fs::write(&fail, "#!/bin/sh\ncat >/dev/null\necho nope >&2\nexit 7\n").unwrap();
            std::fs::set_permissions(&fail, std::fs::Permissions::from_mode(0o755)).unwrap();
            let error = write_command(&fail, &[], b"png").unwrap_err();
            assert!(error.contains('7'), "{error}");
            assert!(error.contains("nope"), "{error}");
        }

        #[test]
        fn external_writer_times_out_a_hung_helper() {
            use std::os::unix::fs::PermissionsExt;
            use std::time::{Duration, Instant};

            let dir = tempfile::tempdir().unwrap();
            let hang = dir.path().join("hang.sh");
            std::fs::write(&hang, "#!/bin/sh\ncat >/dev/null\nexec sleep 5\n").unwrap();
            std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();

            let start = Instant::now();
            let error = write_command_with_timeout(&hang, &[], b"png", Duration::from_millis(100))
                .unwrap_err();
            assert!(error.contains("100"), "{error}");
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "timeout did not stop the helper"
            );
        }
    }

    #[test]
    fn dib_has_a_bitmap_info_header_and_opaque_bgra_pixels() {
        let dib = dib_from_rgba(2, 1, &[255, 0, 0, 0, 0, 255, 0, 0]);
        assert_eq!(&dib[0..4], &40u32.to_le_bytes());
        assert_eq!(&dib[4..8], &2i32.to_le_bytes());
        assert_eq!(&dib[8..12], &1i32.to_le_bytes());
        assert_eq!(&dib[12..14], &1u16.to_le_bytes());
        assert_eq!(&dib[14..16], &32u16.to_le_bytes());
        assert_eq!(&dib[16..20], &0u32.to_le_bytes());
        assert_eq!(&dib[20..24], &8u32.to_le_bytes());
        assert_eq!(&dib[40..], &[0, 0, 255, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn dib_stores_rows_bottom_up() {
        let dib = dib_from_rgba(1, 2, &[255, 0, 0, 255, 0, 0, 255, 255]);
        assert_eq!(&dib[40..], &[255, 0, 0, 255, 0, 0, 255, 255]);
    }
}
