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

/// Opens the daemon with deletion recovery only when this process owns the
/// single-instance lock. Maintenance runs that lose the lock still proceed,
/// but leave running deletions and execution to the owner.
fn open_daemon(
    owns_instance: bool,
    db_path: PathBuf,
    photo_dir: Option<PathBuf>,
    log_dir: Option<PathBuf>,
) -> Result<Daemon> {
    if owns_instance {
        Daemon::open(db_path, photo_dir, log_dir)
    } else {
        Daemon::open_preserving_running_deletions(db_path, photo_dir, log_dir)
    }
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
    let maintenance = args.once || args.compress_pending;
    let instance = match single_instance::SingleInstance::new("vrc-photo-manager-daemon") {
        Ok(instance) if instance.is_single() => Some(instance),
        Ok(_) if maintenance => {
            tracing::info!(
                "another instance owns the single-instance lock; preserving its running deletions"
            );
            None
        }
        Ok(_) => {
            eprintln!("vrc-photo-daemon is already running");
            return Ok(());
        }
        Err(error) if !maintenance => {
            return Err(error).context("cannot acquire single-instance lock");
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "cannot acquire single-instance lock; continuing without deletion recovery"
            );
            None
        }
    };

    let mut daemon = open_daemon(instance.is_some(), db_path, args.photo_dir, args.log_dir)
        .context("cannot start VRC Photo Manager daemon")?;
    if args.no_tray {
        daemon.set_tray_enabled(false);
    }

    if maintenance {
        daemon.run_once()?;
        if args.compress_pending {
            daemon.compress_pending()?;
        }
        return Ok(());
    }

    daemon.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vrc_photo_core::db::Db;
    use vrc_photo_core::model::{DeletionReason, DeletionStatus, NewPhoto};

    fn seed_running_deletion(db_path: &std::path::Path) -> i64 {
        let db = Db::open(db_path).unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: "a.png".to_string(),
                abs_path: "/photos/a.png".to_string(),
                file_name: "a.png".to_string(),
                captured_at_utc: 1_700_000_000,
                captured_at_local: "2026-10-21 21:14:03".to_string(),
                tz_offset_sec: 9 * 3600,
                width: 16,
                height: 10,
                file_size: 10,
                mtime: 1_700_000_000,
                blake3: None,
            })
            .unwrap();
        let request = db
            .request_deletion(&[photo], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.claim_deletion(request).unwrap().is_some());
        photo
    }

    #[test]
    fn open_daemon_preserves_running_deletions_without_instance_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let photo = seed_running_deletion(&db_path);

        let preserving = open_daemon(false, db_path.clone(), None, None).unwrap();
        let db = Db::open(&db_path).unwrap();
        assert_eq!(
            db.active_deletion_request(photo).unwrap().unwrap().status,
            DeletionStatus::Running
        );
        drop(preserving);

        let _owning = open_daemon(true, db_path.clone(), None, None).unwrap();
        let db = Db::open(&db_path).unwrap();
        assert_eq!(
            db.active_deletion_request(photo).unwrap().unwrap().status,
            DeletionStatus::Pending
        );
    }
}
