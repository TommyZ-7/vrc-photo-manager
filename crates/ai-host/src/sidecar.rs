//! Spawning, handshaking with, and probing the AI sidecar process.
//!
//! The sidecar speaks JSON Lines over stdin/stdout. Its stderr is free-form log
//! output and is appended to the host-provided log file. Every read waits on a
//! reader thread with a timeout so a wedged sidecar cannot block the daemon
//! forever.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Write, copy};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use uuid::Uuid;
use vrc_photon_ai_protocol::{
    Hello, ImageRef, Op, PROTOCOL_VERSION, Probe, ProtocolError, SidecarError, SidecarErrorCode,
    SidecarMessage, SidecarRequest, SidecarResult, TagBranch, read_message, write_message,
};

use crate::error::{AiHostError, Result};

/// How often a blocked request re-checks its [`CancelToken`].
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[cfg(target_os = "windows")]
const LIBRARY_PATH_VAR: &str = "PATH";
#[cfg(not(target_os = "windows"))]
const LIBRARY_PATH_VAR: &str = "LD_LIBRARY_PATH";

/// File-system layout of an installed sidecar package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarPaths {
    /// Directory the sidecar treats as its working directory.
    pub install_dir: PathBuf,
    /// The sidecar executable (or the test mock).
    pub executable: PathBuf,
    /// `catalog.json` shipped inside the package.
    pub catalog: PathBuf,
    /// `calibration.json` shipped inside the package.
    pub calibration: PathBuf,
}

/// The model files passed to the sidecar at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPaths {
    pub model: PathBuf,
    pub mmproj: PathBuf,
}

/// Everything needed to launch one sidecar process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarConfig {
    pub paths: SidecarPaths,
    pub model: ModelPaths,
    pub variant: String,
    pub backend: String,
    pub plugin_id: String,
    /// When set, the `hello` catalog version must match this value.
    pub catalog_version: Option<String>,
    pub log_path: PathBuf,
    /// Test-only environment overrides applied to the child process.
    ///
    /// This is a test seam, not a production configuration surface:
    /// production callers must leave it empty. The integration tests use it
    /// only to configure the mock sidecar through `MOCK_SIDECAR_*` variables.
    pub env: Vec<(String, String)>,
}

/// Identity fields a `--probe` result must advertise.
///
/// The variant is always checked; the plugin id and catalog version are
/// checked when the caller knows them. [`SidecarClient::probe`] uses the
/// variant-only form so existing callers keep working, while the daemon can
/// use [`SidecarClient::probe_with_expectations`] for the full identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeExpectations {
    pub plugin_id: Option<String>,
    pub variant: String,
    pub catalog_version: Option<String>,
}

impl ProbeExpectations {
    /// Checks the variant only.
    pub fn variant(variant: impl Into<String>) -> Self {
        Self {
            plugin_id: None,
            variant: variant.into(),
            catalog_version: None,
        }
    }

    /// Checks the plugin id and variant.
    pub fn new(plugin_id: impl Into<String>, variant: impl Into<String>) -> Self {
        Self {
            plugin_id: Some(plugin_id.into()),
            variant: variant.into(),
            catalog_version: None,
        }
    }

    /// Also checks the advertised catalog version.
    pub fn with_catalog_version(mut self, catalog_version: impl Into<String>) -> Self {
        self.catalog_version = Some(catalog_version.into());
        self
    }
}

/// Timeouts for each phase of the sidecar lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidecarTimeouts {
    /// Model loading can be slow on first start.
    pub hello: Duration,
    pub request: Duration,
    pub cancel_ack: Duration,
    pub shutdown: Duration,
}

impl Default for SidecarTimeouts {
    fn default() -> Self {
        Self {
            hello: Duration::from_secs(60),
            request: Duration::from_secs(120),
            cancel_ack: Duration::from_secs(2),
            shutdown: Duration::from_secs(5),
        }
    }
}

/// A running sidecar process that has completed the hello handshake.
pub struct SidecarClient {
    inner: Arc<SidecarInner>,
}

impl std::fmt::Debug for SidecarClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SidecarClient")
            .field("variant", &self.inner.hello.variant)
            .field("alive", &self.is_alive())
            .finish()
    }
}

/// A cancellation flag shared between the IPC handler and an in-flight request.
///
/// The token must be checked by the request loop: the caller can cancel from
/// another thread while `request` is blocked waiting for the sidecar.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Requests cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Reports whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The in-flight request and a cancel that belongs to exactly that request.
///
/// `cancel_current` records an id instead of writing to stdin. The request
/// loop consumes the pending cancel only when the id matches its own, so a
/// late `cancel_current` cannot cancel the next request.
#[derive(Debug, Default)]
struct CurrentRequest {
    id: Option<String>,
    cancel: Option<String>,
}

impl CurrentRequest {
    /// Publishes the request that is about to be written to the sidecar.
    fn begin(&mut self, id: &str) {
        self.id = Some(id.to_string());
        self.cancel = None;
    }

    /// Records a cancel for `id`; ignored once another request is current.
    fn mark_cancel(&mut self, id: &str) {
        if self.id.as_deref() == Some(id) {
            self.cancel = Some(id.to_string());
        }
    }

    /// Consumes a pending cancel only when it belongs to `id`.
    fn take_cancel(&mut self, id: &str) -> bool {
        if self.id.as_deref() == Some(id) && self.cancel.as_deref() == Some(id) {
            self.cancel = None;
            true
        } else {
            false
        }
    }

    /// Clears the in-flight request; a stale id is ignored.
    fn finish(&mut self, id: &str) {
        if self.id.as_deref() == Some(id) {
            self.clear();
        }
    }

    fn clear(&mut self) {
        self.id = None;
        self.cancel = None;
    }
}

struct SidecarInner {
    hello: Hello,
    timeouts: SidecarTimeouts,
    /// The child's stdin. Only the request loop writes to it.
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    /// The sidecar handles one request at a time; serialise them here.
    request_lock: Mutex<()>,
    /// The request currently in flight and its id-scoped pending cancel.
    current: Mutex<CurrentRequest>,
    /// Per-request response channels keyed by request id.
    pending: Mutex<HashMap<String, Sender<std::result::Result<SidecarMessage, RequestFailure>>>>,
    /// The waiter for an outstanding `shutdown`.
    shutdown: Mutex<Option<Sender<std::result::Result<(), RequestFailure>>>>,
    /// Cleared as soon as the sidecar can no longer serve requests.
    alive: AtomicBool,
    reader: Mutex<Option<JoinHandle<()>>>,
    stderr_logger: Mutex<Option<JoinHandle<()>>>,
}

impl SidecarInner {
    fn write(&self, request: &SidecarRequest) -> Result<()> {
        let mut stdin = lock_unpoisoned(&self.stdin);
        write_message(&mut *stdin, request).map_err(protocol_or_io_error)
    }

    fn deliver(&self, id: &str, message: SidecarMessage) {
        let sender = lock_unpoisoned(&self.pending).remove(id);
        if let Some(sender) = sender {
            let _ = sender.send(Ok(message));
        }
    }

    /// Clears the bookkeeping for a request that just finished.
    ///
    /// Returns whether `cancel_current` recorded a cancel before the request
    /// finished, so an already-delivered result can still be discarded.
    fn finish_request(&self, id: &str) -> bool {
        lock_unpoisoned(&self.pending).remove(id);
        let mut current = lock_unpoisoned(&self.current);
        let cancelled = current.take_cancel(id);
        current.finish(id);
        cancelled
    }

    fn fail_all(&self, failure: RequestFailure) {
        let senders: Vec<_> = {
            let mut pending = lock_unpoisoned(&self.pending);
            pending.drain().map(|(_, sender)| sender).collect()
        };
        for sender in senders {
            let _ = sender.send(Err(failure.clone()));
        }
    }

    fn complete_shutdown(&self, outcome: std::result::Result<(), RequestFailure>) {
        let waiter = lock_unpoisoned(&self.shutdown).take();
        if let Some(waiter) = waiter {
            let _ = waiter.send(outcome);
        }
    }

    fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    fn kill_child(&self) {
        let mut child = lock_unpoisoned(&self.child);
        kill_and_wait(&mut child);
    }

    /// Reaps an exited child, and kills it if it closed stdout but kept running.
    fn reap_child(&self) {
        let mut child = lock_unpoisoned(&self.child);
        match child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => kill_and_wait(&mut child),
            Err(_) => {}
        }
    }

    fn wait_for_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut child = lock_unpoisoned(&self.child);
                match child.try_wait() {
                    Ok(Some(_)) => return true,
                    Ok(None) => {}
                    Err(_) => return false,
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Why a request could not produce a response.
#[derive(Debug, Clone)]
enum RequestFailure {
    Exited,
    Protocol(String),
    Fatal(SidecarError),
}

impl RequestFailure {
    fn into_error(self) -> AiHostError {
        match self {
            Self::Exited => AiHostError::SidecarExited,
            Self::Protocol(message) => AiHostError::SidecarProtocol(message),
            Self::Fatal(error) => AiHostError::SidecarFatal(error),
        }
    }
}

enum ReaderEvent {
    Message(Box<SidecarMessage>),
    Failed(String),
    Closed,
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn protocol_or_io_error(error: ProtocolError) -> AiHostError {
    match error {
        ProtocolError::Io(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            AiHostError::SidecarExited
        }
        ProtocolError::Io(error) => AiHostError::Io(error),
        other => AiHostError::SidecarProtocol(other.to_string()),
    }
}

/// Append-only sink for stdout protocol violations.
///
/// The sidecar writes protocol messages to stdout and free-form logs to
/// stderr. When stdout contains something that is not a protocol message, the
/// daemon must both fail the operation and leave the evidence in the same
/// `sidecar.log` that receives stderr.
struct ProtocolLog {
    file: Mutex<File>,
}

impl ProtocolLog {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    fn record(&self, message: &str) {
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{message}");
            let _ = file.flush();
        }
    }
}

impl SidecarClient {
    /// Starts the sidecar and waits for its `hello` message.
    pub fn spawn(config: SidecarConfig) -> Result<Self> {
        Self::spawn_with_timeouts(config, SidecarTimeouts::default())
    }

    /// Starts the sidecar with explicit timeouts (tests and diagnostics).
    pub fn spawn_with_timeouts(config: SidecarConfig, timeouts: SidecarTimeouts) -> Result<Self> {
        let mut command = sidecar_command(&config);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let protocol_log = ProtocolLog::open(&config.log_path)?;
        let mut child = command.spawn()?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                kill_and_wait(&mut child);
                return Err(AiHostError::SidecarProtocol(
                    "sidecar stdin was not piped".to_string(),
                ));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                kill_and_wait(&mut child);
                return Err(AiHostError::SidecarProtocol(
                    "sidecar stdout was not piped".to_string(),
                ));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                kill_and_wait(&mut child);
                return Err(AiHostError::SidecarProtocol(
                    "sidecar stderr was not piped".to_string(),
                ));
            }
        };
        let stderr_logger = match spawn_stderr_logger(stderr, &config.log_path) {
            Ok(logger) => logger,
            Err(error) => {
                kill_and_wait(&mut child);
                return Err(error);
            }
        };

        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || reader_loop(stdout, sender, protocol_log));

        let startup = match receiver.recv_timeout(timeouts.hello) {
            Ok(ReaderEvent::Message(message)) => match *message {
                SidecarMessage::Hello(hello) => {
                    if hello.protocol_version != PROTOCOL_VERSION {
                        Err(AiHostError::UnsupportedProtocol(hello.protocol_version))
                    } else {
                        validate_hello(&config, &hello).map(|()| hello)
                    }
                }
                SidecarMessage::Fatal { error } => Err(AiHostError::SidecarFatal(error)),
                other => Err(AiHostError::SidecarProtocol(format!(
                    "expected hello, got {}",
                    message_kind(&other)
                ))),
            },
            Ok(ReaderEvent::Failed(message)) => Err(AiHostError::SidecarProtocol(message)),
            Ok(ReaderEvent::Closed) => Err(AiHostError::SidecarExited),
            Err(RecvTimeoutError::Timeout) => Err(AiHostError::SidecarTimeout),
            Err(RecvTimeoutError::Disconnected) => Err(AiHostError::SidecarExited),
        };

        match startup {
            Ok(hello) => {
                let inner = Arc::new(SidecarInner {
                    hello,
                    timeouts,
                    stdin: Mutex::new(stdin),
                    child: Mutex::new(child),
                    request_lock: Mutex::new(()),
                    current: Mutex::new(CurrentRequest::default()),
                    pending: Mutex::new(HashMap::new()),
                    shutdown: Mutex::new(None),
                    alive: AtomicBool::new(true),
                    reader: Mutex::new(Some(reader)),
                    stderr_logger: Mutex::new(Some(stderr_logger)),
                });
                spawn_dispatcher(Arc::downgrade(&inner), receiver);
                Ok(Self { inner })
            }
            Err(error) => {
                kill_and_wait(&mut child);
                let _ = reader.join();
                let _ = stderr_logger.join();
                Err(error)
            }
        }
    }

    /// Runs one operation on the sidecar and waits for its response.
    ///
    /// Requests are serialised because protocol v1 handles one at a time.
    /// Cancellation is cooperative: the token is polled while waiting, a
    /// `cancel` line is sent, and the child is force-stopped if no `cancelled`
    /// arrives within the cancel-ack timeout. A request that exceeds its own
    /// timeout is also force-stopped, so a wedged model cannot poison the next
    /// request.
    pub fn request(
        &self,
        op: Op,
        image: &ImageRef,
        branch: Option<TagBranch>,
        cancel: &CancelToken,
    ) -> Result<SidecarResult> {
        let _request = lock_unpoisoned(&self.inner.request_lock);

        if cancel.is_cancelled() {
            return Err(AiHostError::Cancelled);
        }
        if !self.is_alive() {
            return Err(AiHostError::SidecarExited);
        }

        let id = Uuid::new_v4().to_string();
        let (sender, receiver) = mpsc::channel();
        lock_unpoisoned(&self.inner.pending).insert(id.clone(), sender);

        let request = SidecarRequest::Request {
            id: id.clone(),
            op,
            image: image.clone(),
            branch,
        };
        {
            // Publishing the id and writing the request under one lock keeps
            // `cancel_current` from targeting a request line that has not
            // reached the sidecar yet.
            let mut current = lock_unpoisoned(&self.inner.current);
            current.begin(&id);
            if let Err(error) = self.inner.write(&request) {
                current.clear();
                drop(current);
                self.kill();
                return Err(error);
            }
        }

        let request_deadline = Instant::now() + self.inner.timeouts.request;
        let mut cancel_sent_at: Option<Instant> = None;

        loop {
            let now = Instant::now();

            let external_cancel = lock_unpoisoned(&self.inner.current).take_cancel(&id);
            if cancel_sent_at.is_none() && (external_cancel || cancel.is_cancelled()) {
                let cancel_line = SidecarRequest::Cancel { id: id.clone() };
                match self.inner.write(&cancel_line) {
                    Ok(()) => cancel_sent_at = Some(Instant::now()),
                    Err(_) => {
                        self.kill();
                        return Err(AiHostError::SidecarExited);
                    }
                }
            }

            let cancel_deadline = cancel_sent_at.map(|sent| sent + self.inner.timeouts.cancel_ack);
            let deadline =
                cancel_deadline.map_or(request_deadline, |ack| ack.min(request_deadline));

            if now >= deadline {
                let ignored_cancel = cancel_deadline.is_some_and(|ack| now >= ack);
                self.kill();
                return if ignored_cancel {
                    Err(AiHostError::SidecarExited)
                } else {
                    Err(AiHostError::SidecarTimeout)
                };
            }

            let wait = deadline
                .saturating_duration_since(now)
                .min(CANCEL_POLL_INTERVAL);
            match receiver.recv_timeout(wait) {
                Ok(Ok(message)) => {
                    let cancelled = self.inner.finish_request(&id);
                    return interpret_response(
                        message,
                        op,
                        branch,
                        cancel_sent_at.is_some() || cancelled,
                    );
                }
                Ok(Err(failure)) => {
                    let _ = self.inner.finish_request(&id);
                    return Err(failure.into_error());
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = self.inner.finish_request(&id);
                    return Err(AiHostError::SidecarExited);
                }
            }
        }
    }

    /// Requests cancellation of the request currently in flight, if any.
    ///
    /// Safe to call from another thread (the IPC handler) while `request` is
    /// blocked. The waiting request loop writes the `cancel` line and starts
    /// the cancel-ack deadline only if it still owns the matching id, so a
    /// call that races with completion cannot cancel or poison the next
    /// request.
    pub fn cancel_current(&self) {
        let mut current = lock_unpoisoned(&self.inner.current);
        let Some(id) = current.id.clone() else {
            return;
        };
        current.mark_cancel(&id);
    }

    /// Shuts the sidecar down and waits for the process to exit.
    ///
    /// Waits for `shutdown_ack` up to the shutdown timeout, then kills a
    /// sidecar that does not exit. Calling it again after exit is a no-op.
    pub fn shutdown(&self) -> Result<()> {
        let _request = lock_unpoisoned(&self.inner.request_lock);

        if !self.is_alive() {
            return Ok(());
        }

        let deadline = Instant::now() + self.inner.timeouts.shutdown;
        let (sender, receiver) = mpsc::channel();
        *lock_unpoisoned(&self.inner.shutdown) = Some(sender);

        if let Err(error) = self.inner.write(&SidecarRequest::Shutdown) {
            *lock_unpoisoned(&self.inner.shutdown) = None;
            self.inner.mark_dead();
            self.inner.kill_child();
            return if matches!(error, AiHostError::SidecarExited) {
                Ok(())
            } else {
                Err(error)
            };
        }

        let outcome = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        *lock_unpoisoned(&self.inner.shutdown) = None;

        match outcome {
            Ok(Ok(())) => {
                self.inner.mark_dead();
                let remaining = deadline.saturating_duration_since(Instant::now());
                if !self.inner.wait_for_exit(remaining) {
                    self.inner.kill_child();
                }
                Ok(())
            }
            Ok(Err(RequestFailure::Exited)) => {
                self.inner.mark_dead();
                self.inner.reap_child();
                Ok(())
            }
            Ok(Err(failure)) => {
                self.inner.mark_dead();
                self.inner.kill_child();
                Err(failure.into_error())
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                // No ack: force-stop so shutdown always terminates the process.
                self.inner.mark_dead();
                self.inner.kill_child();
                Ok(())
            }
        }
    }

    /// Force-stops the child and fails any request that is still waiting.
    pub fn kill(&self) {
        self.inner.mark_dead();
        self.inner.fail_all(RequestFailure::Exited);
        lock_unpoisoned(&self.inner.current).clear();
        self.inner.kill_child();
    }

    /// The `hello` received during a successful spawn.
    pub fn hello(&self) -> &Hello {
        &self.inner.hello
    }

    /// Reports whether the child process is still running.
    pub fn is_alive(&self) -> bool {
        if !self.inner.alive.load(Ordering::SeqCst) {
            return false;
        }
        let mut child = lock_unpoisoned(&self.inner.child);
        matches!(child.try_wait(), Ok(None))
    }

    /// Runs `--probe` and checks the advertised variant.
    ///
    /// The probe loads no model, so it gets the shorter 30 second timeout.
    pub fn probe(paths: &SidecarPaths, variant: &str, log_path: &Path) -> Result<Probe> {
        Self::probe_with_expectations(paths, &ProbeExpectations::variant(variant), log_path)
    }

    /// Runs `--probe` and checks the full expected identity.
    ///
    /// The probe loads no model, so it gets the shorter 30 second timeout.
    pub fn probe_with_expectations(
        paths: &SidecarPaths,
        expectations: &ProbeExpectations,
        log_path: &Path,
    ) -> Result<Probe> {
        Self::probe_with_timeout(paths, expectations, log_path, Duration::from_secs(30))
    }

    fn probe_with_timeout(
        paths: &SidecarPaths,
        expectations: &ProbeExpectations,
        log_path: &Path,
        timeout: Duration,
    ) -> Result<Probe> {
        let mut command = probe_command(paths, &expectations.variant);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let protocol_log = ProtocolLog::open(log_path)?;
        let mut child = command.spawn()?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                kill_and_wait(&mut child);
                return Err(AiHostError::SidecarProtocol(
                    "probe stdout was not piped".to_string(),
                ));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                kill_and_wait(&mut child);
                return Err(AiHostError::SidecarProtocol(
                    "probe stderr was not piped".to_string(),
                ));
            }
        };
        let stderr_logger = match spawn_stderr_logger(stderr, log_path) {
            Ok(logger) => logger,
            Err(error) => {
                kill_and_wait(&mut child);
                return Err(error);
            }
        };

        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || reader_loop(stdout, sender, protocol_log));

        let probe = match receiver.recv_timeout(timeout) {
            Ok(ReaderEvent::Message(message)) => match *message {
                SidecarMessage::Probe(probe) => {
                    if probe.protocol_version != PROTOCOL_VERSION {
                        Err(AiHostError::UnsupportedProtocol(probe.protocol_version))
                    } else {
                        validate_probe(expectations, &probe).map(|()| probe)
                    }
                }
                SidecarMessage::Fatal { error } => Err(AiHostError::SidecarFatal(error)),
                other => Err(AiHostError::SidecarProtocol(format!(
                    "expected probe, got {}",
                    message_kind(&other)
                ))),
            },
            Ok(ReaderEvent::Failed(message)) => Err(AiHostError::SidecarProtocol(message)),
            Ok(ReaderEvent::Closed) => Err(AiHostError::SidecarExited),
            Err(RecvTimeoutError::Timeout) => Err(AiHostError::SidecarTimeout),
            Err(RecvTimeoutError::Disconnected) => Err(AiHostError::SidecarExited),
        };

        finish_child(&mut child);
        let _ = reader.join();
        let _ = stderr_logger.join();
        probe
    }
}

impl Drop for SidecarInner {
    fn drop(&mut self) {
        self.mark_dead();
        if let Ok(mut child) = self.child.lock() {
            kill_and_wait(&mut child);
        }
        // Unblock any waiter that outlives the client (defensive: `request`
        // borrows the client, so normally there is none).
        if let Ok(mut pending) = self.pending.lock() {
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(RequestFailure::Exited));
            }
        }
        if let Ok(mut shutdown) = self.shutdown.lock()
            && let Some(waiter) = shutdown.take()
        {
            let _ = waiter.send(Err(RequestFailure::Exited));
        }
        if let Ok(mut reader) = self.reader.lock()
            && let Some(handle) = reader.take()
        {
            let _ = handle.join();
        }
        if let Ok(mut logger) = self.stderr_logger.lock()
            && let Some(handle) = logger.take()
        {
            let _ = handle.join();
        }
    }
}

/// Builds the command that launches the resident sidecar process.
pub fn sidecar_command(config: &SidecarConfig) -> Command {
    let mut command = Command::new(&config.paths.executable);
    command
        .arg("--model")
        .arg(&config.model.model)
        .arg("--mmproj")
        .arg(&config.model.mmproj)
        .arg("--catalog")
        .arg(&config.paths.catalog)
        .arg("--calibration")
        .arg(&config.paths.calibration)
        .arg("--backend")
        .arg(&config.backend)
        .arg("--variant")
        .arg(&config.variant)
        .current_dir(&config.paths.install_dir);
    for (key, value) in &config.env {
        command.env(key, value);
    }
    prepend_library_path(&mut command, &config.paths.install_dir);
    command
}

/// Builds the command that asks the sidecar to advertise its backends.
pub fn probe_command(paths: &SidecarPaths, variant: &str) -> Command {
    let mut command = Command::new(&paths.executable);
    command
        .arg("--probe")
        .arg("--catalog")
        .arg(&paths.catalog)
        .arg("--calibration")
        .arg(&paths.calibration)
        .arg("--variant")
        .arg(variant)
        .current_dir(&paths.install_dir);
    prepend_library_path(&mut command, &paths.install_dir);
    command
}

fn prepend_library_path(command: &mut Command, install_dir: &Path) {
    let mut entries = vec![install_dir.join("lib")];
    if let Some(existing) = std::env::var_os(LIBRARY_PATH_VAR) {
        entries
            .extend(std::env::split_paths(&existing).filter(|path| !path.as_os_str().is_empty()));
    }
    if let Ok(joined) = std::env::join_paths(entries) {
        command.env(LIBRARY_PATH_VAR, joined);
    }
}

fn reader_loop(stdout: ChildStdout, sender: Sender<ReaderEvent>, log: ProtocolLog) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_message(&mut reader) {
            Ok(Some(message)) => {
                if sender
                    .send(ReaderEvent::Message(Box::new(message)))
                    .is_err()
                {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(ReaderEvent::Closed);
                return;
            }
            Err(error) => {
                log.record(&format!("stdout protocol violation: {error}"));
                let _ = sender.send(ReaderEvent::Failed(error.to_string()));
                return;
            }
        }
    }
}

/// Routes reader-thread events to the request or shutdown that is waiting.
///
/// The dispatcher holds only a weak reference so it never keeps the client
/// alive after the last `SidecarClient` is dropped.
fn spawn_dispatcher(inner: Weak<SidecarInner>, receiver: Receiver<ReaderEvent>) {
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let Some(inner) = inner.upgrade() else {
                return;
            };
            match event {
                ReaderEvent::Message(message) => route_message(&inner, *message),
                ReaderEvent::Failed(message) => {
                    let failure = RequestFailure::Protocol(message);
                    inner.fail_all(failure.clone());
                    inner.complete_shutdown(Err(failure));
                    inner.mark_dead();
                    inner.kill_child();
                }
                ReaderEvent::Closed => {
                    inner.fail_all(RequestFailure::Exited);
                    inner.complete_shutdown(Err(RequestFailure::Exited));
                    inner.mark_dead();
                    inner.reap_child();
                }
            }
        }
    });
}

fn route_message(inner: &SidecarInner, message: SidecarMessage) {
    match message {
        SidecarMessage::Response(response) => {
            let id = response.id.clone();
            inner.deliver(&id, SidecarMessage::Response(response));
        }
        SidecarMessage::Cancelled { id } => {
            let message = SidecarMessage::Cancelled { id: id.clone() };
            inner.deliver(&id, message);
        }
        SidecarMessage::Fatal { error } => {
            let failure = RequestFailure::Fatal(error);
            inner.fail_all(failure.clone());
            inner.complete_shutdown(Err(failure));
            inner.mark_dead();
            inner.kill_child();
        }
        SidecarMessage::ShutdownAck => inner.complete_shutdown(Ok(())),
        other => {
            let message = format!("unexpected {} after hello", message_kind(&other));
            let failure = RequestFailure::Protocol(message);
            inner.fail_all(failure.clone());
            inner.complete_shutdown(Err(failure));
            inner.mark_dead();
            inner.kill_child();
        }
    }
}

fn validate_hello(config: &SidecarConfig, hello: &Hello) -> Result<()> {
    if hello.plugin_id != config.plugin_id {
        return Err(AiHostError::SidecarIdentity(format!(
            "plugin_id: expected {}, got {}",
            config.plugin_id, hello.plugin_id
        )));
    }
    if hello.variant != config.variant {
        return Err(AiHostError::SidecarIdentity(format!(
            "variant: expected {}, got {}",
            config.variant, hello.variant
        )));
    }
    if let Some(expected) = &config.catalog_version
        && hello.catalog.version != *expected
    {
        return Err(AiHostError::SidecarIdentity(format!(
            "catalog_version: expected {}, got {}",
            expected, hello.catalog.version
        )));
    }
    if !config.backend.eq_ignore_ascii_case("auto")
        && normalize_backend(&config.backend) != normalize_backend(&hello.backend.kind)
    {
        return Err(AiHostError::SidecarBackendMismatch {
            expected: config.backend.clone(),
            actual: hello.backend.kind.clone(),
        });
    }
    Ok(())
}

fn validate_probe(expectations: &ProbeExpectations, probe: &Probe) -> Result<()> {
    if let Some(expected) = &expectations.plugin_id
        && probe.plugin_id != *expected
    {
        return Err(AiHostError::SidecarIdentity(format!(
            "plugin_id: expected {}, got {}",
            expected, probe.plugin_id
        )));
    }
    if probe.variant != expectations.variant {
        return Err(AiHostError::SidecarIdentity(format!(
            "variant: expected {}, got {}",
            expectations.variant, probe.variant
        )));
    }
    if let Some(expected) = &expectations.catalog_version
        && probe.catalog_version != *expected
    {
        return Err(AiHostError::SidecarIdentity(format!(
            "catalog_version: expected {}, got {}",
            expected, probe.catalog_version
        )));
    }
    Ok(())
}

/// The runtime reports ROCm as either `roc` or `rocm`; treat them as one
/// backend and compare names case-insensitively.
fn normalize_backend(kind: &str) -> String {
    let normalized = kind.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "rocm" => "roc".to_string(),
        _ => normalized,
    }
}

fn interpret_response(
    message: SidecarMessage,
    expected_op: Op,
    expected_branch: Option<TagBranch>,
    cancel_sent: bool,
) -> Result<SidecarResult> {
    match message {
        SidecarMessage::Response(response) => {
            if cancel_sent {
                // The user asked to cancel; never hand a late result to the
                // database as if the inference had completed.
                return Err(AiHostError::Cancelled);
            }
            if response.ok {
                let result = response.result.ok_or_else(|| {
                    AiHostError::SidecarProtocol(
                        "successful response carried no result".to_string(),
                    )
                })?;
                validate_result(&result, expected_op, expected_branch)?;
                Ok(result)
            } else if let Some(error) = response.error {
                if error.code == SidecarErrorCode::Cancelled {
                    Err(AiHostError::Cancelled)
                } else {
                    Err(AiHostError::SidecarRejected(error))
                }
            } else {
                Err(AiHostError::SidecarProtocol(
                    "failed response carried no error".to_string(),
                ))
            }
        }
        SidecarMessage::Cancelled { .. } => Err(AiHostError::Cancelled),
        SidecarMessage::Fatal { error } => Err(AiHostError::SidecarFatal(error)),
        other => Err(AiHostError::SidecarProtocol(format!(
            "unexpected {} during a request",
            message_kind(&other)
        ))),
    }
}

fn validate_result(
    result: &SidecarResult,
    expected_op: Op,
    expected_branch: Option<TagBranch>,
) -> Result<()> {
    let actual_op = match result {
        SidecarResult::Presence { .. } => Op::Presence,
        SidecarResult::PeopleCount { .. } => Op::PeopleCount,
        SidecarResult::PlaceClass { .. } => Op::PlaceClass,
        SidecarResult::PlaceTags { .. } => Op::PlaceTags,
    };
    if actual_op != expected_op {
        return Err(AiHostError::SidecarProtocol(format!(
            "response op {} does not match request {}",
            actual_op.as_str(),
            expected_op.as_str()
        )));
    }

    if let (SidecarResult::PlaceTags { branch, .. }, Some(expected)) = (result, expected_branch)
        && *branch != expected
    {
        return Err(AiHostError::SidecarProtocol(format!(
            "response branch {} does not match request {}",
            branch.as_str(),
            expected.as_str()
        )));
    }
    Ok(())
}

fn spawn_stderr_logger(stderr: ChildStderr, log_path: &Path) -> Result<JoinHandle<()>> {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    Ok(thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let _ = copy(&mut reader, &mut log);
    }))
}

fn message_kind(message: &SidecarMessage) -> &'static str {
    match message {
        SidecarMessage::Hello(_) => "hello",
        SidecarMessage::Probe(_) => "probe",
        SidecarMessage::Response(_) => "response",
        SidecarMessage::Cancelled { .. } => "cancelled",
        SidecarMessage::Fatal { .. } => "fatal",
        SidecarMessage::ShutdownAck => "shutdown_ack",
    }
}

fn kill_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Lets a well-behaved probe exit on its own, then kills a wedged one.
fn finish_child(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => {
                kill_and_wait(child);
                return;
            }
        }
        if Instant::now() >= deadline {
            kill_and_wait(child);
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> SidecarConfig {
        SidecarConfig {
            paths: SidecarPaths {
                install_dir: PathBuf::from("/data/plugins/autotag/linux-vulkan/1.0.0"),
                executable: PathBuf::from("/data/plugins/autotag/linux-vulkan/1.0.0/d1-sidecar"),
                catalog: PathBuf::from("/data/plugins/autotag/linux-vulkan/1.0.0/catalog.json"),
                calibration: PathBuf::from(
                    "/data/plugins/autotag/linux-vulkan/1.0.0/calibration.json",
                ),
            },
            model: ModelPaths {
                model: PathBuf::from("/models/d1-2B-Q8_0.gguf"),
                mmproj: PathBuf::from("/models/mmproj-d1-2B-Q8_0.gguf"),
            },
            variant: "linux-vulkan".to_string(),
            backend: "vulkan".to_string(),
            plugin_id: "vrc-photon-autotag".to_string(),
            catalog_version: Some("2026-11-03.1".to_string()),
            log_path: PathBuf::from("/data/logs/sidecar.log"),
            env: vec![("MOCK_SIDECAR_MODE".to_string(), "normal".to_string())],
        }
    }

    fn arguments(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn default_timeouts_match_protocol_v1() {
        let timeouts = SidecarTimeouts::default();
        assert_eq!(timeouts.hello, Duration::from_secs(60));
        assert_eq!(timeouts.request, Duration::from_secs(120));
        assert_eq!(timeouts.cancel_ack, Duration::from_secs(2));
        assert_eq!(timeouts.shutdown, Duration::from_secs(5));
    }

    #[test]
    fn backend_names_normalize_roc_and_rocm() {
        assert_eq!(normalize_backend("rocm"), "roc");
        assert_eq!(normalize_backend("ROC"), "roc");
        assert_eq!(normalize_backend(" roc "), "roc");
        assert_eq!(normalize_backend("Vulkan"), "vulkan");
    }

    fn sample_probe() -> Probe {
        Probe {
            protocol_version: PROTOCOL_VERSION,
            sidecar_version: "1.0.0".to_string(),
            plugin_id: "vrc-photon-autotag".to_string(),
            variant: "linux-vulkan".to_string(),
            catalog_version: "2026-11-03.1".to_string(),
            backends: vec![vrc_photon_ai_protocol::BackendInfo {
                kind: "vulkan".to_string(),
                device: "Vulkan0".to_string(),
            }],
        }
    }

    #[test]
    fn probe_identity_mismatches_are_rejected() {
        let expectations = ProbeExpectations::new("vrc-photon-autotag", "linux-vulkan")
            .with_catalog_version("2026-11-03.1");
        assert!(validate_probe(&expectations, &sample_probe()).is_ok());

        let mut probe = sample_probe();
        probe.plugin_id = "other-plugin".to_string();
        assert!(matches!(
            validate_probe(&expectations, &probe),
            Err(AiHostError::SidecarIdentity(message)) if message.contains("plugin_id")
        ));

        let mut probe = sample_probe();
        probe.variant = "linux-cpu".to_string();
        assert!(matches!(
            validate_probe(&expectations, &probe),
            Err(AiHostError::SidecarIdentity(message)) if message.contains("variant")
        ));

        let mut probe = sample_probe();
        probe.catalog_version = "2099-01-01.0".to_string();
        assert!(matches!(
            validate_probe(&expectations, &probe),
            Err(AiHostError::SidecarIdentity(message)) if message.contains("catalog_version")
        ));
    }

    #[test]
    fn variant_only_probe_expectations_skip_plugin_and_catalog() {
        let expectations = ProbeExpectations::variant("linux-vulkan");
        let mut probe = sample_probe();
        probe.plugin_id = "any-plugin".to_string();
        probe.catalog_version = "any-catalog".to_string();
        assert!(validate_probe(&expectations, &probe).is_ok());
    }

    #[test]
    fn sidecar_command_passes_the_documented_arguments() {
        let config = sample_config();
        let command = sidecar_command(&config);

        assert_eq!(
            command.get_program().to_string_lossy(),
            "/data/plugins/autotag/linux-vulkan/1.0.0/d1-sidecar"
        );
        assert_eq!(
            arguments(&command),
            vec![
                "--model",
                "/models/d1-2B-Q8_0.gguf",
                "--mmproj",
                "/models/mmproj-d1-2B-Q8_0.gguf",
                "--catalog",
                "/data/plugins/autotag/linux-vulkan/1.0.0/catalog.json",
                "--calibration",
                "/data/plugins/autotag/linux-vulkan/1.0.0/calibration.json",
                "--backend",
                "vulkan",
                "--variant",
                "linux-vulkan",
            ]
        );
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("/data/plugins/autotag/linux-vulkan/1.0.0"))
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == LIBRARY_PATH_VAR
                && value
                    .and_then(|value| std::env::split_paths(value).next())
                    .is_some_and(|first| {
                        first == Path::new("/data/plugins/autotag/linux-vulkan/1.0.0/lib")
                    })
        }));
    }

    #[test]
    fn probe_command_passes_only_the_probe_arguments() {
        let paths = sample_config().paths;
        let command = probe_command(&paths, "linux-vulkan");

        assert_eq!(
            arguments(&command),
            vec![
                "--probe",
                "--catalog",
                "/data/plugins/autotag/linux-vulkan/1.0.0/catalog.json",
                "--calibration",
                "/data/plugins/autotag/linux-vulkan/1.0.0/calibration.json",
                "--variant",
                "linux-vulkan",
            ]
        );
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("/data/plugins/autotag/linux-vulkan/1.0.0"))
        );
    }

    #[test]
    fn pending_cancel_is_scoped_to_the_matching_request() {
        let mut current = CurrentRequest::default();
        current.begin("a");
        current.mark_cancel("a");
        current.mark_cancel("stale");

        // A cancel recorded for "a" must never be consumed by another id.
        assert!(!current.take_cancel("b"));
        assert!(current.take_cancel("a"));
        assert!(!current.take_cancel("a"));
    }

    #[test]
    fn a_new_request_drops_the_previous_pending_cancel() {
        let mut current = CurrentRequest::default();
        current.begin("a");
        current.mark_cancel("a");

        // Even if a stale cancel survives, publishing a new request must not
        // let the next request consume it.
        current.begin("b");
        assert!(!current.take_cancel("a"));
        assert!(!current.take_cancel("b"));
    }

    #[test]
    fn finishing_a_different_request_keeps_the_current_one() {
        let mut current = CurrentRequest::default();
        current.begin("a");
        current.mark_cancel("a");
        current.finish("b");

        assert!(current.take_cancel("a"));
        current.finish("b");
        assert!(!current.take_cancel("b"));
        current.finish("a");
        assert!(!current.take_cancel("a"));
        assert!(current.id.is_none());
    }
}
