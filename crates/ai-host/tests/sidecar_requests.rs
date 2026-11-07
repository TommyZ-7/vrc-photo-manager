//! Request, cancel, shutdown, crash, and timeout tests for the sidecar client.
//!
//! These tests run the workspace's `mock-sidecar` binary so the request layer
//! is exercised over the real JSONL pipes, including cooperative cancels and
//! forced stops.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use tempfile::TempDir;

use vrc_photon_ai_host::AiHostError;
use vrc_photon_ai_host::sidecar::{
    CancelToken, ModelPaths, SidecarClient, SidecarConfig, SidecarPaths, SidecarTimeouts,
};
use vrc_photon_ai_protocol::{
    ImageRef, Op, PlaceBranch, SidecarErrorCode, SidecarResult, TagBranch,
};

const CATALOG_JSON: &str = r#"{
  "version": "2026-11-03.1",
  "outdoor": [
    {
      "key": "sea",
      "name": "海",
      "name_en": "Sea",
      "description": "海が見える屋外",
      "default_threshold": 0.5
    }
  ],
  "indoor": [
    {
      "key": "japanese_room",
      "name": "和室",
      "name_en": "Japanese room",
      "description": "畳・障子のある和室",
      "default_threshold": 0.5
    }
  ],
  "atmosphere": [
    {
      "key": "weather_clear",
      "name": "晴れ",
      "name_en": "Clear / sunny",
      "description": "晴れている",
      "default_threshold": 0.5,
      "group": "weather"
    }
  ]
}"#;

struct MockSidecar {
    _tempdir: TempDir,
    client: SidecarClient,
}

/// Resolves the workspace mock and builds it when necessary.
///
/// Cargo only builds the binaries of the package under test for its
/// integration tests, so build the mock package explicitly once.
fn mock_sidecar() -> PathBuf {
    static MOCK: OnceLock<PathBuf> = OnceLock::new();

    MOCK.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "--package", "vrc-photon-ai-mock-sidecar"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("build the mock sidecar");
        assert!(status.success(), "cargo build failed for the mock sidecar");
        cargo_bin("mock-sidecar")
    })
    .clone()
}

fn mock_config(mock: &Path, mode: &str, delay_ms: Option<u64>) -> (TempDir, SidecarConfig) {
    let tempdir = tempfile::tempdir().expect("create a test directory");
    let install_dir = tempdir.path().join("plugin");
    fs::create_dir_all(install_dir.join("lib")).expect("create the plugin directory");

    let catalog = tempdir.path().join("catalog.json");
    fs::write(&catalog, CATALOG_JSON).expect("write the catalog");
    let catalog_env = catalog.to_string_lossy().into_owned();
    let calibration = tempdir.path().join("calibration.json");
    fs::write(&calibration, "{}").expect("write the calibration data");

    let model = tempdir.path().join("d1-2B-Q8_0.gguf");
    fs::write(&model, b"mock model").expect("write the model");
    let mmproj = tempdir.path().join("mmproj-d1-2B-Q8_0.gguf");
    fs::write(&mmproj, b"mock mmproj").expect("write the mmproj");

    let mut env = vec![
        ("MOCK_SIDECAR_MODE".to_string(), mode.to_string()),
        ("MOCK_SIDECAR_CATALOG".to_string(), catalog_env),
    ];
    if let Some(delay_ms) = delay_ms {
        env.push(("MOCK_SIDECAR_DELAY_MS".to_string(), delay_ms.to_string()));
    }

    let config = SidecarConfig {
        paths: SidecarPaths {
            install_dir,
            executable: mock.to_path_buf(),
            catalog,
            calibration,
        },
        model: ModelPaths { model, mmproj },
        variant: "linux-vulkan".to_string(),
        backend: "vulkan".to_string(),
        plugin_id: "vrc-photon-autotag".to_string(),
        catalog_version: Some("2026-11-03.1".to_string()),
        log_path: tempdir.path().join("sidecar.log"),
        env,
    };

    (tempdir, config)
}

fn test_timeouts(request: Duration, cancel_ack: Duration, shutdown: Duration) -> SidecarTimeouts {
    SidecarTimeouts {
        hello: Duration::from_secs(10),
        request,
        cancel_ack,
        shutdown,
    }
}

fn default_test_timeouts() -> SidecarTimeouts {
    test_timeouts(
        Duration::from_secs(5),
        Duration::from_secs(1),
        Duration::from_secs(2),
    )
}

fn spawn_mock_with(mode: &str, delay_ms: Option<u64>, timeouts: SidecarTimeouts) -> MockSidecar {
    let mock = mock_sidecar();
    let (tempdir, config) = mock_config(&mock, mode, delay_ms);
    let client =
        SidecarClient::spawn_with_timeouts(config, timeouts).expect("spawn the mock sidecar");
    MockSidecar {
        _tempdir: tempdir,
        client,
    }
}

fn spawn_mock(mode: &str) -> MockSidecar {
    spawn_mock_with(mode, None, default_test_timeouts())
}

fn test_image() -> ImageRef {
    ImageRef {
        path: "/tmp/vrc-photon-job.jpg".to_string(),
        mtime_ms: 1_760_000_000_000,
        bytes: 123_456,
    }
}

#[test]
fn each_op_maps_to_the_expected_result() {
    let mock = spawn_mock("normal");
    let client = &mock.client;
    let image = test_image();

    assert!(matches!(
        client
            .request(Op::Presence, &image, None, &CancelToken::default())
            .unwrap(),
        SidecarResult::Presence { present: false, .. }
    ));
    assert!(matches!(
        client
            .request(Op::PeopleCount, &image, None, &CancelToken::default())
            .unwrap(),
        SidecarResult::PeopleCount {
            interval: [0, 0],
            ..
        }
    ));
    assert!(matches!(
        client
            .request(Op::PlaceClass, &image, None, &CancelToken::default())
            .unwrap(),
        SidecarResult::PlaceClass {
            branch: PlaceBranch::Other,
            ..
        }
    ));
    assert!(matches!(
        client
            .request(
                Op::PlaceTags,
                &image,
                Some(TagBranch::Outdoor),
                &CancelToken::default()
            )
            .unwrap(),
        SidecarResult::PlaceTags { .. }
    ));
    assert!(matches!(
        client
            .request(Op::AtmosphereTags, &image, None, &CancelToken::default())
            .unwrap(),
        SidecarResult::AtmosphereTags { tags }
            if tags.len() == 1 && tags[0].key == "weather_clear"
    ));
}

#[test]
fn a_response_for_the_wrong_op_is_a_protocol_violation() {
    let mock = spawn_mock("wrong_op");

    let result = mock.client.request(
        Op::PeopleCount,
        &test_image(),
        None,
        &CancelToken::default(),
    );

    assert!(
        matches!(&result, Err(AiHostError::SidecarProtocol(message)) if message.contains("people_count")),
        "{result:?}"
    );
}

#[test]
fn a_response_for_the_wrong_branch_is_a_protocol_violation() {
    let mock = spawn_mock("wrong_branch");

    let result = mock.client.request(
        Op::PlaceTags,
        &test_image(),
        Some(TagBranch::Indoor),
        &CancelToken::default(),
    );

    assert!(
        matches!(&result, Err(AiHostError::SidecarProtocol(message)) if message.contains("branch")),
        "{result:?}"
    );
}

#[test]
fn cancel_during_slow_request_returns_cancelled() {
    let mock = spawn_mock_with(
        "slow",
        Some(2_000),
        test_timeouts(
            Duration::from_secs(5),
            Duration::from_secs(1),
            Duration::from_secs(2),
        ),
    );
    let token = CancelToken::default();
    let worker = token.clone();
    let started = Instant::now();

    let result = thread::scope(|scope| {
        scope.spawn(move || {
            thread::sleep(Duration::from_millis(50));
            worker.cancel();
        });
        mock.client
            .request(Op::Presence, &test_image(), None, &token)
    });

    assert!(matches!(&result, Err(AiHostError::Cancelled)), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "cancel must return before the slow response: {:?}",
        started.elapsed()
    );
    assert!(
        mock.client.is_alive(),
        "a cooperative cancel keeps the sidecar alive"
    );
}

#[test]
fn cancel_current_stops_the_in_flight_request() {
    let mock = spawn_mock_with(
        "slow",
        Some(2_000),
        test_timeouts(
            Duration::from_secs(5),
            Duration::from_secs(1),
            Duration::from_secs(2),
        ),
    );
    let started = Instant::now();

    let result = thread::scope(|scope| {
        let client = &mock.client;
        scope.spawn(move || {
            thread::sleep(Duration::from_millis(50));
            client.cancel_current();
        });
        client.request(Op::Presence, &test_image(), None, &CancelToken::default())
    });

    assert!(matches!(&result, Err(AiHostError::Cancelled)), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "cancel_current must return before the slow response: {:?}",
        started.elapsed()
    );
    assert!(mock.client.is_alive());
}

#[test]
fn cancel_without_ack_kills_the_process() {
    let mock = spawn_mock_with(
        "no_cancel",
        Some(5_000),
        test_timeouts(
            Duration::from_secs(10),
            Duration::from_millis(250),
            Duration::from_secs(2),
        ),
    );
    let token = CancelToken::default();
    let worker = token.clone();
    let started = Instant::now();

    let result = thread::scope(|scope| {
        scope.spawn(move || {
            thread::sleep(Duration::from_millis(50));
            worker.cancel();
        });
        mock.client
            .request(Op::Presence, &test_image(), None, &token)
    });

    assert!(
        matches!(&result, Err(AiHostError::SidecarExited)),
        "a missing cancel ack must force-stop the sidecar: {result:?}"
    );
    assert!(!mock.client.is_alive());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the host must not wait for the sidecar's own delay: {:?}",
        started.elapsed()
    );
}

#[test]
fn cancel_current_without_ack_kills_the_process() {
    let mock = spawn_mock_with(
        "no_cancel",
        Some(5_000),
        test_timeouts(
            Duration::from_secs(10),
            Duration::from_millis(250),
            Duration::from_secs(2),
        ),
    );
    let started = Instant::now();

    let result = thread::scope(|scope| {
        let client = &mock.client;
        scope.spawn(move || {
            thread::sleep(Duration::from_millis(50));
            client.cancel_current();
        });
        client.request(Op::Presence, &test_image(), None, &CancelToken::default())
    });

    assert!(
        matches!(&result, Err(AiHostError::SidecarExited)),
        "cancel_current without an ack must force-stop the sidecar: {result:?}"
    );
    assert!(!mock.client.is_alive());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancel_current must not wait for the request timeout: {:?}",
        started.elapsed()
    );
}

#[test]
fn cancel_current_while_idle_does_not_poison_the_next_request() {
    let mock = spawn_mock("normal");

    mock.client.cancel_current();
    let result = mock
        .client
        .request(Op::Presence, &test_image(), None, &CancelToken::default())
        .expect("a stale cancel_current must not cancel the next request");

    assert!(matches!(
        result,
        SidecarResult::Presence { present: false, .. }
    ));
}

#[test]
fn error_response_preserves_the_structured_code_and_retryable() {
    let mock = spawn_mock("error");

    match mock
        .client
        .request(Op::Presence, &test_image(), None, &CancelToken::default())
    {
        Err(AiHostError::SidecarRejected(error)) => {
            assert_eq!(error.code, SidecarErrorCode::ImageDecodeFailed);
            assert_eq!(error.message, "mock image decode failed");
            assert!(!error.retryable);
        }
        other => panic!("expected SidecarRejected, got {other:?}"),
    }
}

#[test]
fn retryable_error_response_preserves_retryable() {
    let mock = spawn_mock("busy");

    match mock
        .client
        .request(Op::Presence, &test_image(), None, &CancelToken::default())
    {
        Err(AiHostError::SidecarRejected(error)) => {
            assert_eq!(error.code, SidecarErrorCode::Busy);
            assert_eq!(error.message, "mock sidecar is busy");
            assert!(error.retryable);
        }
        other => panic!("expected SidecarRejected, got {other:?}"),
    }
}

#[test]
fn fatal_after_hello_preserves_the_structured_error() {
    let mock = spawn_mock("fatal_after_hello");

    let result = mock
        .client
        .request(Op::Presence, &test_image(), None, &CancelToken::default());

    match result {
        Err(AiHostError::SidecarFatal(error)) => {
            assert_eq!(error.code, SidecarErrorCode::OutOfMemory);
            assert_eq!(error.message, "mock fatal after hello");
            assert!(error.retryable);
        }
        other => panic!("expected SidecarFatal, got {other:?}"),
    }
    assert!(!mock.client.is_alive());
}

#[test]
fn crash_returns_sidecar_exited() {
    let mock = spawn_mock("crash");

    let result = mock
        .client
        .request(Op::Presence, &test_image(), None, &CancelToken::default());

    assert!(
        matches!(&result, Err(AiHostError::SidecarExited)),
        "{result:?}"
    );
    assert!(!mock.client.is_alive());
}

#[test]
fn shutdown_acks_and_exits() {
    let mock = spawn_mock("normal");

    mock.client.shutdown().expect("shutdown the sidecar");

    assert!(!mock.client.is_alive());
    mock.client
        .shutdown()
        .expect("shutdown is idempotent after exit");
}

#[test]
fn shutdown_without_ack_kills_the_process_within_the_timeout() {
    let mock = spawn_mock_with(
        "no_shutdown",
        None,
        test_timeouts(
            Duration::from_secs(5),
            Duration::from_secs(1),
            Duration::from_millis(250),
        ),
    );
    let started = Instant::now();

    mock.client
        .shutdown()
        .expect("a missing shutdown ack must force-stop the sidecar");

    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(200),
        "shutdown must wait for the ack before killing: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown must not hang: {elapsed:?}"
    );
    assert!(!mock.client.is_alive());
}

#[test]
fn request_timeout_is_typed() {
    let mock = spawn_mock_with(
        "slow",
        Some(3_000),
        test_timeouts(
            Duration::from_millis(200),
            Duration::from_secs(1),
            Duration::from_secs(2),
        ),
    );
    let started = Instant::now();

    let result = mock
        .client
        .request(Op::Presence, &test_image(), None, &CancelToken::default());

    assert!(
        matches!(&result, Err(AiHostError::SidecarTimeout)),
        "{result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the request timeout must not wait for the sidecar: {:?}",
        started.elapsed()
    );
    assert!(
        !mock.client.is_alive(),
        "a timed-out sidecar is force-stopped"
    );
}
