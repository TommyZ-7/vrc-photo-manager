# Sidebar Progress Card: Design

- Date: 2026-10-25
- Status: Draft for approval
- Depends on: PR #99 (`feat/import-ux`, import progress and report fields)
- Target: `crates/ui/src/main.rs`, `docs/ui.md`

## 1. Intent

The sidebar currently ends with a static library card, a daemon status row,
and a version label. Compression, import, and scan progress are spread across
the Dashboard, the Photos import panel, and the daemon card. The user wants a
single compact card directly above the library card that switches between
those three progress views, and can start waiting compression jobs without
opening the Dashboard.

## 2. Goals

- Show compression, import, and scan status in one card.
- Switch views with three compact tabs; no stacked mini cards.
- Show activity dots on the other tabs so failures or running work are not
  hidden while another tab is selected.
- Start all `waiting` compression jobs from the compression view, using the
  existing confirmation flow when the policy is destructive.
- Retry failed jobs from the same view when there are no waiting jobs.
- Keep the card hidden while the sidebar is collapsed, matching the library
  card.
- Reuse existing polling/IPC updates; no new DB table or setting.

## 3. Non-goals

- Moving import runs out of the import modal, or keeping an import running
  after the modal closes.
- Persisting the selected tab across restarts.
- Auto-rotating or auto-switching tabs. A dot signals activity; the user
  chooses when to look.
- Replacing the Dashboard queue or the Photos import report panel.
- New queue operations beyond releasing waiting jobs and retrying failures.

## 4. Approaches

### A. One card with segmented tabs (chosen)

A three-segment control (`圧縮` / `取り込み` / `スキャン`) inside one card.
Each view has a headline, one detail line, an optional progress bar, and one
footer action. Activity dots on the tabs carry the state of the inactive
views.

Trade-offs: slightly denser tab row; in exchange every progress domain has a
stable home and the card height does not jump when switching.

### B. Auto-cycling single view

One view that rotates through active domains.

Trade-offs: no manual control; a user reading one status can lose it, and
idle domains waste a cycle. Rejected.

### C. Stacked mini cards

Three fixed rows, one per domain.

Trade-offs: violates the single-card requirement and consumes too much
vertical space above the library card. Rejected.

## 5. Placement and layout

- Rendered in the expanded sidebar, directly above the library card.
- Not rendered when `sidebar_collapsed` is true.
- Same width and panel treatment as the library card (`PANEL`, 13 px radius,
  1 px low-contrast border). No new colors or fonts.
- Fixed compact height (about 134 px) so switching tabs never moves the
  library card.
- Header: `進行状況` and a 6 px status dot for the selected view.
- Tabs: three equal-width 20 px segments. Active: `ACCENT_SOFT` background,
  `BLUE` text, subtle blue border. Inactive: `MUTED_2`, hover `PANEL_2`.
  A 4 px dot at the top-right marks activity: red for failure, amber for
  waiting/unprocessed, blue for running.
- Body: 13 px tone-colored headline, 10 px `MUTED` detail with truncation,
  and a 2 px progress bar for import/scan when a ratio is available.
- Footer: one or two `Button::small()` actions, 24 px high, left aligned.
- Default tab: compression. The selection lives in `App` for the session.
- No motion beyond existing hover/click feedback; the card must not animate
  on every 3-second refresh.

## 6. View content

### Compression

- Headline priority:
  - `未実行 N` (amber) when waiting jobs exist.
  - `実行中 N` (blue) when pending/processing jobs exist.
  - `失敗 N` (red) when only failed jobs remain.
  - `キューは空です` (muted) otherwise.
- Detail: `実行中 N · 失敗 N`; when idle, `未実行のジョブはありません`.
- Footer:
  - Waiting jobs: primary `未実行を開始` -> `request_release_waiting_jobs`.
  - Failed jobs: ghost `再試行` -> `retry_all_failed_jobs`; shown
    alongside the waiting action when both exist.
  - Neither: ghost `キューを開く` -> Dashboard.
- `実行中` means `active_job_count - waiting_job_count`, clamped at zero.

### Import

- Running import (dialog phase `Running`): headline `取り込み中` (blue),
  detail `processed/total · 取込 N · 失敗 N`, progress bar from
  `import_progress_ratio`.
- Last report with unprocessed sources: `未処理 N` (amber), detail
  `取込 N/total · 失敗 N`.
- Last report with failures: `失敗 N` (red), detail `取込 N/total` or
  `取込 N/total · スキャン失敗 N` when destination indexing failed.
  Indexing failures (`scan_failures` / `scan_error`) count toward the
  failure headline and the red activity dot.
- Last report with only partial moves: `部分移動 N` (amber), detail
  `取込 N/total · 完了`.
- Last completed report: `完了 N 枚` (green), detail `前回の取り込み` or
  `前回の取り込み · 連番 N`.
- No report: `履歴なし` (muted), detail `取り込み結果がここに残ります`.
- Footer: `結果を見る` when a report exists (opens Photos, where the report
  panel is mounted), otherwise `写真を開く`.
- The modal remains the primary progress surface during a run; the card
  mirrors it but is normally behind the modal scrim.

### Scan

- Scan running with a known total: headline `NN%` (blue), detail from
  `scan_progress_label`, progress bar from `scan_progress_ratio`.
- Scan running without a total: `走査中…` (blue), same detail, no bar.
- Daemon paused: `一時停止中` (amber), detail `監視を再開すると走査します`.
- Daemon stopped: `停止中` (muted), detail `デーモンは接続されていません`.
- Daemon active and idle: `待機中` (muted), detail `変更を監視しています`.
- Footer: `再スキャン` when the daemon is active and idle, otherwise
  `ダッシュボード` (opens Dashboard).

## 7. State and data

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarStatusTab {
    Compression,
    Import,
    Scan,
}

struct CardStatus {
    headline: String,
    detail: String,
    tone: StatusTone,
}

enum StatusTone {
    Idle,
    Running,
    Waiting,
    Failed,
    Success,
}
```

- New `App` field: `sidebar_status_tab: SidebarStatusTab`, default
  `Compression`.
- Inputs: `active_job_count`, `waiting_job_count`, `failed_job_count`,
  `import_dialog`, `import_report`, `scan_progress`, `daemon_active`,
  `daemon_paused`.
- `import_dialog` access is read-only: the card extracts the current
  `ImportProgress` from `ImportDialogPhase::Running`.
- Pure helpers build each `CardStatus` so the text can be unit-tested:
  `compression_card_status`, `import_card_status`, `scan_card_status`,
  `running_job_count`.
- The tab attention dots derive from the same inputs and do not mutate state.

## 8. Actions

- `未実行を開始`: existing `request_release_waiting_jobs(window, cx)`; the
  destructive replace policy keeps its alert dialog.
- `失敗を再試行`: existing `retry_all_failed_jobs(cx)`.
- `結果を見る`: `switch_screen(Screen::Photos, cx)`.
- `写真を開く`: same Photos switch.
- `キューを開く` / `ダッシュボード`: `switch_screen(Screen::Dashboard, cx)`.
- `再スキャン`: existing `request_daemon_rescan(cx)`.
- No new background work or blocking DB call is introduced by rendering.

## 9. Tests

Pure unit tests in `crates/ui/src/main.rs`:

- Compression headline/detail/tone for waiting, running, failed, and idle.
- `running_job_count` clamps when waiting exceeds active.
- Import status for running progress, remaining, failures, success, and no
  report. Reuse the import report fixture.
- Scan status for known total, unknown total, paused, stopped, and idle.
- Tone colors map to the intended existing palette constants.

Manual QA:

- Expanded sidebar shows the card above the library card; collapsed sidebar
  hides it.
- Tabs switch without moving the library card.
- Waiting jobs start from the card, including the replace confirmation.
- Failed jobs retry from the card when no waiting jobs remain.
- A completed import shows `完了`/`失敗`/`未処理` and opens the Photos report.
- A rescan shows the percentage and bar, then returns to `待機中`.
- Screenshot check at the default window size and at a narrow one.

## 10. Documentation

- `docs/ui.md` 2.1: add the status card, tabs, activity dots, compression
  action, import result link, and collapsed behavior.
- `docs/features.md`: note that the sidebar can release waiting jobs, next to
  the existing Dashboard action.
- User-facing docs land in a separate docs-only PR after the code PR.

## 11. Left to the builder

- Exact split between inline tab rendering and helper methods.
- Whether the footer uses one button row or wraps at very narrow sidebars.
- Exact empty-state wording beyond the quoted strings.
- Whether the card header should also link to the Dashboard.
