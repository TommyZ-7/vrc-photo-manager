//! File and stderr logging for the daemon binary.
//!
//! Log records are appended to the per-user log file and mirrored to stderr
//! so terminal launches stay debuggable. The two sinks are separate layers:
//! the file is always plain text, while stderr may use ANSI colors when it
//! is a terminal.

use std::fs::File;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Registry};

/// Initializes the global subscriber.
///
/// `RUST_LOG` overrides `default_filter`; an unparsable value falls back to
/// the default. Returns the log file path, or `None` when no data directory
/// or writable log file is available; logging then goes to stderr only and
/// the reason is logged as a warning.
pub fn init(process: &str, default_filter: &str) -> Option<PathBuf> {
    let filter = filter_from(std::env::var("RUST_LOG").ok().as_deref(), default_filter);
    let (file, warning) = match open_file_writer(process) {
        Ok((path, file)) => (Some((path, file)), None),
        Err(reason) => (None, Some(reason)),
    };
    let registry = Registry::default().with(filter);
    match file {
        Some((path, file)) => {
            // The mutex serializes events from every thread, so lines from
            // concurrent tasks never interleave in the file.
            registry
                .with(fmt::layer().with_ansi(false).with_writer(Mutex::new(file)))
                .with(
                    fmt::layer()
                        .with_ansi(io::stderr().is_terminal())
                        .with_writer(io::stderr),
                )
                .init();
            Some(path)
        }
        None => {
            registry
                .with(
                    fmt::layer()
                        .with_ansi(io::stderr().is_terminal())
                        .with_writer(io::stderr),
                )
                .init();
            if let Some(reason) = warning {
                tracing::warn!(%reason, "file logging is unavailable; using stderr only");
            }
            None
        }
    }
}

/// Resolves the effective filter: a parsable `RUST_LOG` value wins, anything
/// else (unset, empty, unparsable) falls back to `default_filter`.
///
/// `ureq` is always forced off. At trace level it logs the full request URI,
/// which contains the webhook token; a user-supplied `RUST_LOG=trace` must not
/// be able to leak it into the log file.
fn filter_from(env_value: Option<&str>, default_filter: &str) -> EnvFilter {
    let filter = env_value
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new(default_filter));
    // The directive is added after the environment one, so it wins when both
    // name the same target (the last equally specific directive is kept).
    filter.add_directive("ureq=off".parse().expect("static ureq directive"))
}

/// Opens the process log file in the default log directory.
fn open_file_writer(process: &str) -> Result<(PathBuf, File), String> {
    let dir = vrc_photon_core::logging::logs_dir()
        .ok_or_else(|| "no data directory for the log file".to_string())?;
    open_file_writer_in(&dir, process)
}

/// Prepares and opens the log file under `dir`, naming the failing step
/// when preparation fails so the caller can log the reason.
fn open_file_writer_in(dir: &Path, process: &str) -> Result<(PathBuf, File), String> {
    let path = vrc_photon_core::logging::prepare_log_file_in(
        dir,
        process,
        vrc_photon_core::logging::MAX_LOG_BYTES,
    )
    .map_err(|error| format!("cannot prepare log file in {}: {error}", dir.display()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("cannot open log file {}: {error}", path.display()))?;
    Ok((path, file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::Arc;

    #[test]
    fn rust_log_wins_when_parsable_and_default_applies_otherwise() {
        let debug = filter_from(Some("debug"), "info").to_string();
        assert!(
            debug.contains("debug") && debug.contains("ureq=off"),
            "{debug}"
        );
        let trace = filter_from(Some("vrc_photonn=trace"), "info").to_string();
        assert!(
            trace.contains("vrc_photonn=trace") && trace.contains("ureq=off"),
            "{trace}"
        );
        // An explicit ureq directive in RUST_LOG is overridden.
        let ureq_trace = filter_from(Some("ureq=trace"), "info").to_string();
        assert!(ureq_trace.contains("ureq=off"), "{ureq_trace}");
        assert!(!ureq_trace.contains("ureq=trace"), "{ureq_trace}");

        let fallback = filter_from(Some("!!!"), "info").to_string();
        assert!(
            fallback.contains("info") && fallback.contains("ureq=off"),
            "{fallback}"
        );
        let default = filter_from(None, "info").to_string();
        assert!(
            default.contains("info") && default.contains("ureq=off"),
            "{default}"
        );
    }

    #[test]
    fn ureq_trace_records_stay_disabled_even_with_rust_log_trace() {
        let filter = filter_from(Some("trace"), "info");
        let buf = SharedBuf::default();
        let subscriber = Registry::default()
            .with(filter)
            .with(fmt::layer().with_ansi(false).with_writer(buf.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(
                target: "ureq",
                "https://discord.com/api/webhooks/1/SECRET"
            );
            tracing::trace!(target: "vrc_photon_daemon", "visible-probe");
        });
        let contents = buf.contents();
        assert!(
            !contents.contains("SECRET"),
            "ureq's full-URI trace record reached the log: {contents}"
        );
        assert!(
            contents.contains("visible-probe"),
            "the trace filter was disabled entirely: {contents}"
        );
    }

    #[test]
    fn file_writer_appends_to_the_prepared_log() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut file) = open_file_writer_in(dir.path(), "ui-test").unwrap();
        assert_eq!(path, dir.path().join("ui-test.log"));
        file.write_all(b"line\n").unwrap();
        drop(file);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line\n");
    }

    #[test]
    fn file_writer_reports_why_preparation_failed() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("not-a-dir");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let error = open_file_writer_in(&not_a_dir, "ui-test").unwrap_err();
        assert!(
            error.contains("cannot prepare log file"),
            "error should name the failing step: {error}"
        );
    }

    /// A `MakeWriter` over a shared buffer, for asserting on emitted logs.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> fmt::writer::MakeWriter<'a> for SharedBuf {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn split_layers_deliver_each_event_to_both_sinks() {
        let file_buf = SharedBuf::default();
        let stderr_buf = SharedBuf::default();
        let subscriber = Registry::default()
            .with(fmt::layer().with_ansi(false).with_writer(file_buf.clone()))
            .with(
                fmt::layer()
                    .with_ansi(false)
                    .with_writer(stderr_buf.clone()),
            );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("dual-sink-probe");
        });
        assert!(
            file_buf.contents().contains("dual-sink-probe"),
            "file sink missed the event: {}",
            file_buf.contents()
        );
        assert!(
            stderr_buf.contents().contains("dual-sink-probe"),
            "stderr sink missed the event: {}",
            stderr_buf.contents()
        );
    }
}
