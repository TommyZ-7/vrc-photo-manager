# Sidebar Progress Card Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add one compact status card above the sidebar library card that switches between compression, import, and scan progress, and can release waiting compression jobs without opening the Dashboard.

**Architecture:** Add a `SidebarStatusTab` field to `App` and pure status-summary helpers. Render a single `sidebar_status_card` above the existing library card, reading the compression counters, `import_dialog`/`import_report`, and `scan_progress` that are already refreshed by polling and IPC. Actions call the existing queue, retry, screen-switch, and rescan methods.

**Tech Stack:** Rust 2024, gpui-kit 0.8 / gpui-component, existing SQLite-backed state and 3-second polling.

**Spec:** `docs/superpowers/specs/2026-10-25-sidebar-status-card-design.md`

## Global Constraints

- No new dependency, DB table, column, or persisted setting.
- The selected tab lives in `App` memory only; default is compression and it does not auto-switch.
- The card is rendered only when the sidebar is expanded, directly above the library card.
- The card height stays stable across tabs; the library card must not jump.
- Use the existing palette constants (`PANEL`, `PANEL_2`, `ACCENT_SOFT`, `TEXT`, `MUTED`, `MUTED_2`, `BLUE`, `AMBER`, `RED`, `GREEN`) and existing 13 px card radius.
- Compression actions are the existing `request_release_waiting_jobs` (with its replace-policy confirmation) and `retry_all_failed_jobs`.
- Import progress is read from `ImportDialogPhase::Running`; import runs themselves are not moved out of the modal.
- Do not include this plan or the design spec in the code PR; they belong in the separate docs PR.
- Before completion: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` must pass.

## Review Focus

- **Inconsistent counters:** `active_job_count` includes waiting jobs, so `active - waiting` must clamp at zero rather than show a negative running count. Task 1 tests this.
- **Import report priority:** a report can have failures, unprocessed sources, and partial moves at once. The headline must prefer unprocessed, the detail must keep the failure count, and the activity dot must stay red when failures exist. Task 1 tests both.
- **Scan without a total:** walking scans and indexing scans with `total = None` must show `走査中…`, not `0%` or a zero-width progress bar. Task 1 tests both.
- **Long detail text at 238 px sidebar width:** detail lines truncate and the body keeps a fixed height, so switching tabs does not move the library card. Task 3 checks this visually.
- **Background refresh while reading another tab:** the 3-second refresh and IPC events update counts but must not reset the selected tab or replay an animation. Task 3 checks this while switching and waiting for a refresh.

---

### Task 1: Status model and pure helpers

**Files:**
- Modify: `crates/ui/src/main.rs`
  - Types near `ManualCompressionDialog` (currently lines 1406-1417).
  - Helpers after `scan_progress_ratio` (currently ends at line 10915).
  - Tests at the end of the `#[cfg(test)] mod tests` block.

**Interfaces:**
- Consumes: `ImportProgress`, `ImportReportView`, `ImportDialogState`, `ImportDialogPhase`, `ScanProgressView`, `import_issue_counts`, `scan_progress_label`, `scan_progress_ratio`.
- Produces:
  - `enum SidebarStatusTab { Compression, Import, Scan }` with `Clone, Copy, Debug, PartialEq, Eq`.
  - `enum StatusTone { Idle, Running, Waiting, Failed, Success }` with `Clone, Copy, Debug, PartialEq, Eq` and `fn color(self) -> u32`.
  - `struct CardStatus { headline: String, detail: String, tone: StatusTone }` with `Clone, Debug, PartialEq, Eq`.
  - `fn running_job_count(active: i64, waiting: i64) -> i64`.
  - `fn compression_card_status(active: i64, waiting: i64, failed: i64) -> CardStatus`.
  - `fn import_card_status(progress: Option<&ImportProgress>, report: Option<&ImportReportView>) -> CardStatus`.
  - `fn scan_card_status(progress: Option<&ScanProgressView>, daemon_active: bool, daemon_paused: bool) -> CardStatus`.
  - `fn compression_attention(active: i64, waiting: i64, failed: i64) -> Option<StatusTone>`.
  - `fn import_attention(progress: Option<&ImportProgress>, report: Option<&ImportReportView>) -> Option<StatusTone>`.
  - `fn scan_attention(progress: Option<&ScanProgressView>, daemon_active: bool, daemon_paused: bool) -> Option<StatusTone>`.
  - `fn import_running_progress(dialog: Option<&ImportDialogState>) -> Option<&ImportProgress>`.

- [ ] **Step 1: Write the failing tests**

Add these tests at the end of the test module. Add `AtomicBool` to the test imports with `use std::sync::atomic::AtomicBool;`, and extend the existing `use super::{...}` list with `AMBER, BLUE, GREEN, MUTED_2, RED`, `StatusTone`, `compression_attention`, `compression_card_status`, `import_attention`, `import_card_status`, `import_running_progress`, `running_job_count`, `scan_attention`, `scan_card_status`.

```rust
#[test]
fn running_job_count_clamps_waiting_above_active() {
    assert_eq!(running_job_count(6, 2), 4);
    assert_eq!(running_job_count(2, 6), 0);
    assert_eq!(running_job_count(0, 0), 0);
}

#[test]
fn compression_card_status_prioritizes_waiting_running_and_failed() {
    let waiting = compression_card_status(6, 5, 2);
    assert_eq!(waiting.headline, "未実行 5");
    assert_eq!(waiting.detail, "実行中 1 · 失敗 2");
    assert_eq!(waiting.tone, StatusTone::Waiting);

    let running = compression_card_status(4, 1, 0);
    assert_eq!(running.headline, "実行中 3");
    assert_eq!(running.detail, "未実行 0 · 失敗 0");
    assert_eq!(running.tone, StatusTone::Running);

    let failed = compression_card_status(0, 0, 2);
    assert_eq!(failed.headline, "失敗 2");
    assert_eq!(failed.detail, "未実行 0 · 実行中 0");
    assert_eq!(failed.tone, StatusTone::Failed);

    let idle = compression_card_status(0, 0, 0);
    assert_eq!(idle.headline, "キューは空です");
    assert_eq!(idle.detail, "未実行のジョブはありません");
    assert_eq!(idle.tone, StatusTone::Idle);
}

#[test]
fn import_card_status_covers_running_remaining_failed_partial_and_success() {
    let mut progress = progress_fixture(42, 100);
    progress.imported = 40;
    progress.failed = 1;
    let running = import_card_status(Some(&progress), None);
    assert_eq!(running.headline, "取り込み中");
    assert_eq!(running.detail, "42/100 · 取込 40 · 失敗 1");
    assert_eq!(running.tone, StatusTone::Running);

    let pending = ImportReportView {
        total: 100,
        processed: 40,
        imported: 40,
        issues: vec![non_retryable_issue("/bad.png")],
        remaining: vec![PathBuf::from("/next.png")],
        ..import_report_fixture()
    };
    let pending = import_card_status(None, Some(&pending));
    assert_eq!(pending.headline, "未処理 1");
    assert_eq!(pending.detail, "取込 40/100 · 失敗 1");
    assert_eq!(pending.tone, StatusTone::Waiting);

    let failed = ImportReportView {
        total: 10,
        processed: 10,
        imported: 8,
        issues: vec![non_retryable_issue("/a.png"), non_retryable_issue("/b.png")],
        ..import_report_fixture()
    };
    let failed = import_card_status(None, Some(&failed));
    assert_eq!(failed.headline, "失敗 2");
    assert_eq!(failed.detail, "取込 8/10");
    assert_eq!(failed.tone, StatusTone::Failed);

    let partial = ImportReportView {
        total: 10,
        processed: 10,
        imported: 9,
        issues: vec![partial_issue("/c.png")],
        ..import_report_fixture()
    };
    let partial = import_card_status(None, Some(&partial));
    assert_eq!(partial.headline, "部分移動 1");
    assert_eq!(partial.detail, "取込 9/10 · 完了");
    assert_eq!(partial.tone, StatusTone::Waiting);

    let success = ImportReportView {
        total: 10,
        processed: 10,
        imported: 10,
        renamed: vec![rename_fixture("/a.png", "/dest/a-1.png")],
        ..import_report_fixture()
    };
    let success = import_card_status(None, Some(&success));
    assert_eq!(success.headline, "完了 10 枚");
    assert_eq!(success.detail, "前回の取り込み · 連番 1");
    assert_eq!(success.tone, StatusTone::Success);

    let none = import_card_status(None, None);
    assert_eq!(none.headline, "履歴なし");
    assert_eq!(none.detail, "取り込み結果がここに残ります");
    assert_eq!(none.tone, StatusTone::Idle);
}

#[test]
fn scan_card_status_handles_percent_walking_paused_and_stopped() {
    let indexing = ScanProgressView {
        phase: IpcScanPhase::Indexing,
        processed: 50,
        total: Some(100),
        indexed: 49,
        root: 2,
        roots: 3,
    };
    let indexing = scan_card_status(Some(&indexing), true, false);
    assert_eq!(indexing.headline, "50%");
    assert_eq!(indexing.detail, "フォルダ 2/3 · 索引 50/100");
    assert_eq!(indexing.tone, StatusTone::Running);

    let walking = ScanProgressView {
        phase: IpcScanPhase::Walking,
        processed: 12,
        total: None,
        indexed: 0,
        root: 1,
        roots: 2,
    };
    let walking = scan_card_status(Some(&walking), true, false);
    assert_eq!(walking.headline, "走査中…");
    assert_eq!(walking.detail, "フォルダ 1/2 · 検索 12 件");
    assert_eq!(walking.tone, StatusTone::Running);

    let unknown_total = ScanProgressView {
        total: None,
        ..indexing_fixture()
    };
    assert_eq!(
        scan_card_status(Some(&unknown_total), true, false).headline,
        "走査中…"
    );

    let zero_total = ScanProgressView {
        total: Some(0),
        ..indexing_fixture()
    };
    assert_eq!(
        scan_card_status(Some(&zero_total), true, false).headline,
        "走査中…"
    );

    let paused = scan_card_status(None, true, true);
    assert_eq!(paused.headline, "一時停止中");
    assert_eq!(paused.detail, "監視を再開すると走査します");
    assert_eq!(paused.tone, StatusTone::Waiting);

    let stopped = scan_card_status(None, false, false);
    assert_eq!(stopped.headline, "停止中");
    assert_eq!(stopped.detail, "デーモンは接続されていません");
    assert_eq!(stopped.tone, StatusTone::Idle);

    let idle = scan_card_status(None, true, false);
    assert_eq!(idle.headline, "待機中");
    assert_eq!(idle.detail, "変更を監視しています");
    assert_eq!(idle.tone, StatusTone::Idle);
}

#[test]
fn attention_dots_prioritize_failures_over_running_and_waiting() {
    assert_eq!(
        compression_attention(6, 5, 2),
        Some(StatusTone::Failed)
    );
    assert_eq!(
        compression_attention(4, 1, 0),
        Some(StatusTone::Running)
    );
    assert_eq!(
        compression_attention(2, 2, 0),
        Some(StatusTone::Waiting)
    );
    assert_eq!(compression_attention(0, 0, 0), None);

    let progress = progress_fixture(1, 10);
    assert_eq!(import_attention(Some(&progress), None), Some(StatusTone::Running));

    let failed = ImportReportView {
        total: 2,
        processed: 2,
        imported: 1,
        issues: vec![non_retryable_issue("/a.png")],
        remaining: vec![PathBuf::from("/b.png")],
        ..import_report_fixture()
    };
    assert_eq!(import_attention(None, Some(&failed)), Some(StatusTone::Failed));

    let pending = ImportReportView {
        remaining: vec![PathBuf::from("/b.png")],
        ..import_report_fixture()
    };
    assert_eq!(import_attention(None, Some(&pending)), Some(StatusTone::Waiting));

    assert_eq!(import_attention(None, None), None);

    let scanning = ScanProgressView {
        phase: IpcScanPhase::Indexing,
        processed: 1,
        total: Some(10),
        indexed: 1,
        root: 1,
        roots: 1,
    };
    assert_eq!(scan_attention(Some(&scanning), true, false), Some(StatusTone::Running));
    assert_eq!(scan_attention(None, true, true), Some(StatusTone::Waiting));
    assert_eq!(scan_attention(None, true, false), None);
    assert_eq!(scan_attention(None, false, false), None);
}

#[test]
fn import_running_progress_only_reads_the_running_phase() {
    assert!(import_running_progress(None).is_none());

    let ready = import_dialog_fixture();
    assert!(import_running_progress(Some(&ready)).is_none());

    let mut running = import_dialog_fixture();
    running.phase = ImportDialogPhase::Running {
        progress: progress_fixture(1, 2),
        cancel: Arc::new(AtomicBool::new(false)),
        cancel_requested: false,
    };
    assert_eq!(
        import_running_progress(Some(&running)).map(|progress| progress.processed),
        Some(1)
    );
}

#[test]
fn tone_colors_use_the_sidebar_palette() {
    assert_eq!(StatusTone::Idle.color(), MUTED_2);
    assert_eq!(StatusTone::Running.color(), BLUE);
    assert_eq!(StatusTone::Waiting.color(), AMBER);
    assert_eq!(StatusTone::Failed.color(), RED);
    assert_eq!(StatusTone::Success.color(), GREEN);
}
```

Add this small fixture next to the existing scan progress test or near the new tests:

```rust
fn indexing_fixture() -> ScanProgressView {
    ScanProgressView {
        phase: IpcScanPhase::Indexing,
        processed: 12,
        total: Some(20),
        indexed: 12,
        root: 1,
        roots: 1,
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui running_job_count_clamps -- --nocapture`

Expected: compile errors for the missing types and functions.

- [ ] **Step 3: Implement the status types and helpers**

Add the types after `ManualCompressionDialog`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarStatusTab {
    Compression,
    Import,
    Scan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusTone {
    Idle,
    Running,
    Waiting,
    Failed,
    Success,
}

impl StatusTone {
    fn color(self) -> u32 {
        match self {
            Self::Idle => MUTED_2,
            Self::Running => BLUE,
            Self::Waiting => AMBER,
            Self::Failed => RED,
            Self::Success => GREEN,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CardStatus {
    headline: String,
    detail: String,
    tone: StatusTone,
}
```

Implement the helpers after `scan_progress_ratio`:

- `running_job_count` returns `(active - waiting).max(0)`.
- `compression_card_status` priority: waiting > running > failed > idle. Headlines and details must match the tests exactly. The running count comes from `running_job_count`.
- `import_card_status` priority: running progress > remaining > failed > partial move > success > no report. Use `import_issue_counts` for failed/partial.
- `scan_card_status`: indexing with a positive `total` uses the rounded percent and `scan_progress_label`; walking or indexing without a positive total uses `走査中…`; then paused, stopped, idle.
- `compression_attention`: failed > running > waiting > `None`.
- `import_attention`: running > failed > remaining/partial > `None`.
- `scan_attention`: running > paused > `None`.
- `import_running_progress`: return `Some(progress)` only for `ImportDialogPhase::Running`; otherwise `None`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-ui running_job_count_clamps compression_card_status import_card_status scan_card_status attention_dots import_running_progress tone_colors`

Expected: all new tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat(ui): model sidebar progress card status"
```

---

### Task 2: Render the card and wire actions

**Files:**
- Modify: `crates/ui/src/main.rs`
  - `App` field and constructor.
  - `sidebar` method and a new `sidebar_status_card` method.
  - A small `status_progress_bar` helper.

**Interfaces:**
- Consumes: all Task 1 types and helpers; existing `request_release_waiting_jobs`, `retry_all_failed_jobs`, `request_daemon_rescan`, `switch_screen`, `self.active_job_count`, `self.waiting_job_count`, `self.failed_job_count`, `self.import_dialog`, `self.import_report`, `self.scan_progress`, `self.daemon_active`, `self.daemon_paused`.
- Produces: `fn sidebar_status_card(&self, cx: &mut Context<Self>) -> AnyElement`.

- [ ] **Step 1: Add the tab state**

Add to `App` after `sidebar_collapsed`:

```rust
sidebar_status_tab: SidebarStatusTab,
```

Initialize it after `sidebar_collapsed: initial_settings.ui_sidebar_collapsed,`:

```rust
sidebar_status_tab: SidebarStatusTab::Compression,
```

- [ ] **Step 2: Add the card renderer**

Add `fn sidebar_status_card(&self, cx: &mut Context<Self>) -> AnyElement` after the `sidebar` method and before `nav_highlight`.

Use this structure (exact spacing and colors):

- Card: `v_flex().flex_shrink_0().p(px(9.)).gap(px(4.)).rounded(px(13.)).bg(rgb(PANEL)).border_1().border_color(rgba(0xffffff0f))`.
- Read `let import_progress = import_running_progress(self.import_dialog.as_ref());`, then build `compression`, `import`, and `scan` `CardStatus` values and their attention tones before rendering.
- Header: `h_flex().items_center().h(px(12.))`, title `進行状況` at 10 px `MUTED_2` semibold, a spacer, then a 6 px `rounded_full` dot in the selected status's tone color.
- Tabs: `h_flex().gap(px(4.))` with three `flex_1` tabs, each 20 px high, 6 px radius, 10 px medium text. Active tab: `ACCENT_SOFT` background, `BLUE` text, transparent 1 px border replaced by `rgba(0x66b3ff33)`. Inactive: `MUTED_2` text, no fill, hover `PANEL_2` and `TEXT`. Give each tab a unique id (`sidebar-status-tab-compression`, `sidebar-status-tab-import`, `sidebar-status-tab-scan`) and `.cursor_pointer()`; clicking sets `self.sidebar_status_tab = tab` and notifies. If `attention` is `Some(tone)`, render a 4 px absolute dot at top 3 px / right 4 px in `tone.color()`.
- Body: `v_flex().h(px(36.)).justify_center().gap(px(2.))`. Headline is 13 px semibold in `status.tone.color()`; detail is 10 px `MUTED` with `.truncate()`. When the selected view has a ratio, append a 2 px progress bar: full-width `PANEL_2` track with a `BLUE` fill at `relative(ratio)`. Import running uses `import_progress_ratio`; scan uses `scan_progress_ratio` only when greater than zero; compression has no bar.
- Footer: `h_flex().items_center().h(px(24.)).gap(px(6.))`.
  - Compression: if `waiting_job_count > 0`, primary `未実行を開始` (id `sidebar-status-release-waiting`) calling `request_release_waiting_jobs(window, cx)`; if `failed_job_count > 0`, ghost `失敗を再試行` (id `sidebar-status-retry-failed`) calling `retry_all_failed_jobs(cx)`; if neither, ghost `キューを開く` (id `sidebar-status-open-queue`) switching to `Screen::Dashboard`.
  - Import: ghost `結果を見る` (id `sidebar-status-import-result`) when `import_report.is_some()`, otherwise `写真を開く` (id `sidebar-status-photos`); both switch to `Screen::Photos`.
  - Scan: ghost `再スキャン` (id `sidebar-status-rescan`) when there is no `scan_progress`, the daemon is active, and it is not paused; otherwise `ダッシュボード` (id `sidebar-status-dashboard`) switching to `Screen::Dashboard`. Rescan calls `request_daemon_rescan(cx)`.
  - All buttons use `.small()`; primary or ghost exactly as stated. Add tooltips only where the label alone is ambiguous (`未実行を開始`, `失敗を再試行`, `再スキャン`).

Add this helper near the status helpers:

```rust
fn status_progress_bar(ratio: f32) -> Div {
    div()
        .h(px(2.))
        .w_full()
        .rounded_full()
        .bg(rgb(PANEL_2))
        .child(
            div()
                .h_full()
                .w(relative(ratio.clamp(0.0, 1.0)))
                .rounded_full()
                .bg(rgb(BLUE)),
        )
}
```

- [ ] **Step 3: Insert the card above the library card**

In `sidebar`, before building `let body = ...`, add:

```rust
let status_card = (!collapsed).then(|| self.sidebar_status_card(cx));
```

Then, after `.child(div().flex_1())` and before the existing `.when(!collapsed, ...)` library card block, add:

```rust
.when_some(status_card, |this, card| this.child(card))
```

The existing library card block stays unchanged. Do not render the card when collapsed.

- [ ] **Step 4: Run formatting, clippy, and tests**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

Expected: all pass. If `fmt --check` fails, run `cargo fmt --all` and re-run the check.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat(ui): add sidebar progress card"
```

---

### Task 3: Visual verification and polish

**Files:**
- Modify only if the screenshot check finds a layout defect: `crates/ui/src/main.rs`.

- [ ] **Step 1: Build and launch the demo**

Run: `cargo run -p vrc-photo-ui --bin vrc-photo-manager -- --demo`

Use the visual QA skill available in the session to capture the window. If the environment is WSL, use `wsl-gpui-testing`; otherwise use `hyprland-visual-qa`.

- [ ] **Step 2: Seed waiting jobs for the action check**

Close the app, then insert three waiting jobs into the demo DB. The demo DB is `$TMPDIR/vrc-photo-manager-demo/library.db` (fall back to `/tmp/vrc-photo-manager-demo/library.db`):

```bash
sqlite3 /tmp/vrc-photo-manager-demo/library.db <<'SQL'
INSERT INTO compression_jobs(id, photo_id, profile_id, status, progress, policy, created_at)
SELECT 'qa-wait-' || p.id, p.id,
       (SELECT id FROM compression_profiles LIMIT 1),
       'waiting', 0, 'keep', datetime('now')
FROM photos AS p
LIMIT 3;
SQL
```

Relaunch the demo. The compression tab must show `未実行 3` and the `未実行を開始` button.

- [ ] **Step 3: Verify the interactive cases**

- Click the three tabs. The headline, detail, progress bar, and footer change without moving the library card.
- Click `未実行を開始`. With the default keep policy it runs immediately; with the replace policy it opens the existing danger confirmation. After the click the waiting count disappears and the pending/running count appears.
- Import two sample images through the Photos header. After completion, the import tab shows `完了 2 枚` and `結果を見る` opens Photos with the report panel.
- Press `Ctrl+B`. The whole progress card disappears together with the library card; expand again and confirm the selected tab is unchanged.
- Wait for at least one 3-second refresh while the import or scan tab is selected. The selected tab must not reset.
- Resize to the minimum window (1080×700) and confirm the version row and library card are still visible.

- [ ] **Step 4: Check the screenshots**

Capture at least:

- compression tab with waiting jobs,
- import tab after a completed import,
- scan tab in the demo (daemon stopped is acceptable; the running bar is covered by Task 1 tests),
- collapsed sidebar.

If text clips, the card height jumps, or the buttons wrap awkwardly, adjust only the card's padding/gap/font sizes in `sidebar_status_card`, then re-run Task 2 verification and re-capture.

- [ ] **Step 5: Final verification and commit any polish**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

Commit only if Step 4 changed code:

```bash
git add crates/ui/src/main.rs
git commit -m "fix(ui): polish sidebar progress card layout"
```

---

### After the code PR

- Open the code PR against `main` with `Refs` to the design discussion; do not include docs.
- Watch `CI` and the PR-time `Release` bundle with the `ci-watch` skill; merge only when both are green.
- Follow up with a separate docs-only PR updating `docs/ui.md` 2.1 and the compression section of `docs/features.md`.
