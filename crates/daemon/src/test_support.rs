//! Helpers shared by the daemon's in-crate test modules.
//!
//! Cargo binaries used by tests must only be built once per test process. Each
//! test module used to keep its own `OnceLock`, so two modules could run
//! `cargo build` at the same time while another test was already executing the
//! binary, which fails with `ETXTBSY` ("Text file busy") on Linux.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use assert_cmd::cargo::cargo_bin;

static MOCK_SIDECAR_BUILD: Mutex<()> = Mutex::new(());
static MOCK_SIDECAR: OnceLock<PathBuf> = OnceLock::new();

/// Initializes `cell` exactly once, serializing concurrent builders with
/// `lock`.
///
/// The fast path avoids taking the mutex after initialization. Callers that
/// miss the fast path still serialize on the mutex, then re-check the cell so
/// only the first one runs `init`.
pub(crate) fn init_once<T: Clone>(
    lock: &'static Mutex<()>,
    cell: &'static OnceLock<T>,
    init: impl FnOnce() -> T,
) -> T {
    if let Some(value) = cell.get() {
        return value.clone();
    }
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    cell.get_or_init(init).clone()
}

/// Returns the mock sidecar binary, building it once per test process.
///
/// The build is guarded by [`MOCK_SIDECAR_BUILD`] so no test can execute the
/// binary while a concurrent build is rewriting it.
pub(crate) fn mock_sidecar() -> PathBuf {
    init_once(&MOCK_SIDECAR_BUILD, &MOCK_SIDECAR, || {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "--package", "vrc-photon-ai-mock-sidecar"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("build the mock sidecar");
        assert!(status.success(), "cargo build failed for the mock sidecar");
        cargo_bin("mock-sidecar")
    })
}

#[cfg(test)]
mod tests {
    use super::{init_once, mock_sidecar};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, OnceLock};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn concurrent_callers_run_the_initializer_once() {
        static LOCK: Mutex<()> = Mutex::new(());
        static CELL: OnceLock<usize> = OnceLock::new();
        static BUILDS: AtomicUsize = AtomicUsize::new(0);

        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    init_once(&LOCK, &CELL, || {
                        BUILDS.fetch_add(1, Ordering::SeqCst);
                        // Give other callers time to reach the lock while the
                        // first initializer is still running.
                        thread::sleep(Duration::from_millis(50));
                        42
                    })
                })
            })
            .collect();

        for handle in handles {
            assert_eq!(handle.join().unwrap(), 42);
        }
        assert_eq!(
            BUILDS.load(Ordering::SeqCst),
            1,
            "the initializer must run exactly once across all threads"
        );
    }

    #[test]
    fn mock_sidecar_is_the_same_usable_path_from_many_threads() {
        let handles: Vec<_> = (0..8).map(|_| thread::spawn(mock_sidecar)).collect();
        let paths: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        let first = &paths[0];
        assert!(first.is_file(), "mock sidecar must exist: {first:?}");
        assert!(
            std::fs::read(first).is_ok_and(|bytes| !bytes.is_empty()),
            "mock sidecar must be a readable, non-empty binary: {first:?}"
        );
        for path in &paths[1..] {
            assert_eq!(path, first, "every caller must see the same build");
        }
    }
}
