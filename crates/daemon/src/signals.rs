use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Wakes the daemon's main loop when IPC work arrives.
#[derive(Debug, Default)]
pub struct WakeSignal {
    pending: Mutex<bool>,
    condvar: Condvar,
}

impl WakeSignal {
    pub fn notify(&self) {
        *self.pending.lock().unwrap() = true;
        self.condvar.notify_all();
    }

    /// Waits for a notification or the timeout, then consumes the flag.
    pub fn wait(&self, timeout: Duration) {
        let mut pending = self.pending.lock().unwrap();
        if !*pending {
            let (guard, _) = self.condvar.wait_timeout(pending, timeout).unwrap();
            pending = guard;
        }
        *pending = false;
    }

    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        *self.pending.lock().unwrap()
    }
}

/// Wakes compression workers when a job is enqueued.
#[derive(Debug, Default)]
pub struct JobSignal {
    generation: Mutex<u64>,
    condvar: Condvar,
}

impl JobSignal {
    pub fn notify(&self) {
        let mut generation = self.generation.lock().unwrap();
        *generation = generation.wrapping_add(1);
        self.condvar.notify_all();
    }

    /// Returns `true` when the generation changed. Each worker keeps its own
    /// `seen` value, so one notification wakes every worker.
    pub fn wait(&self, seen: &mut u64, timeout: Duration) -> bool {
        let generation = self.generation.lock().unwrap();
        if *generation != *seen {
            *seen = *generation;
            return true;
        }
        let (generation, _) = self.condvar.wait_timeout(generation, timeout).unwrap();
        let changed = *generation != *seen;
        if changed {
            *seen = *generation;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn wake_signal_consumes_one_notification() {
        let signal = WakeSignal::default();
        assert!(!signal.is_pending());
        signal.notify();
        assert!(signal.is_pending());
        signal.wait(Duration::from_millis(1));
        assert!(!signal.is_pending());
    }

    #[test]
    fn job_signal_wakes_each_worker_generation() {
        let signal = JobSignal::default();
        let mut seen = 0;
        signal.notify();
        assert!(signal.wait(&mut seen, Duration::from_millis(1)));
        assert!(!signal.wait(&mut seen, Duration::from_millis(1)));
    }
}
