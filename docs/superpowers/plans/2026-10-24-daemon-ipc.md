# UI⇔デーモンIPC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a local-only, versioned IPC channel so UI commands wake the daemon immediately and scan/compression/error events reach the UI without waiting for the three-second poll, while SQLite remains the durable source of truth.

**Architecture:** A new `crates/ipc` crate owns the protocol and `interprocess` local-socket transport. The daemon runs a listener thread and publishes events through an `EventBus`; the UI runs a reconnecting subscription thread and sends infrequent one-shot requests. Durable commands stay in `daemon_commands`, compression jobs stay in `compression_jobs`, and the heartbeat remains the no-IPC fallback.

**Tech Stack:** Rust stable (edition 2024), `interprocess` 2.4 (Unix domain socket / Windows named pipe), serde/serde_json newline framing, rusqlite, gpui-kit 0.8, `async-channel` for the UI pump.

**Spec:** `docs/superpowers/specs/2026-10-24-daemon-ipc-design.md`

## Global Constraints

- `PROTOCOL_VERSION = 1`; every connection starts with `hello` and closes on a mismatch.
- IPC is local-only: Unix domain socket or Windows named pipe. No TCP, no remote access.
- SQLite remains the source of truth for commands, jobs, errors, and heartbeat. `wake_commands` is wake-only and never inserts a command.
- Keep `app_settings.daemon_heartbeat`; `daemon_active = heartbeat_fresh || ipc_connected`.
- Unix socket directory mode `0700`, socket mode `0600`. Never unlink a live endpoint; probe before replacing a stale one.
- No IPC call may block the GPUI thread. One-shot request timeout is 2 seconds.
- Scan progress events are throttled to at most one every 200 ms plus phase changes.
- Keep the existing three-second `refresh_live` fallback and the daemon's behavior with no UI.
- Do not modify `opencode.json` or unrelated user changes.
- Commit prefixes: `feat:`, `fix:`, `docs:`, `ci:`, `chore:`.

## Review Focus

- **Daemon absent or socket path missing:** the UI must stay open, show "デーモン停止" from the heartbeat, keep retired commands in SQLite, and retry the subscription quietly.
- **Long full scan:** the daemon's main SQLite connection is held for the scan; IPC status/wake requests must still answer because handlers never touch SQLite.
- **Live vs stale socket:** a second process must never unlink a live daemon's socket; a refused connection means stale and may be replaced.
- **Protocol version mismatch:** the UI must show a non-fatal update message and continue with the heartbeat/SQLite path.
- **Command exactly once:** a rescan queued in SQLite plus an IPC wake must not create a second command or start two scans.

---

### Task 1: IPC crate protocol and framing

**Files:**
- Create: `crates/ipc/Cargo.toml`
- Create: `crates/ipc/src/lib.rs`
- Create: `crates/ipc/src/protocol.rs`
- Create: `crates/ipc/src/framing.rs`
- Modify: `Cargo.toml` (workspace members and shared dependencies)
- Test: inline `#[cfg(test)]` modules in the new files

**Interfaces:**
- Consumes: nothing.
- Produces: `vrc_photo_ipc::{PROTOCOL_VERSION, APP_VERSION}`, `protocol::{Request, Response, Event, StatusSnapshot, ScanPhase, JobStatus, Operation, ClientInfo, ErrorCode, RequestEnvelope, ServerMessage}`, `framing::{read_message, write_message}`.

- [ ] **Step 1: Write the failing protocol and framing tests**

Add tests asserting exact wire names and round-trips:

```rust
#[test]
fn request_envelope_round_trips_as_tagged_json() {
    let value = RequestEnvelope {
        id: 7,
        request: Request::SetPaused { paused: true },
    };
    let json = serde_json::to_string(&value).unwrap();
    assert!(json.contains("\"set_paused\""));
    assert_eq!(serde_json::from_str::<RequestEnvelope>(&json).unwrap(), value);
}

#[test]
fn server_event_round_trips() {
    let value = ServerMessage::Event {
        event: Event::ScanProgress {
            phase: ScanPhase::Indexing,
            processed: 50,
            total: Some(100),
            indexed: 49,
            root: 1,
            roots: 2,
        },
    };
    let json = serde_json::to_string(&value).unwrap();
    assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), value);
}

#[test]
fn framing_reads_one_newline_delimited_message() {
    let input = b"{\"id\":1,\"request\":{\"type\":\"status\"}}\n{\"id\":2,\"request\":{\"type\":\"shutdown\"}}\n";
    let mut reader = std::io::BufReader::new(&input[..]);
    let first: RequestEnvelope = read_message(&mut reader).unwrap().unwrap();
    let second: RequestEnvelope = read_message(&mut reader).unwrap().unwrap();
    assert_eq!(first.id, 1);
    assert_eq!(second.id, 2);
    assert!(read_message::<_, RequestEnvelope>(&mut reader).unwrap().is_none());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ipc`
Expected: FAIL because the crate and types do not exist.

- [ ] **Step 3: Create the crate and implement the types**

Add `crates/ipc` to the workspace. `protocol.rs` defines the types from the spec, with `#[serde(tag = "type", rename_all = "snake_case")]` on request/response/event enums and `#[serde(rename_all = "snake_case")]` on `ScanPhase`, `JobStatus`, `Operation`, and `ErrorCode`. `framing.rs` reads/writes one JSON object plus `\n`, rejects lines over 1 MiB, and maps serde errors to `io::ErrorKind::InvalidData`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-ipc`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ipc
git commit -m "feat: add IPC protocol and framing"
```

---

### Task 2: IPC endpoint, listener, request client, and subscription

**Files:**
- Create: `crates/ipc/src/endpoint.rs`
- Create: `crates/ipc/src/server.rs`
- Create: `crates/ipc/src/client.rs`
- Modify: `crates/ipc/src/lib.rs`
- Test: inline tests in the new modules

**Interfaces:**
- Consumes: Task 1 protocol/framing.
- Produces: `Endpoint` (`for_data_dir`, `for_tests`, `display`), `server::Listener` (`bind`, `set_nonblocking_accept`, `accept`), `client::request`, `client::Subscription::open`, `Subscription::next_event`, `IpcError`.

- [ ] **Step 1: Write the failing transport tests**

Cover request/response, version mismatch, subscription, and stale/live endpoint behavior:

```rust
#[test]
fn request_round_trips_over_a_local_socket() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::for_tests(dir.path());
    let listener = server::Listener::bind(endpoint.clone()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        let hello: RequestEnvelope = framing::read_message(&mut std::io::BufReader::new(&mut stream)).unwrap().unwrap();
        assert!(matches!(hello.request, Request::Hello { .. }));
        framing::write_message(&mut stream, &ServerMessage::Response { id: hello.id, response: Response::Hello { protocol_version: PROTOCOL_VERSION, daemon_version: APP_VERSION.into(), pid: 1, status: StatusSnapshot::default() } }).unwrap();
        let request: RequestEnvelope = framing::read_message(&mut std::io::BufReader::new(&mut stream)).unwrap().unwrap();
        assert!(matches!(request.request, Request::WakeCommands));
        framing::write_message(&mut stream, &ServerMessage::Response { id: request.id, response: Response::Ok }).unwrap();
    });
    let response = client::request(&endpoint, Request::WakeCommands, std::time::Duration::from_secs(1)).unwrap();
    assert!(matches!(response, Response::Ok));
    server.join().unwrap();
}

#[test]
fn protocol_mismatch_is_reported() {
    // Fake server replies Hello with protocol_version = PROTOCOL_VERSION + 1.
    // Assert client::request returns IpcError::ProtocolMismatch { .. }.
}

#[test]
fn subscription_receives_events() {
    // Fake server: hello, subscribe -> Subscribed, then one ScanStarted event.
    // Assert Subscription::open returns the status and next_event returns the event.
}

#[test]
fn stale_socket_is_replaced_but_live_socket_is_not() {
    // Bind, drop the listener (stale file), bind again -> Ok.
    // Bind, then bind the same endpoint again -> io::ErrorKind::AddrInUse.
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ipc transport`
Expected: FAIL because the transport types do not exist.

- [ ] **Step 3: Implement the endpoint and transport**

- `Endpoint::for_data_dir`: Unix uses `$XDG_RUNTIME_DIR/vrc-photo-manager/daemon.sock` or `<data_dir>/run/daemon.sock`, falls back to `/tmp/vrc-photo-manager-<uid>/daemon.sock` when the path exceeds 100 bytes; Windows uses a namespaced pipe name containing the user name.
- `Endpoint::for_tests`: Unix uses `<dir>/daemon.sock`; Windows uses a unique namespaced name built from the process id plus an atomic counter.
- `Listener::bind`: probe with `client::connect` for 200 ms. If the probe succeeds, return `AddrInUse` without unlinking. If it fails with `ConnectionRefused`/`NotFound`, remove the Unix socket file and bind. Create the parent directory `0700` and set the socket `0600`.
- `client::connect`: `ConnectOptions::new().name(...).wait_mode(ConnectWaitMode::Timeout(timeout)).connect_sync()`.
- `client::request`: connect, set receive/send timeouts, send `Hello` (id 1), validate `Response::Hello`, send the request (id 2), return the matching response. Map `Response::Error` and version mismatch to `IpcError`.
- `Subscription::open`: hello, validate, set receive timeout to `None`, send `Subscribe`, read `Subscribed`, return the status.
- `Subscription::next_event`: read `ServerMessage`; return `Event`, reject an unexpected `Response`.
- Add `tempfile` as a dev-dependency for the transport tests.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-ipc`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ipc Cargo.lock
git commit -m "feat: add local socket IPC transport"
```

---

### Task 3: Scanner progress observer

**Files:**
- Modify: `crates/core/src/scanner.rs`
- Test: `crates/core/src/scanner.rs` tests

**Interfaces:**
- Consumes: existing scanner walk/index behavior.
- Produces: `scanner::{ScanPhase, ScanProgress, scan_and_index_observed}`; `scan_and_index` remains unchanged for callers.

- [ ] **Step 1: Write the failing observer test**

```rust
#[test]
fn observed_scan_reports_walking_and_indexing_progress() {
    let dir = tempdir().unwrap();
    for name in ["VRChat_2026-10-21_21-14-03.221_4x3.png", "VRChat_2026-10-21_21-14-04.221_4x3.png"] {
        write_png(&dir.path().join(name));
    }
    let db = Db::open_in_memory().unwrap();
    let root_id = db.add_root(&dir.path().to_string_lossy(), "reference").unwrap();
    let mut events = Vec::new();
    let report = scan_and_index_observed(&db, root_id, dir.path(), |p| events.push(p)).unwrap();
    assert_eq!(report.indexed, 2);
    assert!(events.iter().any(|p| p.phase == ScanPhase::Walking && p.total.is_none()));
    let last = events.last().unwrap();
    assert_eq!(last.phase, ScanPhase::Indexing);
    assert_eq!(last.total, Some(2));
    assert_eq!(last.processed, 2);
    assert_eq!(last.indexed, 2);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-core observed_scan_reports_walking_and_indexing_progress`
Expected: FAIL because `scan_and_index_observed` does not exist.

- [ ] **Step 3: Implement the observed scanner**

Add `walk_with(root, dir, photos, failures, &mut observe)`, call it from `scan_root_report`, and make `scan_and_index_observed` emit `Walking` after each candidate photo and `Indexing` after each upsert/rebind/skip. `scan_and_index` delegates with `|_| {}`. Keep sorting and report semantics identical.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-core scanner`
Expected: PASS, including the existing scanner tests.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/scanner.rs
git commit -m "feat: report scan progress from the scanner"
```

---

### Task 4: Daemon wake signals and status fields

**Files:**
- Create: `crates/daemon/src/signals.rs`
- Modify: `crates/daemon/src/lib.rs`
- Modify: `crates/daemon/Cargo.toml`
- Test: inline tests in `signals.rs`

**Interfaces:**
- Consumes: `vrc-photo-ipc` for `ScanPhase`.
- Produces: `signals::{WakeSignal, JobSignal}`; `DaemonStatus` gains `scanning`, `scan_phase`, `scan_processed`, `scan_total`, `scan_indexed`, `scan_root`, `scan_roots`.

- [ ] **Step 1: Write the failing signal tests**

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-daemon signals`
Expected: FAIL because the module does not exist.

- [ ] **Step 3: Implement the signals and status fields**

`WakeSignal` is `Mutex<bool>` + `Condvar`; `wait` returns immediately when pending, otherwise waits up to the timeout and clears the flag. `JobSignal` is a generation counter + `Condvar`; each worker keeps its own `seen` generation. Add the status fields with defaults and import `vrc_photo_ipc::ScanPhase` in `DaemonStatus`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-daemon signals`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/daemon/Cargo.toml crates/daemon/src/lib.rs crates/daemon/src/signals.rs Cargo.lock
git commit -m "feat: add daemon wake signals and scan status"
```

---

### Task 5: Daemon IPC server

**Files:**
- Create: `crates/daemon/src/ipc_server.rs`
- Modify: `crates/daemon/src/lib.rs` (`mod ipc_server;`)
- Test: inline tests in `ipc_server.rs`

**Interfaces:**
- Consumes: Task 1/2 IPC types and Task 4 signals/status.
- Produces: `EventBus` (`publish`, `subscribe`), `IpcControl { running, status, wake, jobs, events }`, `IpcServer::start(Endpoint, IpcControl)`, `status_snapshot(&DaemonStatus) -> StatusSnapshot`.

- [ ] **Step 1: Write the failing server tests**

Use `Endpoint::for_tests(tempdir)` and `client::request`/`Subscription`:

```rust
#[test]
fn wake_commands_wakes_without_touching_sqlite() {
    let (endpoint, control) = test_control();
    let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
    let response = client::request(&endpoint, Request::WakeCommands, Duration::from_secs(1)).unwrap();
    assert!(matches!(response, Response::Ok));
    assert!(control.wake.is_pending());
}

#[test]
fn set_paused_updates_status_and_publishes_an_event() {
    let (endpoint, control) = test_control();
    let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
    let mut subscription = Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();
    let response = client::request(&endpoint, Request::SetPaused { paused: true }, Duration::from_secs(1)).unwrap();
    assert!(matches!(response, Response::Ok));
    assert!(control.status.lock().unwrap().paused);
    assert!(matches!(subscription.next_event().unwrap(), Some(Event::StatusChanged { .. })));
}

#[test]
fn shutdown_clears_running_and_wakes() {
    let (endpoint, control) = test_control();
    let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
    let response = client::request(&endpoint, Request::Shutdown, Duration::from_secs(1)).unwrap();
    assert!(matches!(response, Response::Ok));
    assert!(!control.running.load(Ordering::Relaxed));
    assert!(control.wake.is_pending());
}
```

Add a test that `hello`/`status` returns the cached snapshot and counts from the control alone; `IpcControl` intentionally has no SQLite handle, so a long-held scan lock cannot block it.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-daemon ipc_server`
Expected: FAIL because the server does not exist.

- [ ] **Step 3: Implement the server**

- `EventBus`: `Arc<Mutex<Vec<Sender<Event>>>>`; `publish` clones the event and retains only successful sends; `subscribe` pushes a channel receiver.
- `IpcControl`: `running`, `status`, `wake`, `jobs`, `events`; `snapshot()` converts `DaemonStatus`; `set_paused` updates status, notifies `wake`, and publishes `StatusChanged`; `shutdown` clears running and notifies `wake`.
- `IpcServer::start`: bind the endpoint, set `ListenerNonblockingMode::Accept`, spawn the accept thread with a stop flag, and return a handle whose `Drop` stops and joins the accept thread.
- Per connection: require `Hello` first, reject mismatches with `protocol_mismatch`, respond with `hello` + snapshot; handle `Status`, `WakeCommands`, `WakeJobs`, `SetPaused`, `Shutdown`; `Subscribe` replies `subscribed` and streams events with a 100 ms receive timeout, exiting when `running` is false or the socket write fails. Use `BufReader<Stream>` and write through `get_mut()`.
- Log accept/connection errors with `tracing`; never panic on a malformed client.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-daemon ipc_server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/daemon/src/ipc_server.rs crates/daemon/src/lib.rs
git commit -m "feat: add daemon IPC server"
```

---

### Task 6: Daemon integration (events, wake loop, workers)

**Files:**
- Modify: `crates/daemon/src/lib.rs`
- Test: inline tests in `lib.rs`

**Interfaces:**
- Consumes: Task 3 observed scanner, Task 4 signals/status, Task 5 server/EventBus.
- Produces: `Daemon::run` starts the IPC server; `DaemonStatus` is live during scans; `EventBus` receives scan/compression/error/status events; main loop and workers wake immediately.

- [ ] **Step 1: Write the failing integration tests**

```rust
#[test]
fn scan_photos_publishes_started_progress_and_finished() {
    let dir = tempdir().unwrap();
    write_png(&dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
    let mut daemon = test_daemon(&dir);
    let mut events = daemon.events.subscribe();
    daemon.run_once().unwrap();
    assert!(matches!(events.try_recv().unwrap(), Event::ScanStarted { .. }));
    let mut finished = None;
    while let Ok(event) = events.try_recv() {
        if let Event::ScanFinished { .. } = event { finished = Some(event); }
    }
    assert!(finished.is_some());
}

#[test]
fn set_paused_publishes_status_changed_and_wakes() {
    let dir = tempdir().unwrap();
    let daemon = test_daemon(&dir);
    let mut events = daemon.events.subscribe();
    daemon.set_paused(true);
    assert!(matches!(events.try_recv().unwrap(), Event::StatusChanged { .. }));
    assert!(daemon.wake.is_pending());
}

#[test]
fn report_daemon_error_publishes_changed_once() {
    // First report publishes DaemonErrorChanged; an identical report does not.
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-daemon`
Expected: FAIL because the events/wake wiring does not exist (the new tests fail; existing tests pass).

- [ ] **Step 3: Integrate the daemon**

- Add `db_path`, `ipc_endpoint`, `wake`, `jobs`, `events`, and `ipc` to `Daemon`. Initialize them in `open`; resolve the endpoint from `vrc_photo_core::data_dir()` and allow `set_ipc_endpoint` for tests.
- `run`: start `IpcServer` before the initial scan; if binding fails, log and continue. Replace both `thread::sleep(SCAN_INTERVAL)` calls with `self.wake.wait(SCAN_INTERVAL)`.
- Refactor `scan_photos` into `begin_scan` + `scan_photos_inner` + `finish_scan` so every error path clears `scanning`. Use `scan_and_index_observed` per root; update status and publish throttled `ScanProgress`. Publish one `ScanStarted`/`ScanFinished` pair per full scan. `ScanFinished` carries `failed: bool` and is emitted after the post-scan work.
- `set_paused`: update status, notify `wake`, publish `StatusChanged`.
- `poll_tray` shutdown: set `running=false` and notify `wake`.
- Compression worker: take `Arc<JobSignal>` and `EventBus`; wait on the signal with a 400 ms timeout; publish `Processing { progress: 0.05 }` before `process_job` and `Completed`/`Failed` after. Add `JobStatus` mapping.
- `start_workers` passes the signal/events; `stop_workers` notifies the signal before joining.
- `report_daemon_error`/`clear_daemon_error`: publish `DaemonErrorChanged` only on a real change.
- After `enqueue_auto_jobs` in the scan and incremental paths, notify `jobs`.
- `shutdown`: drop the IPC server before stopping workers.
- Add `status_snapshot` conversion and keep `DaemonStatus` counts updated.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-daemon`
Expected: PASS, including existing daemon tests.

- [ ] **Step 5: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat: publish daemon events and wake on IPC"
```

---

### Task 7: UI IPC bridge and liveness

**Files:**
- Create: `crates/ui/src/ipc_bridge.rs`
- Modify: `crates/ui/Cargo.toml`
- Modify: `crates/ui/src/main.rs` (module, `App` fields, `start_ipc`, `handle_ipc_incoming`, `handle_ipc_event`, `refresh_monitor`, wake helpers)
- Test: inline tests in `main.rs` and `ipc_bridge.rs`

**Interfaces:**
- Consumes: Task 1/2 IPC client/protocol.
- Produces: `ipc_bridge::{Incoming, start, request}`, `App::{ipc_connected, ipc_error, daemon_paused, scan_progress}`, `daemon_liveness(Option<i64>, bool) -> bool`.

- [ ] **Step 1: Write the failing UI helper/bridge tests**

```rust
#[test]
fn daemon_liveness_uses_heartbeat_or_ipc() {
    assert!(daemon_liveness(Some(3), false));
    assert!(daemon_liveness(None, true));
    assert!(!daemon_liveness(Some(30), false));
    assert!(!daemon_liveness(None, false));
}

#[test]
fn bridge_delivers_a_daemon_event() {
    // Start a listener with a fake hello/subscribe, then ipc_bridge::start with
    // an async_channel receiver; assert Connected then Event.
}

#[test]
fn bridge_reports_disconnected_when_no_daemon_is_listening() {
    // Point at a temp endpoint with no listener; assert the first incoming
    // message is Disconnected and the app would stay usable.
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui daemon_liveness_uses_heartbeat_or_ipc`
Expected: FAIL because `daemon_liveness` does not exist.

- [ ] **Step 3: Implement the bridge and app wiring**

- `ipc_bridge::Incoming`: `Connected(StatusSnapshot)`, `Disconnected(String)`, `Event(Event)`.
- `start`: spawn a thread that opens a `Subscription`, sends `Connected`, forwards events, sends `Disconnected` on error, sleeps 2 s, retries until the channel closes.
- `request`: `vrc_photo_ipc::client::request(&endpoint, request, Duration::from_secs(2))`.
- In `App::new`, resolve `Endpoint::for_data_dir` from `vrc_photo_core::data_dir()`; if present, start the pump. Store the endpoint, connected/error/paused/scan fields.
- `handle_ipc_incoming`: update `ipc_connected`, `ipc_error`, `daemon_paused`, `scan_progress`; apply `StatusSnapshot`; call `refresh_monitor`/`cx.notify`.
- `handle_ipc_event`: scan events update/clear progress; `ScanFinished` calls `refresh_live`; compression events call `refresh_live`; error changes reload job state; status changes apply the snapshot. Never panic on unknown/disconnected input.
- `refresh_monitor`: `daemon_active = daemon_liveness(self.daemon_age_secs, self.ipc_connected)`.
- `request_daemon_command`: after the DB insert, `send_wake_commands()` if connected.
- `enqueue_selected` and the import-destination rescan: `send_wake_jobs()`/`send_wake_commands()`.
- Settings path-save rescan: call the same wake helper after the direct DB insert.
- Add non-blocking one-shot send helpers using `cx.spawn` + `background_executor`; pause/shutdown wait for the result and show a message; fire-and-forget wakes only log failures.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS, including existing UI tests.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/Cargo.toml crates/ui/src/main.rs crates/ui/src/ipc_bridge.rs Cargo.lock
git commit -m "feat: connect the UI to the daemon IPC channel"
```

---

### Task 8: UI monitor controls and scan progress

**Files:**
- Modify: `crates/ui/src/main.rs`
- Test: inline tests in `main.rs`

**Interfaces:**
- Consumes: Task 7 bridge/state.
- Produces: monitor-screen daemon card with IPC status, pause/resume, rescan, shutdown, and scan progress; pure helpers `ipc_status_label`, `scan_progress_label`, `scan_progress_ratio`.

- [ ] **Step 1: Write the failing helper tests**

```rust
#[test]
fn scan_progress_label_uses_root_and_counts() {
    let p = ScanProgressView { phase: ScanPhase::Indexing, processed: 50, total: Some(100), indexed: 49, root: 2, roots: 3 };
    assert_eq!(scan_progress_label(&p), "フォルダ 2/3 · 索引 50/100");
    assert_eq!(scan_progress_ratio(&p), 0.5);
}

#[test]
fn ipc_status_label_distinguishes_disconnected_and_mismatch() {
    assert_eq!(ipc_status_label(true, None), "IPC 接続中");
    assert!(ipc_status_label(false, Some("protocol mismatch")).contains("未接続"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui scan_progress_label_uses_root_and_counts`
Expected: FAIL because the helpers do not exist.

- [ ] **Step 3: Implement the monitor card and controls**

- Add `daemon_card(&mut self, cx)` on `Screen::Monitor` after the stat cards. Show heartbeat sync, IPC state/error, paused state, and scan progress with a simple bar (`ratio` clamped 0..1).
- Add buttons: `再スキャン` (DB command + wake), `監視を一時停止`/`再開` (`SetPaused`), `デーモンを終了` (two-click confirmation: first click arms and updates the message, second sends `Shutdown`).
- Disable pause/shutdown when `ipc_connected` is false and show "デーモン停止中".
- Keep `daemon_error_card` and the existing heartbeat text unchanged.
- Update the sidebar status text to show "一時停止中" when `daemon_paused`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: add daemon IPC controls to the monitor screen"
```

---

### Task 9: ADR and documentation

**Files:**
- Create: `docs/adr/0011-daemon-ipc.md`
- Modify: `docs/adr/README.md`
- Modify: `docs/adr/0002-sqlite-shared-state.md`
- Modify: `docs/architecture.md`
- Modify: `docs/features.md`
- Modify: `docs/ui.md`
- Modify: `docs/troubleshooting.md`
- Modify: `docs/data-model.md`
- Modify: `docs/testing.md`
- Modify: `docs/roadmap.md`
- Modify: `docs/HANDOFF.md`
- Modify: `README.md`
- Modify: `docs/開発企画書.md` §7.2 (scope note)
- Include: `docs/superpowers/specs/2026-10-24-daemon-ipc-design.md` and this plan file

**Interfaces:**
- Consumes: implemented behavior from Tasks 1–8.
- Produces: documented decision and operator instructions.

- [ ] **Step 1: Write ADR-0011**

Record: local socket via `interprocess`, newline JSON, protocol version 1, SQLite complement, wake-only commands, heartbeat + IPC liveness, stale/live binding, Unix permissions, Windows pipe caveat, alternatives (localhost TCP, replace SQLite, faster polling), and consequences.

- [ ] **Step 2: Reconcile the existing docs**

- ADR-0002: mark "amended by ADR-0011"; change the "IPC not implemented" consequence to "complemented by IPC".
- Architecture: add IPC to the diagram and thread table; document startup order, wake signals, fallback, and socket path/permissions.
- Features/UI: monitor daemon card, pause/resume, rescan, shutdown, scan progress, IPC status.
- Troubleshooting: socket path, stale socket, permission errors, version mismatch, "daemon healthy but IPC disconnected", and the fact that commands still queue.
- Data model: `daemon_commands` remains the durable command channel; IPC only wakes; no schema change.
- Testing: add IPC transport/server tests, scanner observer test, and the UI helper/bridge tests to the test matrix.
- Roadmap/HANDOFF/README: IPC first slice implemented; Windows ACL verification remains; open question 4 is resolved.
- `docs/開発企画書.md` §7.2: note the implemented subset and future commands/events.

- [ ] **Step 3: Check documentation consistency**

Run: `rg -n "IPCは未実装|IPC.*未実装|socket|daemon_commands|wake_commands|daemon_active" README.md docs`
Expected: no stale "IPC is not implemented" claims; all new terms match the code.

- [ ] **Step 4: Commit**

```bash
git add docs README.md
git commit -m "docs: document the daemon IPC decision and operation"
```

---

### Task 10: Full verification and manual smoke test

**Files:**
- Modify: any files needed for fixes found below.
- Test: workspace commands and a temporary manual run.

**Interfaces:**
- Consumes: all prior tasks.
- Produces: verified branch ready for PR.

- [ ] **Step 1: Run formatting, lints, and tests**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all pass.

- [ ] **Step 2: Build the workspace**

Run: `cargo build --workspace`
Expected: both binaries and the IPC crate build without warnings. The `Release` workflow covers the packaged release build.

- [ ] **Step 3: Run the manual IPC smoke test**

Use an isolated data/runtime directory so the real library is untouched:

```bash
SMOKE=$(mktemp -d /tmp/opencode/vrc-ipc-smoke.XXXXXX)
mkdir -p "$SMOKE/data" "$SMOKE/runtime" "$SMOKE/photos" "$SMOKE/logs"
XDG_DATA_HOME="$SMOKE/data" XDG_RUNTIME_DIR="$SMOKE/runtime" \
  ./target/debug/vrc-photo-daemon --no-tray --photo-dir "$SMOKE/photos" --log-dir "$SMOKE/logs" &
```

Run the UI with the same `XDG_DATA_HOME`/`XDG_RUNTIME_DIR`, then verify:

1. The monitor card shows "IPC 接続中".
2. Rescan starts immediately (scan progress appears; no three-second wait).
3. Pause changes the state and the monitor card; resume works.
4. A compression job's state/progress appears without waiting for the refresh tick.
5. Shutdown exits the daemon; the UI stays open and changes to "デーモン停止" after the heartbeat ages out.
6. Start the daemon again; the UI reconnects automatically.

- [ ] **Step 4: Stop the smoke processes and clean up**

Run: `kill <daemon-pid>` if still running, then `rm -rf "$SMOKE"`.
Expected: no stray daemon/UI processes.

- [ ] **Step 5: Commit any fixes from verification**

```bash
git add -A
git commit -m "fix: address IPC verification findings"
```

Only create this commit if Step 1–4 required changes. Otherwise leave the branch clean.
