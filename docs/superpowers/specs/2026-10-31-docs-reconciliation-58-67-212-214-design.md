# Docs reconciliation for issues 58, 67, 212, and 214

- Date: 2026-10-31
- Issues: [#58](https://github.com/TommyZ-7/vrc-photo-manager/issues/58),
  [#67](https://github.com/TommyZ-7/vrc-photo-manager/issues/67),
  [#212](https://github.com/TommyZ-7/vrc-photo-manager/issues/212),
  [#214](https://github.com/TommyZ-7/vrc-photo-manager/issues/214)
- Status: Approved

## Intent

Bring the architecture docs, the IPC spec/plan, and two Rust doc comments back
in line with the current implementation. No runtime behavior changes.

## Decisions

### #58: SQLite command queue and troubleshooting

- ADR-0002 states that `daemon_commands` is the implemented durable command
  queue and that ADR-0011's local IPC (implemented) only wakes the daemon and
  carries events.
- `docs/troubleshooting.md` describes the current relink paths: a rescan/relog
  also runs linking, and a link error's "再試行" enqueues `relink`.
- `docs/ui.md` corrects the dashboard stat card labels to match the code
  (写真 / 未完了ジョブ / ライブラリ容量 / デーモン).
- Queue limits, `daemon_errors.attempts`, and the log-card stderr fallback are
  re-checked; their existing wording already matches the code.

### #67: Version mismatch stays a `hello` response

The implementation is canonical. On a version mismatch the daemon sends a
`Hello` response containing its own `protocol_version`, then closes the
connection. The client compares the versions and raises
`IpcError::ProtocolMismatch { daemon, client }`, which the UI turns into
「デーモンの更新が必要です」. No `protocol_mismatch` error response is sent.

- Update the IPC design spec and implementation plan to describe this flow.
- Remove the unused `ErrorCode` variants `ProtocolMismatch`, `UnknownRequest`,
  and `Internal`; keep `InvalidRequest`, the only code the server emits.

### #212: Import scan doc comment

Move the "Adds dest to the settings roots..." paragraph back to
`add_photo_root_and_save`, and give `request_import_scan` a one-line doc that
says it requests a scan of only the import destination root.

### #214: `CountDebounce` doc

Describe the type as a quiet-period tracker for either the photo count or
`library_revision`, and name both call sites (`lifecycle.rs` count path and
revision path).

## Verification

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- Confirm no remaining references to the removed `ErrorCode` variants.

## Out of scope

- Any change to IPC wire behavior.
- Re-auditing unrelated documentation beyond the four issues.
