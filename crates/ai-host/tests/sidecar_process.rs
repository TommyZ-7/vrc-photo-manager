//! Process-level tests for the AI sidecar client.
//!
//! These tests run the workspace's `mock-sidecar` binary, so they cover the
//! real stdio pipe setup, the handshake, probe mode, and stderr redirection.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use tempfile::TempDir;

use vrc_photon_ai_host::AiHostError;
use vrc_photon_ai_host::sidecar::{ModelPaths, SidecarClient, SidecarConfig, SidecarPaths};

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
  ]
}"#;

const DECOY_CATALOG_JSON: &str = r#"{
  "version": "decoy",
  "outdoor": [
    {
      "key": "env_only",
      "name": "Env only",
      "description": "must never be selected while --catalog is present",
      "default_threshold": 0.5
    }
  ],
  "indoor": []
}"#;

struct MockFixture {
    _tempdir: TempDir,
    config: SidecarConfig,
}

/// Resolves the workspace mock and builds it when necessary.
///
/// Cargo only builds the binaries of the package under test for its
/// integration tests. `cargo test --workspace` therefore does not place the
/// mock in `target/debug` unless it is explicitly built, so do that once here.
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

fn test_config(mock: &Path, mode: &str) -> MockFixture {
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
        log_path: tempdir.path().join("sidecar.log"),
        env: vec![
            ("MOCK_SIDECAR_MODE".to_string(), mode.to_string()),
            ("MOCK_SIDECAR_CATALOG".to_string(), catalog_env),
        ],
    };

    MockFixture {
        _tempdir: tempdir,
        config,
    }
}

#[test]
fn spawn_waits_for_hello_and_reports_the_selected_backend() {
    let mock = mock_sidecar();
    let fixture = test_config(&mock, "normal");

    let client = SidecarClient::spawn(fixture.config).expect("spawn the mock sidecar");

    assert_eq!(client.hello().protocol_version, 1);
    assert_eq!(client.hello().backend.kind, "vulkan");
    assert_eq!(client.hello().variant, "linux-vulkan");
    assert_eq!(client.hello().catalog.outdoor[0].key, "sea");
    assert!(client.is_alive());
}

#[test]
fn spawn_returns_a_typed_error_on_fatal_before_hello() {
    let mock = mock_sidecar();
    let fixture = test_config(&mock, "fatal");

    let error = SidecarClient::spawn(fixture.config).expect_err("fatal must fail the spawn");

    assert!(matches!(error, AiHostError::SidecarFatal(_)), "{error:?}");
}

#[test]
fn probe_reads_backends() {
    let mock = mock_sidecar();
    let fixture = test_config(&mock, "normal");

    let probe = SidecarClient::probe(
        &fixture.config.paths,
        &fixture.config.variant,
        &fixture.config.log_path,
    )
    .expect("probe the mock sidecar");

    assert_eq!(probe.protocol_version, 1);
    assert_eq!(probe.variant, "linux-vulkan");
    assert_eq!(probe.catalog_version, "2026-11-03.1");
    assert!(
        probe
            .backends
            .iter()
            .any(|backend| backend.kind == "vulkan"),
        "{:?}",
        probe.backends
    );
    assert!(
        probe.backends.iter().any(|backend| backend.kind == "cpu"),
        "{:?}",
        probe.backends
    );
}

#[test]
fn spawn_redirects_stderr_to_the_log() {
    let mock = mock_sidecar();
    let fixture = test_config(&mock, "normal");
    let log_path = fixture.config.log_path.clone();
    fs::write(&log_path, "existing log line\n").expect("seed the log file");

    let client = SidecarClient::spawn(fixture.config).expect("spawn the mock sidecar");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut contents = String::new();
    while Instant::now() < deadline {
        contents = fs::read_to_string(&log_path).unwrap_or_default();
        if contents.contains("mock-sidecar") {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert!(
        contents.contains("existing log line"),
        "the log must be appended to: {contents:?}"
    );
    assert!(
        contents.contains("mock-sidecar"),
        "the sidecar stderr must be redirected: {contents:?}"
    );
    assert!(client.is_alive());
}

#[test]
fn catalog_argument_wins_over_the_environment() {
    let mock = mock_sidecar();
    let mut fixture = test_config(&mock, "normal");
    let decoy = fixture._tempdir.path().join("decoy-catalog.json");
    fs::write(&decoy, DECOY_CATALOG_JSON).expect("write the decoy catalog");

    let decoy_catalog: vrc_photon_ai_protocol::Catalog =
        serde_json::from_str(DECOY_CATALOG_JSON).expect("the decoy catalog must parse");
    assert_eq!(decoy_catalog.outdoor[0].key, "env_only");

    let catalog_env = fixture
        .config
        .env
        .iter_mut()
        .find(|(key, _)| key == "MOCK_SIDECAR_CATALOG")
        .expect("the test config sets MOCK_SIDECAR_CATALOG");
    catalog_env.1 = decoy.to_string_lossy().into_owned();

    let client = SidecarClient::spawn(fixture.config).expect("spawn the mock sidecar");

    assert_eq!(client.hello().catalog.version, "2026-11-03.1");
    assert_eq!(client.hello().catalog.outdoor[0].key, "sea");
}

#[test]
fn stdout_protocol_violations_are_written_to_the_log() {
    let mock = mock_sidecar();
    let fixture = test_config(&mock, "bad_stdout");
    let log_path = fixture.config.log_path.clone();

    let error = SidecarClient::spawn(fixture.config).expect_err("non-JSON stdout must fail");

    assert!(
        matches!(error, AiHostError::SidecarProtocol(_)),
        "{error:?}"
    );
    let contents = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        contents.contains("stdout protocol violation"),
        "the protocol violation must reach the log: {contents:?}"
    );
}
