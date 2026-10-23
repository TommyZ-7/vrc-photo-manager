//! File and stderr logging for the UI binary.
//!
//! Log records are appended to the per-user log file and mirrored to stderr so
//! terminal launches stay debuggable.

/// Initializes the global subscriber.
///
/// `RUST_LOG` overrides `default_filter`. Returns the log file path, or `None`
/// when no data directory or writable log file is available; logging then goes
/// to stderr only.
pub fn init(process: &str, default_filter: &str) -> Option<std::path::PathBuf> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    // Log files are plain text; never write terminal escape sequences to them.
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    match vrc_photo_core::logging::prepare_log_file(process)
        .ok()
        .flatten()
        .and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
                .map(|file| (path, file))
        }) {
        Some((path, file)) => {
            use tracing_subscriber::fmt::writer::MakeWriterExt as _;
            builder
                .with_writer(std::sync::Mutex::new(file).and(std::io::stderr))
                .init();
            Some(path)
        }
        None => {
            builder.with_writer(std::io::stderr).init();
            None
        }
    }
}
