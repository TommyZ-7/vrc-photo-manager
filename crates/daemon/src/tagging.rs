//! Cancellation and wake-up plumbing shared by the tagging IPC handlers and
//! the tagging worker.
//!
//! The worker itself lands in a later task; this module only owns the
//! registration table that lets `CancelTagging` reach an in-flight sidecar
//! request.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use vrc_photon_ai_host::CancelToken;

use crate::signals::JobSignal;

/// Tracks one cancel token per running tagging job and wakes the worker.
pub(crate) struct TaggingControl {
    signal: Arc<JobSignal>,
    tokens: Mutex<HashMap<String, CancelToken>>,
}

impl TaggingControl {
    pub(crate) fn new(signal: Arc<JobSignal>) -> Self {
        Self {
            signal,
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a job before the worker issues its first sidecar request.
    /// Re-registering a job replaces its previous token.
    pub(crate) fn register(&self, job_id: &str) -> CancelToken {
        let token = CancelToken::default();
        lock_unpoisoned(&self.tokens).insert(job_id.to_string(), token.clone());
        token
    }

    /// Drops the token once the job reached a terminal state.
    pub(crate) fn unregister(&self, job_id: &str) {
        lock_unpoisoned(&self.tokens).remove(job_id);
    }

    /// Cancels the token for a running job. Returns false when the job has no
    /// registered token (for example a waiting job); the database row is the
    /// authoritative state either way.
    pub(crate) fn cancel(&self, job_id: &str) -> bool {
        match lock_unpoisoned(&self.tokens).get(job_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// The wake signal shared with the tagging worker.
    pub(crate) fn signal(&self) -> &Arc<JobSignal> {
        &self.signal
    }

    /// Nudges the tagging worker.
    pub(crate) fn notify(&self) {
        self.signal.notify();
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cancel_only_affects_registered_jobs() {
        let signal = Arc::new(JobSignal::default());
        let control = TaggingControl::new(signal);
        assert!(!control.cancel("missing"));

        let token = control.register("job-1");
        assert!(!token.is_cancelled());
        assert!(control.cancel("job-1"));
        assert!(token.is_cancelled());

        control.unregister("job-1");
        assert!(!control.cancel("job-1"));
    }

    #[test]
    fn notify_wakes_the_shared_signal() {
        let signal = Arc::new(JobSignal::default());
        let control = TaggingControl::new(signal.clone());
        let mut seen = 0;
        control.notify();
        assert!(signal.wait(&mut seen, Duration::from_millis(1)));
    }
}
