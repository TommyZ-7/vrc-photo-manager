//! Spawning, handshaking with, and probing the AI sidecar process.
//!
//! The sidecar speaks JSON Lines over stdin/stdout. Its stderr is free-form log
//! output and is appended to the host-provided log file. Every read waits on a
//! reader thread with a timeout so a wedged sidecar cannot block the daemon
//! forever.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Write, copy};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use vrc_photon_ai_protocol::{Hello, PROTOCOL_VERSION, Probe, SidecarMessage, read_message};

use crate::error::{AiHostError, Result};

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
    pub log_path: PathBuf,
    /// Test-only environment overrides applied to the child process.
    ///
    /// This is a test seam, not a production configuration surface:
    /// production callers must leave it empty. The integration tests use it
    /// only to set `MOCK_SIDECAR_MODE` and `MOCK_SIDECAR_CATALOG` on the mock
    /// sidecar.
    pub env: Vec<(String, String)>,
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

struct SidecarInner {
    hello: Hello,
    /// The child's stdin. Task 4 sends requests through this handle.
    _stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    /// Messages produced by the reader thread. Task 4 routes them per request.
    events: Mutex<Option<Receiver<ReaderEvent>>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    stderr_logger: Mutex<Option<JoinHandle<()>>>,
}

enum ReaderEvent {
    Message(Box<SidecarMessage>),
    Failed(String),
    Closed,
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
                    if hello.protocol_version == PROTOCOL_VERSION {
                        Ok(hello)
                    } else {
                        Err(AiHostError::UnsupportedProtocol(hello.protocol_version))
                    }
                }
                SidecarMessage::Fatal { error } => Err(AiHostError::SidecarFatal(format!(
                    "{}: {}",
                    error.code.as_str(),
                    error.message
                ))),
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
            Ok(hello) => Ok(Self {
                inner: Arc::new(SidecarInner {
                    hello,
                    _stdin: Mutex::new(stdin),
                    child: Mutex::new(child),
                    events: Mutex::new(Some(receiver)),
                    reader: Mutex::new(Some(reader)),
                    stderr_logger: Mutex::new(Some(stderr_logger)),
                }),
            }),
            Err(error) => {
                kill_and_wait(&mut child);
                let _ = reader.join();
                let _ = stderr_logger.join();
                Err(error)
            }
        }
    }

    /// The `hello` received during a successful spawn.
    pub fn hello(&self) -> &Hello {
        &self.inner.hello
    }

    /// Reports whether the child process is still running.
    pub fn is_alive(&self) -> bool {
        match self.inner.child.lock() {
            Ok(mut child) => matches!(child.try_wait(), Ok(None)),
            Err(_) => false,
        }
    }

    /// Runs `--probe` and returns the advertised backends.
    ///
    /// The probe loads no model, so it gets the shorter 30 second timeout.
    pub fn probe(paths: &SidecarPaths, variant: &str, log_path: &Path) -> Result<Probe> {
        Self::probe_with_timeout(paths, variant, log_path, Duration::from_secs(30))
    }

    fn probe_with_timeout(
        paths: &SidecarPaths,
        variant: &str,
        log_path: &Path,
        timeout: Duration,
    ) -> Result<Probe> {
        let mut command = probe_command(paths, variant);
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
                    if probe.protocol_version == PROTOCOL_VERSION {
                        Ok(probe)
                    } else {
                        Err(AiHostError::UnsupportedProtocol(probe.protocol_version))
                    }
                }
                SidecarMessage::Fatal { error } => Err(AiHostError::SidecarFatal(format!(
                    "{}: {}",
                    error.code.as_str(),
                    error.message
                ))),
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
        if let Ok(mut child) = self.child.lock() {
            kill_and_wait(&mut child);
        }
        if let Ok(mut events) = self.events.lock() {
            let _ = events.take();
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
}
