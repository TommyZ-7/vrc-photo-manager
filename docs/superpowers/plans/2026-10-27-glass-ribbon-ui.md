# ガラス調リボンUI + UIモジュール分割 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `crates/ui/src/main.rs`（14,833行・134テスト）を責務単位へ分割し、その上でガラス調シェル、カスタムタイトルバー、3タブのコンパクトリボン、見出しのないカード主体の画面へ刷新する。

**Architecture:** 2段階。PR1は**見た目を変えない**モジュール分割（`App`、状態、テーマ、共通部品、画面、ビューア、ダイアログ、CLI/demoを分離）。PR2/PR3で `TitleBar` とウィンドウ背景を使ったシェル、リボン、画面再配置を実装する。データ/DB/IPC/画像処理の意味は変えない。

**Tech Stack:** Rust 2024, `gpui-kit = "=0.8.0"`（`gpui-pre 0.3.9` / `gpui-component 0.8.0`）, SQLite via `vrc-photo-core`, `image` 0.25.

**Spec:** `docs/superpowers/specs/2026-10-27-glass-ribbon-ui-design.md`

**Design mock:** `docs/ui-glass-ribbon-mockup.html`  
**Baseline commit:** `1f62ec9`（この計画作成時点の `docs/glass-ui-mock`）。行番号は移動前のアンカーであり、分割後は名前で探す。

## Global Constraints

- `gpui-kit = "=0.8.0"` を固定。`gpui` を直接依存に追加しない。依存追加なし。
- UIのDBスキーマ・IPCプロトコル・画像デコード経路を増やさない。
- 新しい設定は `ui_ribbon_collapsed: bool` の1つだけ。`#[serde(default)]` を付け、既存JSONを壊さない。
- ライブラリ内インライン検索、リスト表示、実フィルタリング、新しいソート、ガラス強度設定、年レール表示設定は**今回の対象外**。モックとの差分は設計書 §6.1 に従う。
- PR1（分割）では見た目・文言・余白・アニメーション・フォーカス順を変えない。既存134テストを減らさない。
- PR1とUI変更を同じPRに混ぜない。`docs/ui.md` / `docs/architecture.md` の更新は最後のdocs-only PRに分ける。
- すべての作業は `main` から切ったworktreeで行う。`main` へ直接pushしない。
- 画像デコード/DBクエリをアニメーションフレームやスクロールフレームで増やさない。
- 各PRの完了条件: `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`。
- Windows実機の表示確認は #9 に委譲する。Linux（X11/Wayland）で1280×820と1440×900を確認する。

## Review Focus

1. **分割の無変化性**: PR1で画面・文言・余白・動作が変わらない。134テストが減らず、`--demo` の1280/1440スクリーンショットが分割前と一致する。Task 1, 12。
2. **タイトルバーのフォールバック**: Linuxでクライアント装飾が拒否されてもネイティブタイトルバーとアプリ内タブが二重破綻しない。タブクリック・ドラッグ・ダブルクリック最大化・ウィンドウ操作が動く。Task 13。
3. **ブラー非対応環境**: `Blurred` が効かなくてもアンビエント＋半透明カードで読める。壁紙が見えることを前提にしない。Task 13。
4. **見出し削除で失う情報**: グローバルアラート、`message`、取り込み結果、複数選択バー、ライブラリ索引エラー、再試行導線が消えない。Task 15。
5. **リボンの状態**: 画面切替と手動タブ選択、折りたたみ、写真画面だけの詳細コマンド、設定コマンドの排除が一貫する。Task 14。
6. **設定JSON互換**: `ui_ribbon_collapsed` 追加後も、既存JSONの他設定がリセットされない。Task 14。
7. **カード寸法と年レール**: 3:2カードの列幅→行高→`uniform_list`→年レールジャンプの計算が同じ helper を使い、スクロール/アンカー/リサイズで破綻しない。Task 16。
8. **レスポンシブ/フォーカス**: 1280/1440/1080で横スクロール・カード欠け・文字切れがない。ビューア/モーダルのフォーカスとEscが退行しない。Task 17, 18。

---

# PR1 — behavior-preserving module split

Branch: `refactor/ui-module-split`（updated `main` からworktree）。

## Task 1: Baseline, module skeleton, App/Screen/CLI move

**Files:**
- Create: `crates/ui/src/app.rs`, `crates/ui/src/app/cli.rs`
- Create: `crates/ui/src/state/mod.rs`, `crates/ui/src/ui/mod.rs`
- Modify: `crates/ui/src/main.rs`
- Test: `crates/ui/src/app/cli.rs` の `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 既存の `Screen`, `LaunchOptions`, `App`、`parse_args`, `parse_window_size`, timing helpers。
- Produces:
  - `crate::app::App`（struct。画面モジュールから参照するフィールドは `pub(crate)`）
  - `crate::app::Screen`（`title()`, `Default` 含む）
  - `crate::app::LaunchOptions`
  - `crate::app::cli::{parse_args, parse_window_size, timing_enabled, log_timing, process_start}`
  - `crates/ui/src/state/`, `crates/ui/src/ui/` の空モジュール

- [ ] **Step 1: Capture the current UI**

Run the demo and save baseline screenshots outside the repo:

```bash
cargo run -p vrc-photo-ui -- --demo --window-size 1280x820
cargo run -p vrc-photo-ui -- --demo --window-size 1440x900
```

Use the repository's `gnome-wayland-visual-qa` skill or the existing screenshot method. Save as `/tmp/glass-ribbon-baseline/library-1280.png` and `library-1440.png`. Also record the current test count:

```bash
grep -c '^    #\[test\]' crates/ui/src/main.rs
wc -l crates/ui/src/main.rs
```

Expected: 134 tests, 14,833 lines (or record the actual numbers).

- [ ] **Step 2: Move `Screen`, `App`, and `LaunchOptions`**

Move the `Screen`, `App` struct, `LaunchOptions`, and timing helpers into `app.rs` / `app/cli.rs`. Mark `App` fields `pub(crate)` and keep the `impl App` blocks in `main.rs` for now. Add `mod app; mod state; mod ui;` to `main.rs` and import the moved names.

`app.rs` starts as:

```rust
pub(crate) mod cli;

pub(crate) use cli::LaunchOptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Screen { Photos, Compression, Search, Dashboard, Settings }

impl Screen {
    pub(crate) fn title(self) -> &'static str { /* move existing body */ }
}

pub struct App {
    pub(crate) db: /* existing type */,
    // all existing fields, now pub(crate)
}
```

Do not change any value or method body. Do not make the fields `pub` outside the crate.

- [ ] **Step 3: Add empty module roots**

Create `state/mod.rs` and `ui/mod.rs` with no items yet. `main.rs` declares them. This gives later tasks a stable home.

- [ ] **Step 4: Run the existing tests and build**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
cargo check -p vrc-photo-ui
```

Expected: all existing UI tests pass; no new warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src
git commit -m "refactor(ui): add app module skeleton"
```

## Task 2: Extract theme, metrics, and pure helpers

**Files:**
- Create: `crates/ui/src/ui/theme.rs`
- Modify: `crates/ui/src/main.rs`, `crates/ui/src/app.rs`, `crates/ui/src/ui/mod.rs`
- Test: `crates/ui/src/ui/theme.rs` の `#[cfg(test)] mod tests`

**Interfaces:**
- Produces `pub(crate)` constants and helpers:
  - `BG, PANEL, PANEL_2, SIDEBAR, BORDER, BORDER_STRONG, TEXT, MUTED, MUTED_2, BLUE, ACCENT_SOFT, GREEN, AMBER, RED, PURPLE`
  - metrics: `SIDEBAR_EXPANDED, SIDEBAR_COLLAPSED, MIN_CARD_WIDTH, GRID_GAP, INSPECTOR_WIDTH, INSPECTOR_CARD_WIDTH, INSPECTOR_MARGIN, VIEWER_DETAILS_WIDTH, VIEWER_TOP_BAR, VIEWER_PAD, WORKSPACE_PAD_X`
  - motion: `MOTION_FAST, MOTION_BASE, MOTION_SLOW, MOTION_MORPH, MOTION_FPS, SCREEN_SLIDE, VIEWER_NAV_SLIDE`
  - helpers: `format_bytes`, `format_count`, `short_date`, `card_meta_line`, `card_size_options`, `card_size_label`, `scene_color`, `format_label`, `compression_policy`, `lossless_supported`, `lossless_hint`, `profile_summary`, `apply_profile_format`, `inner_corner_radius`, `sidebar_width`, `sidebar_reveal_width`, `grid_available_width`, `search_available_width`, `grid_columns`, `grid_columns_for`, `viewer_image_box_with_details_width`, `viewer_image_box`, `centered_viewer_rect`, `average_bgra`, `lerp_color`, `cubic_bezier`, `cubic_bezier_slope`, `ease_motion`, `eased_progress`, `viewer_nav_progress`, `stagger_value`, `next_focus_index`, `step_index`, `selection_range`, `toggle_selected_id`, `moved_profile_order`
- Consumes: none outside std/gpui types.

- [ ] **Step 1: Move the token and pure helper blocks**

Move the named items out of `main.rs` without changing their bodies. Add a module-level `use crate::ui::theme::*;` where needed. Keep the old constants in place only until all references compile; delete duplicates in the same commit.

- [ ] **Step 2: Move the matching tests**

Move tests such as `sidebar_widths_match_the_rail`, `inspector_card_and_margin_fill_the_column`, `grid_columns_adapt_to_window_rail_and_inspector`, `viewer_image_box_fits_inside_padding_and_details`, `compact_count_uses_k_for_large_libraries`, `average_bgra_ignores_transparent_pixels`, `lerp_color_hits_both_ends_and_midpoint`, `ease_motion_is_monotonic_and_clamped`, `lerp_bounds_hits_endpoints_and_midpoint`, `motion_durations_are_ordered`, `stagger_value_waits_for_the_card_delay`, `focus_cycle_wraps_in_both_directions`, `step_index`/selection/profile order tests into `ui/theme.rs`. Move CLI/parser tests into `app/cli.rs` instead.

- [ ] **Step 3: Verify**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
cargo clippy -p vrc-photo-ui --all-targets -- -D warnings
```

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src
git commit -m "refactor(ui): extract theme and pure layout helpers"
```

## Task 3: Extract preview and thumbnail state

**Files:**
- Create: `crates/ui/src/state/preview.rs`, `crates/ui/src/state/thumbnail.rs`
- Modify: `crates/ui/src/state/mod.rs`, `crates/ui/src/main.rs`
- Test: 各モジュールの `#[cfg(test)] mod tests`

**Interfaces:**
- `state::preview` produces `PreviewKey`, `PreviewQueue`, `preview_key`, `should_request_preview`, `preview_cache_dir`, `preview_placeholder_label`.
- `state::thumbnail` produces `ThumbnailCache`, `decode_thumbnail`, `make_render_image`, `preview_render_image`.
- Both modules expose the fields/methods needed by `App` as `pub(crate)`.

- [ ] **Step 1: Move preview state and its tests**

Move the `PreviewKey`/`PreviewQueue` block, preview path helpers, and tests `preview_queue_runs_one_decode_and_keeps_the_latest_wanted`, `preview_queue_re_requesting_the_in_flight_key_drops_the_queued_key`, `preview_queue_pending_for_covers_queued_and_in_flight`, `preview_key_uses_the_display_derivative`, `stale_refresh_requests_a_new_variant_once`, `preview_cache_dir_never_uses_the_real_cache_in_demo`, `preview_placeholder_label_distinguishes_loading_from_failure`.

- [ ] **Step 2: Move thumbnail state and its tests**

Move `ThumbnailCache`, `decode_thumbnail`, and the image conversion helpers. Move tests `thumbnail_cache_limits_in_flight_requests`, `thumbnail_cache_evicts_least_recently_used_but_keeps_pinned`, `thumbnail_cache_unpins_a_deleted_selection`, `thumbnail_cache_frees_a_slot_for_a_pinned_retry`, `thumbnail_cache_does_not_retry_failed_or_pending_ids`, `decode_thumbnail_uses_the_cached_file_and_swaps_channels`, `test_render_image`.

- [ ] **Step 3: Update imports and verify**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
```

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src
git commit -m "refactor(ui): extract preview and thumbnail state"
```

## Task 4: Extract photo browser state

**Files:**
- Create: `crates/ui/src/state/photo_browser.rs`
- Modify: `crates/ui/src/state/mod.rs`, `crates/ui/src/main.rs`
- Test: モジュール内 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces `PhotoIndex`, `PhotoPageCache`, `PhotoBrowser`, `CountDebounce`, `anchor_photo_id`, `search_anchor_after_result`, `sync_search_mirror`, `apply_library_index`, `first_visible_photo`, `list_scroll_top`, `PAGE_SIZE`, `MAX_PAGES`.
- `PhotoIndex.entries`, `PhotoIndex.total`, `PhotoBrowser.index` 等、`App`/画面が必要とするフィールドは `pub(crate)`。

- [ ] **Step 1: Move the state types and functions**

Move the named items as a unit. Keep the `uniform_list` page-loading logic in the UI task; only the state/query/cache side moves here.

- [ ] **Step 2: Move the tests**

Move `page_cache_evicts_the_oldest_page`, `page_cache_keeps_recently_touched_pages`, `page_cache_plans_only_missing_pages_and_marks_them_pending`, `count_debounce_waits_for_a_quiet_period`, `count_debounce_restarts_while_imports_keep_arriving`, `stale_generation_cannot_insert_or_fail_pages`, `failed_library_index_keeps_the_mounted_snapshot_and_pages`, `fresh_library_index_replaces_the_snapshot`, `search_mirror_refreshes_only_when_the_library_snapshot_changed`, `anchor_photo_id_uses_the_first_visible_row`.

- [ ] **Step 3: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
git add crates/ui/src
git commit -m "refactor(ui): extract photo browser state"
```

## Task 5: Extract year-rail math and viewer state

**Files:**
- Create: `crates/ui/src/state/year_rail.rs`, `crates/ui/src/state/viewer.rs`
- Modify: `crates/ui/src/state/mod.rs`, `crates/ui/src/main.rs`
- Test: 各モジュール内 tests

**Interfaces:**
- `state::year_rail`: `YearBucket`, `build_year_buckets`, `rail_y`, `rail_row_at`, `format_captured_month`, `for_each_visible_year`, `visible_year_buckets`, `rail_label_hidden_by_pill`, `rail_track_hit_bounds`, `rail_pill_bounds`, `rail_marker_bounds`, `rail_press_hits`, `rail_press_allowed`, `rail_hover_hits`, `rail_text_run`, `rail_scroll_offset`.
- `state::viewer`: `ViewerOpenSource`, `MorphImageKind`, `morph_image_plan`, `ViewerMorph`, `ViewerBoxState`, `ViewerPanelState`, `current_tagged_bounds`, `preferred_bounds`, `morph_bounds_at`.
- `ui/year_rail.rs`（GPUI要素/Labels）はTask 7で移す。

- [ ] **Step 1: Move pure year-rail math and tests**

Move `YearBucket`/rail constants and functions. Move tests `rail_*` and `format_captured_month`-related tests.

- [ ] **Step 2: Move viewer state and tests**

Move the viewer types/functions. Move `morph_bounds_at_hits_both_ends`, `viewer_morph_current_bounds_follows_the_timeline`, `viewer_nav_progress_respects_reduce_motion`, `viewer_box_state_interpolates_from_the_visible_size`, `viewer_box_state_hits_both_details_ends`, `viewer_panel_state_retargets_from_the_current_width`, `morph_image_plan_covers_card_and_inspector_paths`, `current_tagged_bounds_rejects_stale_frames`, `preferred_bounds_falls_back_to_the_other_source`, `closing_prefers_the_original_source`.

- [ ] **Step 3: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
git add crates/ui/src
git commit -m "refactor(ui): extract year rail and viewer state"
```

## Task 6: Extract shared UI components

**Files:**
- Create: `crates/ui/src/ui/components.rs`
- Modify: `crates/ui/src/ui/mod.rs`, `crates/ui/src/main.rs`
- Test: コンポーネント内の純粋helper tests

**Interfaces:**
- Produces `ButtonTone`, `ButtonState`, `glass_panel`, `chip`, `focusable_text_button`, `base_text_button`, `tag_chip`, `preset_button`, `filter_field`, `setting_switch`, `card_size_button`, `manual_option_row`, `status_progress_bar`, `stat_card`, `meta_row`, `section_label`, `badge`.
- `glass_panel` remains a neutral helper in PR1; PR2 changes its fill.

- [ ] **Step 1: Move button/card/chip components**

Move the named functions and `ButtonTone`/`ButtonState`. Do not rewrite the existing layout yet.

- [ ] **Step 2: Move simple helper tests**

Move tests whose subject is a component metric (`inner_corner_radius`, `card_size_options` labels, etc.) to the owning module.

- [ ] **Step 3: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
git add crates/ui/src
git commit -m "refactor(ui): extract shared UI components"
```

## Task 7: Extract library screen and year-rail widget

**Files:**
- Create: `crates/ui/src/ui/library.rs`, `crates/ui/src/ui/year_rail.rs`, `crates/ui/src/app/photos.rs`
- Modify: `crates/ui/src/ui/mod.rs`, `crates/ui/src/app.rs`, `crates/ui/src/main.rs`
- Test: 既存のUIテスト / 手動demo

**Interfaces:**
- `ui::library` produces `photos_screen`, `selection_bar`, `photos_header`（PR1では残す）, `photo_grid`, `load_pages`, `photo_card`, `inspector_column`, `inspector`, `detail_body`.
- `ui::year_rail` produces `YearRailLabels`, `YearRail`, and their `Element`/`IntoElement` implementations.
- Move the `App` methods used only by the library (`browser_mut`, `select_photo`, range selection, preview request/pump, etc.) into `app/photos.rs` in this task; mark cross-module methods `pub(crate)`.

- [ ] **Step 1: Move the render functions unchanged**

Move the library UI functions. Keep `photos_header` and `screen_header` exactly as-is in PR1; they disappear in PR2.

- [ ] **Step 2: Move the year-rail element**

Move the GPUI year-rail element and labels into `ui/year_rail.rs`; keep the pure math in `state/year_rail.rs`. Update imports so the visual output is identical.

- [ ] **Step 3: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
cargo clippy -p vrc-photo-ui --all-targets -- -D warnings
git add crates/ui/src
git commit -m "refactor(ui): extract library screen"
```

## Task 8: Extract search and compression

**Files:**
- Create: `crates/ui/src/ui/search.rs`, `crates/ui/src/ui/compression.rs`
- Create: `crates/ui/src/app/search.rs`, `crates/ui/src/app/compression.rs`
- Modify: `crates/ui/src/ui/mod.rs`, `crates/ui/src/app.rs`, `crates/ui/src/main.rs`

**Interfaces:**
- `ui::search`: `search_screen`, `search_filter_panel`.
- `ui::compression`: `compression_screen`, `profile_editor`, `queue_panel`, profile list/editor helpers.
- `app::search`: `run_search`, `start_search`, `apply_search_result`, `clear_search`, `start_search_refresh`, `refresh_search_after_tag_edit`.
- `app::compression`: `edit_profile`, `move_profile`, `duplicate_profile`, `request_delete_profile`, `delete_profile`, `open_manual_compression`, `close_manual_compression`, `choose_manual_folder`, `confirm_manual_compression`.

- [ ] **Step 1: Move search UI + state methods**

Keep the existing filter panel, results header, selection bar, and grid calls unchanged.

- [ ] **Step 2: Move compression UI + profile/manual-compression methods**

Keep the existing profile order, delete confirmation, output policy, and queue behavior unchanged.

- [ ] **Step 3: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
cargo clippy -p vrc-photo-ui --all-targets -- -D warnings
git add crates/ui/src
git commit -m "refactor(ui): extract search and compression screens"
```

## Task 9: Extract dashboard and settings

**Files:**
- Create: `crates/ui/src/ui/dashboard.rs`, `crates/ui/src/ui/settings.rs`
- Create: `crates/ui/src/app/daemon.rs`
- Modify: `crates/ui/src/ui/mod.rs`, `crates/ui/src/app.rs`, `crates/ui/src/main.rs`

**Interfaces:**
- `ui::dashboard`: `dashboard_screen`, `daemon_card`, `daemon_error_card`, `log_card`, `open_logs_folder`.
- `ui::settings`: `settings_screen`, `setting_switch`, `card_size_button`, settings-specific helpers.
- `app::daemon`: `request_daemon_rescan`, `toggle_daemon_pause`, `toggle_daemon_shutdown`, `request_daemon_command`, `send_wake_commands`, `send_wake_jobs`, `apply_daemon_status`, `refresh_monitor`.

- [ ] **Step 1: Move dashboard UI and daemon methods**

Keep the current card order and all alert/error behavior.

- [ ] **Step 2: Move settings UI and settings methods**

Keep every existing setting, save path, card size, motion, log, and update behavior.

- [ ] **Step 3: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
git add crates/ui/src
git commit -m "refactor(ui): extract dashboard and settings screens"
```

## Task 10: Extract import, viewer, and dialogs

**Files:**
- Create: `crates/ui/src/ui/import.rs`, `crates/ui/src/ui/viewer.rs`, `crates/ui/src/ui/dialogs.rs`
- Create: `crates/ui/src/app/import.rs`, `crates/ui/src/app/viewer.rs`
- Modify: `crates/ui/src/ui/mod.rs`, `crates/ui/src/app.rs`, `crates/ui/src/main.rs`

**Interfaces:**
- `ui::import`: `import_modal_overlay`, `import_report_panel`, import report helpers.
- `ui::viewer`: `quick_look`, `viewer_morph_overlay`, viewer focus helpers.
- `ui::dialogs`: `manual_compression_overlay` and confirmation overlays.
- `app::import`: import dialog/run/cancel/retry/resume methods.
- `app::viewer`: `open_quick_look`, `close_quick_look`, `step_viewer_photo`, `toggle_quick_look_details`, `quick_look_focus_order`, `cycle_quick_look_focus`, `guard_viewer_selection`.

- [ ] **Step 1: Move viewer UI/state methods**

Keep focus trap, Esc, arrows, morph overlay, and selection synchronization unchanged.

- [ ] **Step 2: Move import and dialogs**

Keep copy/move, retry/resume, progress, report panel, manual compression policy/output, and deletion confirmation unchanged.

- [ ] **Step 3: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
cargo clippy -p vrc-photo-ui --all-targets -- -D warnings
git add crates/ui/src
git commit -m "refactor(ui): extract import, viewer, and dialogs"
```

## Task 11: Move App lifecycle, demo, and main bootstrap

**Files:**
- Create: `crates/ui/src/app/lifecycle.rs`, `crates/ui/src/app/demo.rs`, `crates/ui/src/ui/actions.rs`
- Modify: `crates/ui/src/app.rs`, `crates/ui/src/app/cli.rs`, `crates/ui/src/main.rs`
- Test: remaining tests moved to owners

**Interfaces:**
- `app.rs`: `App` struct, `new`, `render`.
- `app::lifecycle`: `start_auto_refresh`, `start_ipc`, `handle_ipc_incoming`, `handle_ipc_event`, `refresh_live`, `reload_job_state`, `reload_aux`, `reload_library`, `reload_all`, `switch_screen`, `toggle_sidebar`, `toggle_inspector`, `focus_search`, `wake_daemon`, `send_ipc_request`, update timer methods, startup completion.
- `app::demo`: `DemoAssets`, `prepare_demo`, `demo_thumb_path`, `demo_image`, `seed_demo`, `open_database`.
- `ui::actions`: `actions!` declarations and the action types.
- `main.rs`: `mod` declarations, global action registration, `main`, `WindowOptions` construction, `gpui_kit::init`, `open_window`.

- [ ] **Step 1: Move App::new/render and lifecycle**

`App::new` moves to `app.rs` only after all field types are reachable. Keep behavior identical, including initial focus and demo/daemon/update startup.

- [ ] **Step 2: Move the remaining helpers and platform functions**

Distribute the remaining helper block from the old bottom of `main.rs` by owner: `daemon_liveness`/`daemon_state_label`/`ipc_status_label`/daemon spawn/supervisor → `app/daemon.rs` or `app/lifecycle.rs`; `import_*`/`scan_*`/`queue_status_badge`/`job_retry_*` → `app/import.rs`/`app/compression.rs`/`ui/components.rs`; `open_path`/`open_status` → `app/daemon.rs`; channel/status helpers → `ui/components.rs` or `app.rs` as appropriate. No helper function should remain in `main.rs` except bootstrap-specific code.

- [ ] **Step 3: Move demo/CLI and shrink main**

`main.rs` should contain no screen rendering and no UI helper functions. Target `<400` lines.

- [ ] **Step 4: Move remaining tests**

Move tests by subject so no giant test module remains in `main.rs`. Keep all 134 tests plus any new ones.

- [ ] **Step 5: Verify module sizes and tests**

```bash
wc -l crates/ui/src/main.rs crates/ui/src/app.rs crates/ui/src/app/*.rs crates/ui/src/state/*.rs crates/ui/src/ui/*.rs
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
cargo clippy -p vrc-photo-ui --all-targets -- -D warnings
```

Expected: `main.rs < 400` lines; no module over 2,000 lines; tests all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src
git commit -m "refactor(ui): move app lifecycle and bootstrap"
```

## Task 12: Verify behavior parity and open PR1

**Files:** none (verification only)

- [ ] **Step 1: Test count and full checks**

```bash
grep -R "#\[test\]" -n crates/ui/src | wc -l
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: test count >=134; all green.

- [ ] **Step 2: Visual parity**

Run `--demo --window-size 1280x820` and `1440x900`. Compare with `/tmp/glass-ribbon-baseline`. There must be no visual diff.

- [ ] **Step 3: Open PR**

```bash
gh pr create --base main --head refactor/ui-module-split \
  --title "refactor(ui): split the UI into modules" \
  --body "Behavior-preserving split of crates/ui/src/main.rs. No visual or behavior changes. Follows docs/superpowers/specs/2026-10-27-glass-ribbon-ui-design.md and docs/superpowers/plans/2026-10-27-glass-ribbon-ui.md."
```

Wait for CI green, then merge. This plan's PR1 ends here.

---

# PR2 — shell, titlebar, ribbon

Branch: `feat/ui-glass-ribbon`（PR1 merge後の `main` からworktree）。

## Task 13: Window options, ambient backdrop, glass theme

**Files:**
- Modify: `crates/ui/src/main.rs` (window options)
- Modify: `crates/ui/src/ui/theme.rs`, `crates/ui/src/ui/shell.rs`, `crates/ui/src/ui/components.rs`
- Test: `ui/theme.rs` helper tests; manual visual

**Interfaces:**
- Consumes: `gpui_kit::component::TitleBar`, `WindowOptions`, `WindowDecorations`, `WindowBackgroundAppearance`.
- Produces `ambient_background() -> impl IntoElement` and updated glass token constants.

- [ ] **Step 1: Set the window options**

Use `TitleBar::window_options()` as the base:

```rust
let window_options = WindowOptions {
    window_bounds: Some(WindowBounds::centered(size(px(width), px(height)), cx)),
    window_min_size: Some(size(px(1080.), px(700.))),
    window_decorations: Some(WindowDecorations::Client),
    window_background: WindowBackgroundAppearance::Blurred,
    ..TitleBar::window_options()
};
```

Keep `window.set_window_title("VRC Photo Manager")`. If `TitleBar` styling cannot reach 46px, record the fallback in the PR and keep the component's native height rather than drawing custom window controls.

- [ ] **Step 2: Add the ambient layer**

In `ui/shell.rs`, render an absolute background behind the cards: a dark linear gradient plus 2-3 large low-opacity glows using rounded divs/shadows. It must be readable without OS blur. Root fill should be translucent, not opaque `BG`.

- [ ] **Step 3: Update glass tokens and `glass_panel`**

Add the glass alpha colors from the design spec. Keep existing constant names; update `glass_panel` to use the new translucent fill/border/shadow. Do not add element-level blur.

- [ ] **Step 4: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
cargo run -p vrc-photo-ui -- --demo --window-size 1280x820
git add crates/ui/src
git commit -m "feat(ui): add glass window shell"
```

## Task 14: Titlebar tabs, compact ribbon, and persisted collapse

**Files:**
- Create: `crates/ui/src/ui/chrome.rs`
- Modify: `crates/ui/src/ui/shell.rs`, `crates/ui/src/app.rs`, `crates/ui/src/app/lifecycle.rs`, `crates/ui/src/ui/theme.rs`, `crates/core/src/settings.rs`
- Test: `ui/theme.rs`, `app.rs` pure helpers, `crates/core/src/settings.rs`

**Interfaces:**
- Produces:
  - `enum RibbonTab { Home, Photo, Compression }`
  - `fn default_ribbon_tab(screen: Screen) -> RibbonTab`
  - `fn ribbon_height(window_height: f32) -> f32` (68.0 or 62.0)
  - `App { active_ribbon_tab: RibbonTab, ribbon_collapsed: bool }`
  - `AppSettings.ui_ribbon_collapsed: bool` (`#[serde(default)]`, default false)
- Consumes: existing import/reload/selection/viewer/compression methods; `TitleBar`.

- [ ] **Step 1: Add settings with tests first**

In `crates/core/src/settings.rs`, add the field and default. Write tests that old JSON without the field loads with `false` and other fields intact, and that round-trip preserves `true`. Run `cargo test -p vrc-photo-core settings`.

- [ ] **Step 2: Add ribbon state helpers and tests**

Write failing tests for `default_ribbon_tab` (Compression -> Compression, others -> Home) and `ribbon_height` (760.0 => 62, 761.0 => 68, 700.0 => 62). Implement.

- [ ] **Step 3: Render the titlebar line**

In `ui/chrome.rs`, wrap `TitleBar` at 46px with app glyph/name, current screen name, three tabs, daemon/sync/status, collapse, notification, and the component's window controls. Use a spacer to push status to the right. Ensure the tabs are interactive and the rest is draggable.

- [ ] **Step 4: Render the ribbon body**

Implement the three command pages from design §10.2 with 44px icon+label buttons, 21px small buttons, 10px group labels, and 68/62px height. Omit hints and the list-view group. Collapse/expand persists via `ui_ribbon_collapsed`.

- [ ] **Step 5: Screen switching**

`switch_screen` sets the default tab; manual tab selection survives until the next screen switch. If collapsed, clicking a tab expands the ribbon.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo run -p vrc-photo-ui -- --demo --window-size 1280x820
git add crates/ui/src crates/core/src/settings.rs
git commit -m "feat(ui): add titlebar tabs and compact ribbon"
```

## Task 15: Remove page headers and rehome actions/messages/alerts

**Files:**
- Modify: all `crates/ui/src/ui/*.rs` screen files, `crates/ui/src/ui/shell.rs`, `crates/ui/src/app/lifecycle.rs`
- Test: existing UI tests + manual states

**Interfaces:**
- Consumes: `Screen::title`, `global_alerts`, `App.message`, existing screen actions.
- Produces: no `screen_header`; titlebar crumb; ribbon commands; alert strip; message slot.

- [ ] **Step 1: Delete `screen_header` and per-screen header calls**

Every screen starts directly with its inner cards. The current screen title is the titlebar crumb. Remove 72px headers from Photos/Search/Compression/Dashboard/Settings.

- [ ] **Step 2: Rehome library actions**

Move copy/move import, reload, open folder, rebuild index, inspector toggle, and multi-select/compress controls to the Home/Photo ribbon commands as specified. Keep conditional selection bar/import report/library error above the gallery.

- [ ] **Step 3: Rehome compression/dashboard/settings actions**

Compression profile add stays in the profile card header; manual compress is in the ribbon. Dashboard reload uses the Home reload or a small daemon-card control. Settings keeps its existing inner controls; add the nav reload if the current "設定を再読込" action is otherwise hidden.

- [ ] **Step 4: Move message and alerts**

Render `global_alerts` above the active screen as slim rows. Render `App.message` in the ribbon's right status slot (truncated). Do not add a toast. Preserve failure details on their screens.

- [ ] **Step 5: Verify states**

Manually test: multi-select bar, import report, library index error, compression failure alert, daemon error, import failure, message. Confirm every path is reachable from the new layout.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src
git commit -m "feat(ui): remove page headers and rehome commands"
```

## Task 16: Library gallery header, 3:2 cards, and year-rail math

**Files:**
- Modify: `crates/ui/src/ui/library.rs`, `crates/ui/src/ui/theme.rs`, `crates/ui/src/state/year_rail.rs`, `crates/ui/src/app/photos.rs`
- Test: theme/layout and year-rail tests

**Interfaces:**
- Produces:
  - `GridMetrics { columns: usize, card_width: f32, row_height: f32 }`
  - `fn library_grid_metrics(available_width: f32) -> GridMetrics`
  - `fn photo_card_width(available_width: f32, columns: usize) -> f32`
  - `fn photo_row_height(card_width: f32) -> f32` (3:2 image + info)
  - Updated `first_visible_photo`, `rail_scroll_offset`, and `grid_row` calculations to take the row height.
- Consumes: existing `uniform_list` and `PhotoBrowser`.

- [ ] **Step 1: Write failing metric tests**

Test 3 columns at 1280/1440, 2 at 1080; exact card widths; row height from 3:2 + info; `rail_scroll_offset(photo_row, columns, row_height, max_offset)` at boundaries; `first_visible_photo` with the new row height. Update existing year-rail tests to pass the row height.

- [ ] **Step 2: Replace fixed `PHOTO_ROW_HEIGHT` usage**

Use `GridMetrics` in `photo_grid`, `load_pages`, year-rail jump/scroll, and anchor helpers. `uniform_list` row height must be constant per layout but recomputed on resize/column change.

- [ ] **Step 3: Implement the gallery header**

42px header: month/count, filter button, sort display, more; no inline search. The filter button may open a popover containing the existing four appearance-only chips. Do not add filtering logic.

- [ ] **Step 4: Update the photo card and inspector dimensions**

3:2 image, min 190px, gap 9px; card info/title/meta; 9 cards fit 3×3 at 1280/1440. Inspector columns 298/276/254 with 168px preview (118px in short windows). Preserve selection/favorite/compression badges and all inspector actions.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
cargo run -p vrc-photo-ui -- --demo --window-size 1280x820
cargo run -p vrc-photo-ui -- --demo --window-size 1440x900
git add crates/ui/src
git commit -m "feat(ui): reflow library into gallery and inspector cards"
```

## Task 17: Search, compression, dashboard, settings reflow

**Files:**
- Modify: `crates/ui/src/ui/search.rs`, `crates/ui/src/ui/compression.rs`, `crates/ui/src/ui/dashboard.rs`, `crates/ui/src/ui/settings.rs`
- Modify: `crates/ui/src/app/lifecycle.rs`, `crates/ui/src/app/settings.rs` if present
- Test: existing UI tests + manual

- [ ] **Step 1: Search**

Remove the page header. Put count/searching/message/selection bar in the results card header. Keep the filter panel and all search behavior. Use the same 3:2 grid metrics.

- [ ] **Step 2: Compression**

Remove the page header. Keep profile list/editor as cards; profile add in the list header; manual compress in the ribbon. Keep all profile/policy/delete behavior.

- [ ] **Step 3: Dashboard**

Remove the page header. Keep stats/queue/daemon/session cards; stretch the queue and right column to fill available height. Keep all daemon/log/error controls.

- [ ] **Step 4: Settings**

Remove the page header. Keep the existing settings cards and controls; add the nav reload if needed. Do not add glass intensity or year-rail settings.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photo-ui
cargo run -p vrc-photo-ui -- --demo --window-size 1280x820
git add crates/ui/src
git commit -m "feat(ui): reflow search, compression, dashboard, and settings"
```

## Task 18: Viewer/modal glass, focus, responsive, and reduced motion

**Files:**
- Modify: `crates/ui/src/ui/viewer.rs`, `crates/ui/src/ui/import.rs`, `crates/ui/src/ui/dialogs.rs`, `crates/ui/src/ui/shell.rs`, `crates/ui/src/ui/theme.rs`
- Test: existing focus/keyboard tests + manual

- [ ] **Step 1: Restyle overlays**

Apply the glass surface/scrim/radius tokens to the full-screen viewer, import modal, manual compression, and confirmation overlays. Do not change their behavior or focus order.

- [ ] **Step 2: Responsive rules**

At width <=1180 hide the ribbon status note; at <=1060 collapse the sidebar and hide the inspector as today; at height <=760 use 62px ribbon and shorten card/inspector metrics. Ensure no horizontal scroll at 1080×700.

- [ ] **Step 3: Keyboard/focus/reduced motion**

Verify `Ctrl/Cmd+B`, `Ctrl/Cmd+I`, `Ctrl/Cmd+K`, viewer Esc/arrows/Tab trap, import focus trap, and reduced-motion behavior. No global shortcut may fire through the viewer/modal.

- [ ] **Step 4: Verify and commit**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git add crates/ui/src
git commit -m "feat(ui): polish viewer, modals, and responsive shell"
```

## Task 19: Visual QA, docs, and final PR

**Files:**
- Modify: `docs/ui.md`, `docs/architecture.md` (separate docs-only PR)
- Test: screenshots and full checks

- [ ] **Step 1: Capture final screenshots**

At 1280×820 and 1440×900, capture Library, Search, Compression, Dashboard, Settings, Viewer, Import modal, Manual compression. Capture 1080×700 for overflow. Check with and without sidebar/ribbon/inspector collapsed, plus failure states.

- [ ] **Step 2: Run the full verification**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 3: Update docs in a separate docs-only PR**

`docs/ui.md`: new shell, titlebar, 3 tabs, command locations, headerless screens, responsive metrics, mock-only exclusions.  
`docs/architecture.md`: new UI module tree and App/screen boundaries.  
Do not mix with code PRs.

- [ ] **Step 4: Open the final code PR and request review**

Include before/after screenshots, the exact commands run, Linux client/server decoration result, and Windows deferral to #9. Do not tag a release.

## Execution notes

- Run PR1 first. Do not start PR2 while PR1 is unmerged; the module paths are the contract.
- When moving code, prefer `git mv`-like moves and compiler-driven imports. Do not rewrite logic during PR1.
- If a split task becomes too large, split by item into additional commits within PR1; keep every commit compiling.
- If a visual decision is missing, consult the design spec's §6.1 (mock-only) and §11 (screen specs). Do not invent filtering/list behavior.
- If GPUI's `TitleBar` cannot be styled to 46px, keep its native height and adjust the header card; do not draw fake window controls.
- If OS blur is unavailable, keep the ambient layer and translucent cards; do not add a renderer feature.
