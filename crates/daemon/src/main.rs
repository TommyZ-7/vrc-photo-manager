use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use vrc_photo_daemon::Daemon;

mod logging;

#[derive(Debug, Parser)]
#[command(
    name = "vrc-photo-daemon",
    version,
    about = "VRC Photo Manager background daemon"
)]
struct Args {
    /// Path to the SQLite library database.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Run one scan/log cycle, then exit.
    #[arg(long)]
    once: bool,
    /// Override the VRChat photo folder.
    #[arg(long)]
    photo_dir: Option<PathBuf>,
    /// Override the VRChat output-log folder.
    #[arg(long)]
    log_dir: Option<PathBuf>,
    /// Process all pending compression jobs and exit (implies --once unless run alone).
    #[arg(long)]
    compress_pending: bool,
    /// Disable the tray icon and run headless.
    #[arg(long)]
    no_tray: bool,
    /// Print the resolved data directory and exit.
    #[arg(long)]
    print_data_dir: bool,
}

fn main() -> Result<()> {
    if logging::init("vrc-photo-daemon", "info,vrc_photo_daemon=debug").is_none() {
        tracing::warn!("file logging is unavailable; using stderr only");
    }

    let args = Args::parse();
    if args.print_data_dir {
        println!(
            "{}",
            vrc_photo_core::data_dir().unwrap_or_default().display()
        );
        return Ok(());
    }

    let db_path = args
        .db
        .or_else(vrc_photo_core::default_db_path)
        .unwrap_or_else(|| PathBuf::from("vrc-photo-manager.db"));
    let _instance = if args.once || args.compress_pending {
        None
    } else {
        match single_instance::SingleInstance::new("vrc-photo-manager-daemon") {
            Ok(instance) if instance.is_single() => Some(instance),
            Ok(_) => {
                eprintln!("vrc-photo-daemon is already running");
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(%error, "cannot acquire single-instance lock; continuing");
                None
            }
        }
    };

    let mut daemon = Daemon::open(db_path, args.photo_dir, args.log_dir)
        .context("cannot start VRC Photo Manager daemon")?;
    if args.no_tray {
        daemon.set_tray_enabled(false);
    }

    if args.once || args.compress_pending {
        daemon.run_once()?;
        if args.compress_pending {
            daemon.compress_pending()?;
        }
        return Ok(());
    }

    daemon.run()
}
