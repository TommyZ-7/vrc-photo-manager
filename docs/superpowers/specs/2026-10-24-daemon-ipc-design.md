# UI⇔デーモンIPC: Design

- Date: 2026-10-24
- Issue: [#18 feat: UI⇔デーモンIPC](https://github.com/TommyZ-7/vrc-photo-manager/issues/18)
- Status: Proposed
- Target: new `crates/ipc`, `crates/core/src/scanner.rs`, `crates/daemon/src/{lib,main}.rs`, `crates/ui/src/main.rs`, docs

## 1. Intent

The UI and daemon currently share SQLite (WAL) and the UI refreshes every
three seconds. A retry request waits for the daemon's next loop, and a
compression job's progress is invisible until the next UI tick. The issue asks
for a real command/event channel without breaking two existing guarantees:

- the daemon keeps working when the UI is not running;
- the UI keeps working, and shows the daemon as stopped, when the daemon is not
  running.

The existing `daemon_commands` queue (added for #17) already makes rescan,
relog, and relink durable across daemon restarts. This change keeps that queue
and adds IPC as a low-latency wake-up and event channel. SQLite remains the
source of truth for durable state; IPC is a complement, not a replacement.

## 2. Goals

- Add a local-only IPC transport with a versioned, newline-delimited JSON
  protocol.
- Make UI-initiated daemon commands take effect immediately when the daemon is
  connected.
- Deliver scan progress, compression progress, daemon error changes, and pause
  state changes to the UI without a three-second wait.
- Add UI controls for rescan, pause/resume monitoring, and daemon shutdown.
- Keep the heartbeat, the command queue, the three-second refresh fallback, and
  the UI's behavior when the daemon is absent.
- Document the decision in an ADR and reconcile the existing architecture docs.

## 3. Non-goals

- Replacing SQLite as the shared state or moving photo/job data into IPC.
- Streaming every file-system watcher event to the UI.
- Remote access, TCP, authentication tokens, or multi-user IPC.
- True per-pixel compression progress. Compression remains one encoder call;
  progress events report the phases the daemon can observe (start, finish,
  failure).
- A daemon auto-restart policy or process supervision beyond the existing
  single-instance and autostart behavior.
- Changing the log parser, XMP handling, or scanner semantics.

## 4. Current behavior and constraints

- `Db::request_daemon_command` inserts a deduplicated command; the daemon drains
  it at the top of its 500 ms loop. The UI also polls every three seconds.
- `DaemonStatus.paused` is in memory only. The tray toggles it directly. A UI
  pause control does not exist.
- Compression workers poll `next_pending_job` every 400 ms and update progress
  only at 0.05 and 1.0.
- `scanner::scan_and_index` first walks a root into a `Vec<ScannedPhoto>`, then
  indexes every photo. There is no progress callback.
- Daemon errors live in `daemon_errors`; the UI reads them on the three-second
  tick.
- The heartbeat (`app_settings.daemon_heartbeat`) is written every 10 seconds and
  is the UI's primary "daemon active" signal. It must stay; the UI will also
  treat a live IPC subscription as liveness while a long scan blocks the loop.
- Linux packages ship a Windows build too. `interprocess` supports Unix domain
  sockets and Windows named pipes with one API.
- `docs/開発企画書.md` §7.2 already names `interprocess`, newline-delimited
  JSON, `Request`/`Response`/`Event`, and a version handshake as the intended
  design. This document makes that concrete.

## 5. Approaches considered

### 5.1 Transport

**A. `interprocess` local socket (chosen).** Unix domain socket on Unix and a
named pipe on Windows. Local-only by construction, no port allocation, no
network token, and the same Rust API on both platforms. Cost: socket lifecycle
(stale files, permissions) and platform-specific endpoint naming.

**B. Localhost TCP.** Easy to inspect with standard tools and works everywhere,
but needs a port, a discovery file, and a bearer token to avoid other local
users connecting. It also exposes a listening socket on the network stack,
which conflicts with the project's "no network communication" stance.

**C. SQLite-only with faster polling.** Smallest change, but still polling and
cannot push compression progress or scan progress. It does not satisfy the
issue's intent.

### 5.2 Relationship to SQLite

**A. Complement SQLite (chosen).** Keep the command queue, jobs, errors, and
heartbeat in SQLite. IPC wakes the daemon and pushes events. Durable commands
still survive a daemon restart, and an absent daemon only degrades latency.

**B. Replace SQLite for control.** IPC becomes the only command path. This loses
the offline queue, needs its own persistence, and makes UI/daemon startup order
matter. Existing retry behavior would regress.

**C. Replace all shared state with IPC.** A much larger change with no benefit
for photo/job reads and much higher failure risk.

## 6. Design

### 6.1 Crate and dependency direction

Add `crates/ipc` (`vrc-photo-ipc`). It contains only:

- protocol types and version constant;
- endpoint resolution (socket path / pipe name);
- framing helpers;
- a synchronous server listener/connection and client request/subscription
  helpers.

It does not depend on `core`, `daemon`, or `ui`. Dependencies: `interprocess`
2.4, `serde`, `serde_json`, `thiserror`, `tracing`.

```text
ui ────────┬──> ipc
           └──> core
daemon ────┼──> ipc
           └──> core
```

### 6.2 Endpoint

- Unix: `${XDG_RUNTIME_DIR}/vrc-photo-manager/daemon.sock` when
  `XDG_RUNTIME_DIR` is set, otherwise `<data_dir>/run/daemon.sock`. The parent
  directory is created with mode 0700 and the socket with mode 0600. If the
  resulting path exceeds the Unix socket path limit, fall back to
  `/tmp/vrc-photo-manager-<uid>/daemon.sock` (directory 0700, socket 0600).
- Windows: a namespaced pipe name containing the user name, for example
  `vrc-photo-manager-daemon-<user>`. The pipe is local-only; remote pipe access
  is disabled by the transport defaults.
- Binding probes the existing endpoint **before** unlinking anything:
  - If `hello` succeeds, a live daemon owns the endpoint. Do not unlink it;
    return `AddrInUse`. `Daemon::run` logs a warning and continues without IPC.
    The existing `single-instance` guard is still the primary duplicate
    protection.
  - If the connection is refused, treat the socket as stale, unlink it, and
    bind.
  - Any other bind error is logged and the daemon continues headless.
- Tests can construct an explicit temp path or a unique namespaced endpoint so
  parallel tests do not collide.

### 6.3 Protocol

Newline-delimited JSON, one object per line, UTF-8. `PROTOCOL_VERSION = 1`.
Envelope:

```json
{"id":1,"request":{"type":"hello","protocol_version":1,"client":{"name":"vrc-photo-manager","version":"0.3.0"}}}
{"id":2,"request":{"type":"wake_commands"}}
{"type":"event","event":{"type":"compression_progress","job_id":"...","photo_id":42,"status":"processing","progress":0.05}}
{"type":"response","id":2,"response":{"type":"ok"}}
```

Requests:

| Request | Purpose |
|---|---|
| `hello` | Version handshake; returns daemon version, PID, and status |
| `status` | Current status snapshot |
| `wake_commands` | Wake the daemon loop after the UI inserted a durable command |
| `wake_jobs` | Wake compression workers after the UI enqueued a job |
| `set_paused { paused }` | Pause/resume monitoring in memory |
| `shutdown` | Graceful daemon shutdown |
| `subscribe` | Switch the connection to event streaming |

The command actions (`rescan`, `relog`, `relink`) travel through the existing
`daemon_commands` queue. `wake_commands` is deliberately **wake-only**: the UI
inserts the durable row first, so a wake that arrives before or after the
daemon's drain cannot create a second command or a duplicate rescan. A
non-UI client that wants a command must use the shared database, not a
separate IPC command path.

Responses: `hello`, `status`, `subscribed`, `ok`, `error { code, message }`.
The only error code emitted today is `invalid_request` (for a missing or
repeated `hello`). Version mismatches do not use an error response.

Status snapshot: `paused`, `watching`, `scanning`, `scan_phase`,
`scan_processed`, `scan_total`, `scan_indexed`, `scan_root`,
`scan_roots`, `photo_count`, `pending_jobs`.

Events:

| Event | Fields |
|---|---|
| `scan_started` | `roots` |
| `scan_progress` | `phase` (`walking`/`indexing`), `processed`, `total`, `indexed`, `root`, `roots` |
| `scan_finished` | `found`, `indexed`, `skipped`, `failures`, `failed` |
| `compression_progress` | `job_id`, `photo_id`, `status`, `progress`, optional `error` |
| `daemon_error_changed` | `operation` (`scan`/`log`/`link`) |
| `status_changed` | full status snapshot |

On a version mismatch the daemon still replies with `hello`, carrying its own
`protocol_version`, and then closes the connection. The client compares the
versions and raises `IpcError::ProtocolMismatch { daemon, client }`; the UI
shows 「デーモンの更新が必要です」 and keeps using the heartbeat/SQLite
fallback.

### 6.4 Daemon server

`crates/daemon/src/ipc_server.rs` owns a listener thread and one thread per
connection.

- The listener uses non-blocking accept with a 50 ms poll and a stop flag, so
  shutdown can join it. The socket file is removed on drop.
- Each connection must start with `hello`. After that it may issue one or more
  requests, or switch to `subscribe`.
- Subscribers are registered in an `EventBus` (an `Arc<Mutex<Vec<Sender<Event>>>>`).
  Publishing uses non-blocking sends; dead subscribers are pruned.
- The server receives a `DaemonControl` handle containing:
  - `running: Arc<AtomicBool>`;
  - `status: Arc<Mutex<DaemonStatus>>`;
  - `wake: Arc<WakeSignal>` for the main loop;
  - `jobs: Arc<JobSignal>` for compression workers;
  - `events: EventBus`.
- `wake_commands` only notifies `wake`; it does not insert a command (the UI
  already inserted the durable row). The IPC handlers never touch SQLite, so a
  long full scan holding the daemon's main connection cannot block them.
- Status responses use the cached `DaemonStatus` snapshot rather than querying
  SQLite on every request. Counts may be one refresh behind during a scan; the
  scan-specific fields are updated live by the scan observer.
- `set_paused` updates `DaemonStatus`, notifies `wake`, and publishes
  `status_changed`.
- `shutdown` replies `ok`, clears `running`, and notifies `wake`.
- If binding fails, the daemon logs a warning and continues without IPC. The
  heartbeat and command queue keep working.

The server starts at the beginning of `Daemon::run`, before the initial scan, so
a UI that is already open can observe initial scan progress. `run_once` and
`compress_pending` do not start a server.

### 6.5 Daemon loop and workers

- Replace `thread::sleep(SCAN_INTERVAL)` in the main loop (including the paused
  branch) with a `WakeSignal` wait. IPC commands, pause changes, and shutdown
  wake the loop immediately; the 500 ms timeout remains as a fallback.
- Replace the compression worker's fixed 400 ms sleep with a `JobSignal`
  generation counter plus `Condvar`. `wake_jobs` and daemon-side auto-enqueue
  notify all workers; the queue remains the source of truth.
- `DaemonStatus` gains `scanning`, `scan_phase`, `scan_processed`,
  `scan_total`, `scan_indexed`, `scan_root`, and `scan_roots`, so a UI that
  connects mid-scan gets the current state in the hello/subscribed response.
- A full `scan_photos` call emits one `scan_started` / `scan_finished` pair.
  Progress is per root (`root` is the 1-based root index, `roots` the count)
  and is throttled to at most every 200 ms plus phase changes. `scan_finished`
  is published after missing-marking, revision bump, auto-enqueue, relink, and
  thumbnail dispatch, so the UI's single reload sees the final state. Errors
  from the scan still follow the existing `daemon_errors` path and also publish
  `daemon_error_changed`.
- Compression workers publish `compression_progress` when a job starts (0.05)
  and after `process_job` returns with completion (1.0) or failure. Completion
  therefore includes the derivative insert and library revision bump.
- `report_daemon_error` and `clear_daemon_error` publish
  `daemon_error_changed` only when the stored state actually changed.

### 6.6 Scan progress in core

Add an observed variant of the scanner:

```rust
pub enum ScanPhase { Walking, Indexing }

pub struct ScanProgress {
    pub phase: ScanPhase,
    pub processed: usize,
    pub total: Option<usize>,
    pub indexed: usize,
}

pub fn scan_and_index_observed<F>(
    db: &Db,
    root_id: i64,
    root: &Path,
    observe: F,
) -> Result<ScanReport>
where
    F: FnMut(ScanProgress);
```

- During the directory walk, `phase = Walking`, `total = None`; `processed`
  counts candidate photos found.
- During indexing, `total = Some(photos.len())`; `processed` counts indexed or
  skipped photos and `indexed` counts successful upserts/rebinds.
- `scan_and_index` delegates with a no-op observer, so existing callers and
  tests keep their behavior.
- The daemon throttles observer callbacks into IPC events; the core callback is
  cheap and can fire per photo.

### 6.7 UI client

Add an IPC pump that starts with the app:

- A dedicated thread owns the subscription connection and reconnects every two
  seconds after a disconnect.
- An `async-channel` carries `Connected(status)`, `Disconnected(error)`, and
  `Event` messages to a GPUI async task, which updates the app on the UI thread.
- A transient request helper connects, performs `hello` plus one request, reads
  one response, and closes. It runs on a GPUI background task or a worker
  thread with a two-second timeout, never on the UI thread. Commands are
  infrequent, so per-request connections keep the client free of shared-stream
  locking.
- `ipc_connected` is true only while a subscription is active. It is displayed
  separately from the heartbeat, so "daemon running but IPC disconnected" is
  visible.
- **Liveness:** `daemon_active` becomes `heartbeat_fresh || ipc_connected`.
  A full scan can hold the daemon's main loop past the 20-second heartbeat
  window; without this, the UI would show "デーモン停止" while scan progress is
  arriving. The heartbeat remains the fallback for an old daemon or a broken
  IPC connection. The monitor card shows both signals.
- On disconnect, clear scan progress, keep the heartbeat-based status, and keep
  retrying. Do not show a modal or overwrite the current user message.

UI actions:

- **Rescan / retry commands:** always insert into `daemon_commands` first (so
  the request survives a daemon restart), then send `wake_commands` over IPC if
  connected. The daemon only wakes; it never inserts a second row. If IPC is
  absent, the existing 500 ms loop consumes the row.
- **Compression enqueue:** keep the existing DB insert, then send `wake_jobs`.
- **Pause/resume:** send `set_paused` and wait up to two seconds for the reply;
  disabled with "デーモン停止中" when `ipc_connected` is false.
- **Shutdown:** send `shutdown` with a second-click confirmation to avoid an
  accidental quit.
- **Monitor screen:** add a daemon card with IPC state, paused state, current
  scan progress, rescan, pause/resume, and shutdown. Keep the existing heartbeat
  and error cards.
- **Compression queue:** on `compression_progress`, reload job state and refresh
  live state immediately; on completion, the library revision/count path also
  refreshes the grid.

The existing three-second `refresh_live` stays as a compatibility and
loss-recovery fallback. It is no longer the only path for job progress.

### 6.8 Error and edge behavior

- Daemon absent: subscription fails; UI marks IPC disconnected, keeps
  heartbeat-driven status, and commands still queue in SQLite.
- UI absent: daemon runs normally; no subscribers, events are dropped.
- Version mismatch: handshake error; UI shows "デーモンの更新が必要です" in the
  daemon card; heartbeat still works.
- Stale socket: the bind probe treats a refused connection as stale, removes
  the file, and binds. If a live server answers, it is never unlinked; the new
  process continues without IPC and the single-instance guard remains the
  primary duplicate protection.
- Event loss: fallback poll refreshes jobs, errors, and library state.
- Slow subscriber: events are unbounded but low-volume and throttled; dead
  senders are pruned on publish.
- Long scan: progress events are throttled; the UI does not rebuild the library
  on every progress event, only on scan completion.
- DB lock ordering: IPC handlers never touch SQLite; they only read the cached
  status and notify signals. Event publishing does not hold a DB lock.

## 7. Testing

- `crates/ipc`: protocol serde round-trips, hello/version mismatch, request/
  response over a temp Unix socket (or unique pipe on Windows), subscription
  delivery, stale socket replacement (Unix).
- `crates/core`: `scan_and_index_observed` emits walking and indexing progress
  with a final total, and the no-observer wrapper keeps current results.
- `crates/daemon`: server tests with a fake control: `wake_commands` wakes
  without touching SQLite, `set_paused` updates status and publishes an event,
  `shutdown` clears running and wakes, and `subscribe` receives published
  events.
- `crates/ui`: pure helper tests for scan-progress labels/percentages and IPC
  status labels. UI behavior is checked in the demo/normal app manually because
  there is no GUI test harness.
- Manual smoke: run the daemon against a temp photo/log directory, run the UI,
  verify rescan is immediate, pause/resume shows in the monitor card, a
  compression job's progress appears within the event path, `shutdown` leaves
  the UI alive showing "デーモン停止", and stopping the daemon externally does
  not crash the UI.
- Pre-push: `cargo fmt --all -- --check &&
  cargo clippy --workspace --all-targets -- -D warnings &&
  cargo test --workspace`.

## 8. Documentation

- Add `docs/adr/0011-daemon-ipc.md` with the decision, consequences, and
  alternatives.
- Amend ADR-0002 (complement, not replacement) and keep it as the shared-state
  ADR.
- Update `docs/architecture.md` (diagram, threads, lifecycle, fallback),
  `docs/features.md`, `docs/ui.md`, `docs/troubleshooting.md`,
  `docs/data-model.md` (command queue semantics), `docs/roadmap.md`,
  `docs/HANDOFF.md`, and `README.md`.
- Document the socket path, permissions, stale-socket recovery, version
  mismatch, and the fact that IPC can be unavailable while the daemon is
  healthy.
- Add a scope note to `docs/開発企画書.md` §7.2: this issue implements the
  first slice (`status`, command wake, job wake, pause/resume, shutdown, scan/
  compression/error events). `Import`, `Compress`, `CancelJob`, `OpenDataDir`,
  the 5-second startup wait, and the broader event list remain future work.
  The issue's acceptance criteria do not depend on them.

## 9. Acceptance mapping

| Issue acceptance criterion | Implementation |
|---|---|
| UIから再スキャンを即時実行できる | DB insert + `wake_commands`; daemon loop no longer waits for the 500 ms tick |
| 圧縮進捗がポーリング待ちなしで表示される | worker `compression_progress` events; UI reloads jobs immediately |
| デーモン未起動時にUIが落ちず、従来通り状態表示できる | reconnect loop, heartbeat unchanged, IPC errors are non-fatal, commands remain in SQLite |

## 10. Risks and open items

- Windows named-pipe ACLs are not explicitly tightened. The transport is
  local-only; verify the default DACL on a Windows machine during the existing
  Windows validation work. If needed, add an explicit current-user-only
  security descriptor in a follow-up.
- If the Unix socket path is unusually long, the endpoint helper falls back to
  a per-user `/tmp` directory. Confirm the fallback on the target distros.
- Compression progress is phase-based, not encoder-percentage-based. A future
  encoder change can emit more events without changing the protocol.
- Adding a workspace crate changes the release bundle dependency graph; verify
  `Release` on the PR (bundle build) before merge.
