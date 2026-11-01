#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::Path;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let command = args
        .next()
        .and_then(|value| value.to_str().map(str::to_string));
    let path = args.next();
    match (command.as_deref(), path) {
        (Some("--apply"), Some(path)) => {
            if !vrc_photo_update::helper::user_mode_allowed(
                vrc_photo_update::helper::effective_uid(),
            ) {
                eprintln!("vrc-photo-updater: --apply must not run as root");
                std::process::exit(10);
            }
            if let Err(error) = vrc_photo_update::helper::run_helper(Path::new(&path)) {
                eprintln!("vrc-photo-updater: {error}");
                std::process::exit(error.helper_exit_code());
            }
        }
        (Some("--apply-system-package"), Some(path)) => {
            let pkexec_uid = std::env::var("PKEXEC_UID")
                .ok()
                .and_then(|value| value.parse::<u32>().ok());
            if !vrc_photo_update::helper::system_mode_allowed(
                vrc_photo_update::helper::effective_uid(),
                pkexec_uid,
            ) {
                eprintln!("vrc-photo-updater: --apply-system-package requires pkexec");
                std::process::exit(10);
            }
            let options = vrc_photo_update::helper::HelperOptions::default();
            if let Err(error) =
                vrc_photo_update::helper::run_system_package(Path::new(&path), &options)
            {
                eprintln!("vrc-photo-updater: {error}");
                std::process::exit(error.helper_exit_code());
            }
        }
        _ => {
            eprintln!("usage: vrc-photo-updater --apply <job.json>");
            eprintln!("       vrc-photo-updater --apply-system-package <job.json>");
            std::process::exit(2);
        }
    }
}
