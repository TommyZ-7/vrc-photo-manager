//! Supervision for long-lived daemon loops.
//!
//! Every long-lived worker is spawned through [`spawn`], which returns an
//! error instead of panicking when the operating system refuses a thread. Once
//! running, the worker loop is wrapped in [`std::panic::catch_unwind`] and
//! restarted with exponential backoff so one bad photo or IPC payload cannot
//! take the whole daemon down.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::DaemonStatus;
use crate::signals::WakeSignal;

/// Restart delays after a worker panic: 1s, 2s, 5s, 10s, then 30s. A worker
/// that ran longer than [`RESET_AFTER`] starts again at the first delay.
const RESTART_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
const RESET_AFTER: Duration = Duration::from_secs(60);

/// Exponential restart delay state shared by workers and the main loop.
#[derive(Debug, Clone)]
pub(crate) struct RestartBackoff {
    delays: Vec<Duration>,
    next: usize,
}

impl RestartBackoff {
    pub(crate) fn new() -> Self {
        Self::with_delays(RESTART_DELAYS.to_vec())
    }

    fn with_delays(delays: Vec<Duration>) -> Self {
        assert!(!delays.is_empty(), "restart delays must not be empty");
        Self { delays, next: 0 }
    }

    /// Returns the next delay and advances the schedule, capping at the last
    /// entry so a permanently broken worker is retried every 30 seconds.
    pub(crate) fn next_delay(&mut self) -> Duration {
        let index = self.next.min(self.delays.len() - 1);
        let delay = self.delays[index];
        self.next = (self.next + 1).min(self.delays.len() - 1);
        delay
    }

    pub(crate) fn reset(&mut self) {
        self.next = 0;
    }
}

/// Extracts the human-readable part of a panic payload.
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Records a restart in the daemon status so the failure stays visible after
/// the log line scrolls away. The status lock is best effort: a panic while it
/// was held must not stop the supervisor itself.
pub(crate) fn record_restart(status: &Arc<Mutex<DaemonStatus>>, worker: &str, message: &str) {
    if let Ok(mut status) = status.lock() {
        status.worker_restarts = status.worker_restarts.saturating_add(1);
        status.last_worker_error = Some(format!("{worker}: {message}"));
    }
}

/// Spawns a supervised long-lived worker. Returns the spawn error so the
/// caller can keep the daemon alive and report the missing worker.
pub(crate) fn spawn<F>(
    name: String,
    running: Arc<AtomicBool>,
    stop: Arc<WakeSignal>,
    status: Arc<Mutex<DaemonStatus>>,
    task: F,
) -> std::io::Result<JoinHandle<()>>
where
    F: FnMut() + Send + 'static,
{
    spawn_with(
        name,
        running,
        stop,
        status,
        RESTART_DELAYS.to_vec(),
        None,
        task,
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_with<F>(
    name: String,
    running: Arc<AtomicBool>,
    stop: Arc<WakeSignal>,
    status: Arc<Mutex<DaemonStatus>>,
    delays: Vec<Duration>,
    stack_size: Option<usize>,
    mut task: F,
) -> std::io::Result<JoinHandle<()>>
where
    F: FnMut() + Send + 'static,
{
    let thread_name = name.clone();
    let mut builder = thread::Builder::new().name(thread_name);
    if let Some(stack_size) = stack_size {
        builder = builder.stack_size(stack_size);
    }
    builder.spawn(move || {
        let mut backoff = RestartBackoff::with_delays(delays);
        while running.load(Ordering::Relaxed) {
            let started = Instant::now();
            let outcome = catch_unwind(AssertUnwindSafe(&mut task));
            if !running.load(Ordering::Relaxed) {
                break;
            }
            let message = match outcome {
                Ok(()) => {
                    // A long-lived loop normally only returns during shutdown.
                    // If it returns early, restart it rather than silently
                    // leaving the daemon without that feature.
                    backoff.reset();
                    "worker loop returned unexpectedly".to_string()
                }
                Err(payload) => {
                    if started.elapsed() >= RESET_AFTER {
                        backoff.reset();
                    }
                    panic_message(payload.as_ref())
                }
            };
            record_restart(&status, &name, &message);
            let delay = backoff.next_delay();
            tracing::error!(
                worker = %name,
                panic = %message,
                restart_in_ms = delay.as_millis(),
                "long-lived worker stopped; restarting after backoff"
            );
            stop.wait(delay);
        }
    })
}

/// Sleeps while `running` stays true, checking every 100 ms so shutdown does
/// not wait for a full 30-second restart delay. Used by the supervised main
/// loop, which already owns the shared wake signal for IPC traffic.
pub(crate) fn wait_while_running(running: &AtomicBool, delay: Duration) {
    let deadline = Instant::now() + delay;
    while running.load(Ordering::Relaxed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn test_status() -> Arc<Mutex<DaemonStatus>> {
        Arc::new(Mutex::new(DaemonStatus::default()))
    }

    #[test]
    fn restart_backoff_escalates_and_caps_at_thirty_seconds() {
        let mut backoff = RestartBackoff::new();
        let delays: Vec<Duration> = (0..7).map(|_| backoff.next_delay()).collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ]
        );
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn panic_message_handles_str_and_string_payloads() {
        let text = "static failure";
        assert_eq!(panic_message(&text), "static failure");
        let owned = "owned failure".to_string();
        assert_eq!(panic_message(&owned), "owned failure");
        assert_eq!(panic_message(&42_u32), "non-string panic payload");
    }

    #[test]
    fn spawn_failure_is_returned_instead_of_panicking() {
        let running = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(WakeSignal::default());
        let result = spawn_with(
            "bad-stack".to_string(),
            running,
            stop,
            test_status(),
            vec![Duration::from_millis(1)],
            Some(usize::MAX),
            || {},
        );
        assert!(result.is_err(), "an impossible stack must report an error");
    }

    #[test]
    fn panic_restarts_the_worker_and_keeps_the_supervisor_alive() {
        let running = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(WakeSignal::default());
        let status = test_status();
        let calls = Arc::new(AtomicUsize::new(0));
        let task_running = running.clone();
        let task_calls = calls.clone();
        let handle = spawn_with(
            "panic-test".to_string(),
            running.clone(),
            stop,
            status.clone(),
            vec![Duration::from_millis(10)],
            None,
            move || {
                let call = task_calls.fetch_add(1, Ordering::SeqCst);
                if call < 2 {
                    panic!("worker boom");
                }
                task_running.store(false, Ordering::Relaxed);
            },
        )
        .unwrap();
        handle.join().unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 3, "two panics then success");
        let status = status.lock().unwrap();
        assert_eq!(status.worker_restarts, 2);
        assert!(
            status
                .last_worker_error
                .as_deref()
                .is_some_and(|message| message.contains("worker boom")),
            "{:?}",
            status.last_worker_error
        );
    }

    #[test]
    fn stop_interrupts_a_restart_wait() {
        let running = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(WakeSignal::default());
        let status = test_status();
        let started = Instant::now();
        let handle = spawn_with(
            "stop-test".to_string(),
            running.clone(),
            stop.clone(),
            status,
            vec![Duration::from_secs(30)],
            None,
            || panic!("always"),
        )
        .unwrap();

        // Let the first panic happen and the supervisor enter its 30s wait.
        thread::sleep(Duration::from_millis(100));
        running.store(false, Ordering::Relaxed);
        stop.notify();
        handle.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown must not wait for the restart timer"
        );
    }
}
