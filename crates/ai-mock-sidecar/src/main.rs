//! A test-only mock of the proprietary AI sidecar.
//!
//! The mock speaks protocol v1 over stdin/stdout and is used by the `ai-host`
//! integration tests. It never ships in a release package.
//!
//! Environment variables:
//! - `MOCK_SIDECAR_MODE`: `normal` (default), `fatal`, `crash`, `slow`,
//!   `no_cancel`, `no_shutdown`, `error`, `busy`, or `bad_stdout`.
//! - `MOCK_SIDECAR_DELAY_MS`: overrides the response delay of `slow` and
//!   `no_cancel`, so timeout and cancel tests can run in milliseconds.
//! - `MOCK_SIDECAR_CATALOG`: fallback path to a `Catalog` JSON file when
//!   `--catalog` is absent. A real sidecar always receives `--catalog`, so
//!   the flag takes precedence. When neither is usable, the built-in two-tag
//!   catalog is used.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use vrc_photon_ai_protocol::{
    BackendInfo, Catalog, Hello, ModelInfo, Op, PROTOCOL_VERSION, PlaceBranch, PlaceProbabilities,
    Probe, SidecarError, SidecarErrorCode, SidecarMessage, SidecarRequest, SidecarResponse,
    SidecarResult, TagBranch, TagDefaults, TagDefinition, TagProbability,
};

const PLUGIN_ID: &str = "vrc-photon-autotag";
const SIDECAR_VERSION: &str = "1.0.0-mock";
const DEFAULT_CATALOG_VERSION: &str = "2026-11-03.1";
const SLOW_DELAY: Duration = Duration::from_secs(2);
/// Longer than the default 2 s cancel-ack timeout so the host reaches its kill
/// path before the mock answers.
const NO_CANCEL_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Fatal,
    Crash,
    Slow,
    NoCancel,
    NoShutdown,
    Error,
    Busy,
    BadStdout,
}

impl Mode {
    fn from_env() -> Self {
        match std::env::var("MOCK_SIDECAR_MODE").as_deref() {
            Ok("fatal") => Self::Fatal,
            Ok("crash") => Self::Crash,
            Ok("slow") => Self::Slow,
            Ok("no_cancel") => Self::NoCancel,
            Ok("no_shutdown") => Self::NoShutdown,
            Ok("error") => Self::Error,
            Ok("busy") => Self::Busy,
            Ok("bad_stdout") => Self::BadStdout,
            _ => Self::Normal,
        }
    }

    /// Both delay modes keep the request in flight long enough for the host to
    /// exercise cancel handling.
    fn response_delay(self, override_delay: Option<Duration>) -> Option<Duration> {
        match self {
            Self::Slow => Some(override_delay.unwrap_or(SLOW_DELAY)),
            Self::NoCancel => Some(override_delay.unwrap_or(NO_CANCEL_DELAY)),
            Self::Normal
            | Self::Fatal
            | Self::Crash
            | Self::NoShutdown
            | Self::Error
            | Self::Busy
            | Self::BadStdout => None,
        }
    }

    fn honours_cancel(self) -> bool {
        !matches!(self, Self::NoCancel)
    }

    /// Responses that exercise the host's structured error handling.
    fn response_error(self) -> Option<SidecarError> {
        match self {
            Self::Error => Some(SidecarError {
                code: SidecarErrorCode::ImageDecodeFailed,
                message: "mock image decode failed".to_string(),
                retryable: false,
            }),
            Self::Busy => Some(SidecarError {
                code: SidecarErrorCode::Busy,
                message: "mock sidecar is busy".to_string(),
                retryable: true,
            }),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Args {
    probe: bool,
    backend: String,
    variant: String,
    catalog: Option<PathBuf>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let mut parsed = Self {
            probe: false,
            backend: "vulkan".to_string(),
            variant: "linux-vulkan".to_string(),
            catalog: None,
        };

        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--probe" => parsed.probe = true,
                "--backend" => parsed.backend = take_value(&mut args, "--backend")?,
                "--variant" => parsed.variant = take_value(&mut args, "--variant")?,
                "--catalog" => {
                    parsed.catalog = Some(PathBuf::from(take_value(&mut args, "--catalog")?));
                }
                // The mock does not load a model. Accept the documented flags
                // so the host's command line is exercised unchanged.
                "--model" | "--mmproj" | "--calibration" => {
                    let _ = take_value(&mut args, &argument)?;
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }

        Ok(parsed)
    }
}

fn take_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}"))
}

fn main() -> ExitCode {
    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("mock-sidecar: {message}");
            return ExitCode::from(2);
        }
    };
    let mode = Mode::from_env();
    let delay = mode.response_delay(configured_delay());
    let catalog = load_catalog(args.catalog.as_deref());

    eprintln!(
        "mock-sidecar: starting mode={mode:?} variant={} backend={}",
        args.variant, args.backend
    );

    if args.probe {
        emit(&SidecarMessage::Probe(probe(&args, &catalog)));
        return ExitCode::SUCCESS;
    }

    if mode == Mode::BadStdout {
        write_raw_stdout_line("this is not a protocol message");
        return ExitCode::from(1);
    }

    if mode == Mode::Fatal {
        emit(&SidecarMessage::Fatal {
            error: SidecarError {
                code: SidecarErrorCode::ModelLoadFailed,
                message: "mock model failed to load".to_string(),
                retryable: false,
            },
        });
        return ExitCode::from(1);
    }

    emit(&SidecarMessage::Hello(hello(&args, &catalog)));
    serve(mode, delay, &catalog);
    ExitCode::SUCCESS
}

fn configured_delay() -> Option<Duration> {
    std::env::var("MOCK_SIDECAR_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
}

fn load_catalog(explicit: Option<&Path>) -> Catalog {
    // The real sidecar always receives `--catalog`; the environment fallback
    // exists only so tests can exercise the built-in catalog. The flag wins so
    // a developer's environment cannot override the sidecar's package data.
    let configured = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("MOCK_SIDECAR_CATALOG").map(PathBuf::from));

    if let Some(path) = configured {
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(catalog) => return catalog,
                Err(error) => {
                    eprintln!("mock-sidecar: ignoring catalog {}: {error}", path.display());
                }
            },
            Err(error) => {
                eprintln!(
                    "mock-sidecar: cannot read catalog {}: {error}",
                    path.display()
                );
            }
        }
    }

    builtin_catalog()
}

fn builtin_catalog() -> Catalog {
    Catalog {
        version: DEFAULT_CATALOG_VERSION.to_string(),
        outdoor: vec![TagDefinition {
            key: "sea".to_string(),
            name: "海".to_string(),
            name_en: Some("Sea".to_string()),
            description: "海が見える屋外".to_string(),
            default_threshold: 0.5,
        }],
        indoor: vec![TagDefinition {
            key: "japanese_room".to_string(),
            name: "和室".to_string(),
            name_en: Some("Japanese room".to_string()),
            description: "畳・障子のある和室".to_string(),
            default_threshold: 0.5,
        }],
    }
}

fn hello(args: &Args, catalog: &Catalog) -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        sidecar_version: SIDECAR_VERSION.to_string(),
        plugin_id: PLUGIN_ID.to_string(),
        variant: args.variant.clone(),
        model: ModelInfo {
            id: "d1-2B".to_string(),
            quantization: "Q8_0".to_string(),
            revision: "mock".to_string(),
            catalog_version: catalog.version.clone(),
        },
        backend: BackendInfo {
            kind: args.backend.clone(),
            device: device_name(&args.backend),
        },
        capabilities: vec![
            "presence".to_string(),
            "people_count".to_string(),
            "place_class".to_string(),
            "place_tags".to_string(),
        ],
        defaults: TagDefaults {
            presence_threshold: 0.9,
            multiple_threshold: 0.5,
        },
        catalog: catalog.clone(),
    }
}

fn probe(args: &Args, catalog: &Catalog) -> Probe {
    let mut backends = vec![BackendInfo {
        kind: args.backend.clone(),
        device: device_name(&args.backend),
    }];
    if args.backend != "cpu" {
        backends.push(BackendInfo {
            kind: "cpu".to_string(),
            device: "CPU".to_string(),
        });
    }

    Probe {
        protocol_version: PROTOCOL_VERSION,
        sidecar_version: SIDECAR_VERSION.to_string(),
        plugin_id: PLUGIN_ID.to_string(),
        variant: args.variant.clone(),
        catalog_version: catalog.version.clone(),
        backends,
    }
}

fn device_name(kind: &str) -> String {
    match kind {
        "cpu" => "CPU".to_string(),
        "vulkan" => "Mock Vulkan device".to_string(),
        "cuda" => "Mock CUDA device".to_string(),
        "rocm" | "roc" => "Mock ROCm device".to_string(),
        other => format!("Mock {other} device"),
    }
}

fn emit(message: &SidecarMessage) {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if let Err(error) = vrc_photon_ai_protocol::write_message(&mut stdout, message) {
        eprintln!("mock-sidecar: failed to write a message: {error}");
        std::process::exit(1);
    }
}

fn write_raw_stdout_line(line: &str) {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

fn serve(mode: Mode, delay: Option<Duration>, catalog: &Catalog) {
    let (sender, receiver) = mpsc::channel();
    spawn_stdin_reader(sender);

    let mut queued = VecDeque::new();
    loop {
        let request = match queued.pop_front() {
            Some(request) => request,
            None => match receiver.recv() {
                Ok(request) => request,
                Err(_) => return,
            },
        };

        match request {
            SidecarRequest::Shutdown => {
                if mode == Mode::NoShutdown {
                    // Stay alive so the host has to exercise its kill path.
                    continue;
                }
                emit(&SidecarMessage::ShutdownAck);
                return;
            }
            SidecarRequest::Cancel { .. } => {
                // A cancel that arrives while idle has nothing to cancel.
            }
            SidecarRequest::Request { id, op, branch, .. } => {
                if mode == Mode::Crash {
                    std::process::exit(1);
                }

                if let Some(error) = mode.response_error() {
                    emit(&SidecarMessage::Response(SidecarResponse {
                        id,
                        ok: false,
                        result: None,
                        error: Some(error),
                    }));
                    continue;
                }

                if let Some(delay) = delay
                    && wait_for_cancel(&receiver, &mut queued, &id, delay, mode)
                {
                    emit(&SidecarMessage::Cancelled { id });
                    continue;
                }

                emit(&SidecarMessage::Response(SidecarResponse {
                    id,
                    ok: true,
                    result: Some(mock_result(op, branch, catalog)),
                    error: None,
                }));
            }
        }
    }
}

fn spawn_stdin_reader(sender: Sender<SidecarRequest>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<SidecarRequest>(trimmed) {
                        Ok(request) => {
                            if sender.send(request).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            eprintln!("mock-sidecar: invalid request line: {error}");
                        }
                    }
                }
                Err(error) => {
                    eprintln!("mock-sidecar: failed to read stdin: {error}");
                    return;
                }
            }
        }
    });
}

fn wait_for_cancel(
    receiver: &Receiver<SidecarRequest>,
    queued: &mut VecDeque<SidecarRequest>,
    id: &str,
    delay: Duration,
    mode: Mode,
) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        match receiver.recv_timeout(remaining) {
            Ok(SidecarRequest::Cancel { id: cancelled_id })
                if cancelled_id == id && mode.honours_cancel() =>
            {
                return true;
            }
            Ok(request) => queued.push_back(request),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn mock_result(op: Op, branch: Option<TagBranch>, catalog: &Catalog) -> SidecarResult {
    match op {
        Op::Presence => SidecarResult::Presence {
            present: false,
            p_yes: 0.1,
        },
        Op::PeopleCount => SidecarResult::PeopleCount {
            count: 0,
            interval: [0, 0],
        },
        Op::PlaceClass => SidecarResult::PlaceClass {
            branch: PlaceBranch::Other,
            probabilities: PlaceProbabilities {
                indoor: 0.3,
                outdoor: 0.3,
                other: 0.4,
            },
        },
        Op::PlaceTags => {
            let branch = branch.unwrap_or(TagBranch::Outdoor);
            let definitions = match branch {
                TagBranch::Indoor => &catalog.indoor,
                TagBranch::Outdoor => &catalog.outdoor,
            };
            let tags = definitions
                .iter()
                .enumerate()
                .map(|(index, definition)| TagProbability {
                    key: definition.key.clone(),
                    p_yes: (0.9 - index as f32 * 0.1).max(0.0),
                })
                .collect();
            SidecarResult::PlaceTags { branch, tags }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_has_the_documented_minimal_tags() {
        let catalog = builtin_catalog();
        assert_eq!(catalog.version, DEFAULT_CATALOG_VERSION);
        assert_eq!(catalog.outdoor[0].key, "sea");
        assert_eq!(catalog.indoor[0].key, "japanese_room");
    }

    #[test]
    fn place_tags_return_every_tag_in_the_requested_branch() {
        let catalog = builtin_catalog();
        let SidecarResult::PlaceTags { branch, tags } =
            mock_result(Op::PlaceTags, Some(TagBranch::Indoor), &catalog)
        else {
            panic!("expected place_tags");
        };

        assert_eq!(branch, TagBranch::Indoor);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].key, "japanese_room");
        assert!(tags[0].p_yes > 0.0);
    }
}
