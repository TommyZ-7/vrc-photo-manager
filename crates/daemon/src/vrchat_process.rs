//! Detects whether the VRChat client process is currently running.
//!
//! Detection is intentionally one-way: the daemon uses a missing process as a
//! reason to close the active session, but never uses a live process to open or
//! keep one open. `None` means the process table could not be read and must not
//! count as a miss.

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn is_vrchat_comm(comm: &str) -> bool {
    comm.trim() == "VRChat.exe"
}

/// Returns `Some(true)` when `VRChat.exe` is running, `Some(false)` when the
/// process list was read and it is absent, and `None` when detection failed.
#[cfg(target_os = "linux")]
pub fn is_running() -> Option<bool> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let Some(pid) = name.to_str() else {
            continue;
        };
        if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        match std::fs::read_to_string(entry.path().join("comm")) {
            Ok(comm) => {
                if is_vrchat_comm(&comm) {
                    return Some(true);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // A process we cannot inspect may be VRChat. Never turn a partial
            // process-table read into a "not running" verdict.
            Err(_) => return None,
        }
    }
    Some(false)
}

/// Returns `Some(true)` when `VRChat.exe` is running, `Some(false)` when the
/// process list was read and it is absent, and `None` when detection failed.
#[cfg(target_os = "windows")]
pub fn is_running() -> Option<bool> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let result = match unsafe { Process32FirstW(snapshot, &mut entry) } {
        Ok(()) => loop {
            let len = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if is_vrchat_comm(&name) {
                break Some(true);
            }
            match unsafe { Process32NextW(snapshot, &mut entry) } {
                Ok(()) => {}
                Err(error) if is_end_of_process_list(&error) => break Some(false),
                Err(_) => break None,
            }
        },
        Err(error) if is_end_of_process_list(&error) => Some(false),
        Err(_) => None,
    };
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    result
}

#[cfg(target_os = "windows")]
fn is_end_of_process_list(error: &windows::core::Error) -> bool {
    error.code()
        == windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_NO_MORE_FILES.0)
}

/// Other platforms do not ship a supported VRChat process table reader.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn is_running() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::is_vrchat_comm;

    #[test]
    fn matches_vrchat_comm_with_line_endings() {
        assert!(is_vrchat_comm("VRChat.exe\n"));
        assert!(is_vrchat_comm("VRChat.exe"));
        assert!(!is_vrchat_comm("vrchat.exe"));
        assert!(!is_vrchat_comm("VRChat.exe.old"));
        assert!(!is_vrchat_comm("wine64-preloader"));
    }
}
