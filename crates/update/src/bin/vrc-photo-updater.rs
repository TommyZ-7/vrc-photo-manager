#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::Path;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let command = args.next();
    let path = args.next();
    let is_apply = command.as_deref().and_then(|value| value.to_str()) == Some("--apply");
    match (is_apply, path) {
        (true, Some(path)) => {
            if let Err(error) = vrc_photo_update::helper::run_helper(Path::new(&path)) {
                eprintln!("vrc-photo-updater: {error}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: vrc-photo-updater --apply <job.json>");
            std::process::exit(2);
        }
    }
}
