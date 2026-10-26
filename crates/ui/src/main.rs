use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use chrono::Utc;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::compressor;
use vrc_photo_core::db::Db;
use vrc_photo_core::importer::{
    self, ImportIssue, ImportIssueKind, ImportMode, ImportProgress, ImportRename, ImportResult,
};
use vrc_photo_core::linker;
use vrc_photo_core::logging::{DAEMON_LOG_FILE, UI_LOG_FILE, logs_dir};
use vrc_photo_core::model::{
    CompressionFormat, CompressionJob, CompressionProfile, DaemonCommand, DaemonOperation,
    LibraryStats, MAX_JOB_RETRIES, NewPhoto, Photo, PhotoContext, PhotoIndexEntry, PhotoPlayer,
    PhotoSort, PhotoTextFilter, QueueStatus, ResizeMode,
};
use vrc_photo_core::preview;
use vrc_photo_core::scanner;
use vrc_photo_core::scanner::ScanFailure;
use vrc_photo_core::settings::{AppSettings, CardSize};
use vrc_photo_core::thumbnail;
use vrc_photo_core::time;
use vrc_photo_ipc::{
    Endpoint, Event as IpcEvent, Request as IpcRequest, ScanPhase as IpcScanPhase, StatusSnapshot,
};

use crate::app::cli::{log_timing, parse_args, process_start, timing_enabled};
use crate::app::{App, LaunchOptions, Screen};
use crate::state::photo_browser::{
    CountDebounce, PhotoBrowser, PhotoIndex, anchor_photo_id, apply_library_index, list_scroll_top,
    search_anchor_after_result, sync_search_mirror,
};
use crate::state::preview::{
    PreviewKey, PreviewQueue, preview_cache_dir, preview_key, preview_placeholder_label,
};
use crate::state::thumbnail::ThumbnailCache;
use crate::state::viewer::{
    MorphImageKind, ViewerBoxState, ViewerMorph, ViewerOpenSource, ViewerPanelState,
    current_tagged_bounds, morph_image_plan, preferred_bounds,
};
use crate::ui::components::{
    ButtonState, ButtonTone, glass_panel, manual_option_row, status_progress_bar,
};
use crate::ui::theme::*;

mod app;
mod ipc_bridge;
mod logging;
mod state;
mod ui;
mod update_view;

const PHOTO_ROW_HEIGHT: f32 = 196.0;
const CARD_BODY_GAP: f32 = 8.0;
const CARD_TEXT_GAP: f32 = 3.0;
const CARD_WORLD_LINE_HEIGHT: f32 = 18.0;
const CARD_META_LINE_HEIGHT: f32 = 15.0;
const CARD_ROW_SLACK: f32 = 24.0;
const NAV_ITEM_HEIGHT: f32 = 40.0;
const NAV_ITEM_GAP: f32 = 3.0;

gpui_kit::actions!(
    ui,
    [
        ToggleSidebar,
        ToggleInspector,
        FocusSearch,
        OpenQuickLook,
        CloseQuickLook,
        ViewerPreviousPhoto,
        ViewerNextPhoto,
        QuickLookNextFocus,
        QuickLookPrevFocus,
    ]
);

/// Outcome of one import run, kept on the Photos screen until the user closes
/// it so failed files can be retried without reopening the file picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScanProgressView {
    phase: IpcScanPhase,
    processed: usize,
    total: Option<usize>,
    indexed: usize,
    root: usize,
    roots: usize,
}

#[derive(Clone)]
struct ImportReportView {
    mode: ImportMode,
    dest: PathBuf,
    total: usize,
    processed: usize,
    imported: usize,
    skipped: usize,
    issues: Vec<ImportIssue>,
    renamed: Vec<ImportRename>,
    remaining: Vec<PathBuf>,
    cancelled: bool,
    scan_failures: Vec<ScanFailure>,
    scan_error: Option<String>,
}

#[derive(Clone)]
enum ImportSource {
    Files(Vec<PathBuf>),
    Folder {
        folder: PathBuf,
        files: Vec<PathBuf>,
        issues: Vec<ImportIssue>,
    },
}

impl ImportSource {
    fn files(&self) -> &[PathBuf] {
        match self {
            Self::Files(files) => files,
            Self::Folder { files, .. } => files,
        }
    }

    fn issues(&self) -> &[ImportIssue] {
        match self {
            Self::Files(_) => &[],
            Self::Folder { issues, .. } => issues,
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::Files(files) => format!("{} 件のファイル", files.len()),
            Self::Folder { folder, files, .. } => {
                format!("{} · {} 件", folder.display(), files.len())
            }
        }
    }
}

#[derive(Clone)]
enum ImportDialogOrigin {
    New,
    Retry {
        report: ImportReportView,
        sources: Vec<PathBuf>,
    },
    Resume {
        report: ImportReportView,
        sources: Vec<PathBuf>,
    },
}

impl ImportDialogOrigin {
    fn run_summary(&self) -> Option<String> {
        match self {
            Self::New => None,
            Self::Retry { sources, .. } => Some(format!("失敗分 {} 件", sources.len())),
            Self::Resume { sources, .. } => Some(format!("残り {} 件", sources.len())),
        }
    }
}

#[derive(Clone)]
enum ImportDialogPhase {
    Ready,
    Running {
        progress: ImportProgress,
        cancel: Arc<AtomicBool>,
        cancel_requested: bool,
    },
}

#[derive(Clone)]
struct ImportDialogState {
    origin: ImportDialogOrigin,
    mode: ImportMode,
    dest: PathBuf,
    source: Option<ImportSource>,
    error: Option<String>,
    phase: ImportDialogPhase,
}

enum ImportRunMessage {
    Progress(ImportProgress),
    Finished(Result<ImportResult, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualOutputTarget {
    Folder,
    Profile,
    Clipboard,
}

fn manual_output_available(photo_count: usize, target: ManualOutputTarget) -> bool {
    target != ManualOutputTarget::Clipboard || photo_count == 1
}

#[derive(Debug, Clone)]
struct ManualCompressionDialog {
    photo_ids: Vec<i64>,
    profile_id: String,
    output: ManualOutputTarget,
    folder: Option<String>,
    keep_original: bool,
}

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

type BoundsFrame = (u64, Bounds<Pixels>);
type CardBoundsMap = Rc<RefCell<HashMap<i64, BoundsFrame>>>;
type InspectorBounds = Rc<RefCell<Option<BoundsFrame>>>;

type ViewerActivate = Rc<dyn Fn(&mut App, &mut Window, &mut Context<App>)>;

impl App {
    fn new(window: &mut Window, cx: &mut Context<Self>, options: &LaunchOptions) -> Self {
        let (db, demo, database_ok, mut message, demo_assets) = open_database(options);
        let db_path = db.path().and_then(|path| std::fs::canonicalize(path).ok());
        let health_token = std::env::var("VRC_PHOTO_HEALTH_TOKEN").ok();
        let updating = health_token.is_some();
        let initial_settings = AppSettings::load(&db).unwrap_or_default();
        let initial_library_revision = db.library_revision().unwrap_or(0);
        cx.set_reduce_motion(initial_settings.ui_reduce_motion);
        let world_input = cx.new(|cx| InputState::new(window, cx).placeholder("ワールド名 / ID"));
        let player_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("プレイヤー名 / ID"));
        let tag_input = cx.new(|cx| InputState::new(window, cx).placeholder("タグ"));
        let tag_edit_input = cx.new(|cx| InputState::new(window, cx).placeholder("タグを追加"));
        let note_edit_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("メモを入力")
                .auto_grow(2, 6)
        });
        let photo_root_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例: C:/Users/you/Pictures/VRChat")
                .default_value(
                    initial_settings
                        .photo_roots
                        .first()
                        .cloned()
                        .unwrap_or_default(),
                )
        });
        let log_dir_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例: C:/Users/you/AppData/LocalLow/VRChat/VRChat")
                .default_value(initial_settings.log_dir.clone().unwrap_or_default())
        });
        let import_dir_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("取り込み先フォルダ")
                .default_value(initial_settings.import_dir().display().to_string())
        });
        let profile_suffix_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("例: _webp"));
        let profile_suffix_subscription = cx.subscribe(
            &profile_suffix_input,
            |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    this.edit_profile(|profile| profile.suffix = value.clone(), cx);
                }
            },
        );
        let photo_grid_focus = cx.focus_handle();
        let import_dialog_focus = cx.focus_handle();
        let quick_look_close_focus = cx.focus_handle();
        let quick_look_prev_focus = cx.focus_handle();
        let quick_look_next_focus = cx.focus_handle();
        let quick_look_details_focus = cx.focus_handle();
        let quick_look_favorite_focus = cx.focus_handle();
        let quick_look_compress_focus = cx.focus_handle();
        let quick_look_trash_focus = cx.focus_handle();
        let quick_look_add_tag_focus = cx.focus_handle();
        let quick_look_note_edit_focus = cx.focus_handle();
        let quick_look_note_save_focus = cx.focus_handle();
        let quick_look_note_cancel_focus = cx.focus_handle();
        let empty_index = Arc::new(PhotoIndex::new(Vec::new()));
        let mut app = Self {
            db,
            screen: Screen::Photos,
            sidebar_collapsed: initial_settings.ui_sidebar_collapsed,
            sidebar_status_tab: SidebarStatusTab::Compression,
            sidebar_motion_epoch: 0,
            nav_previous: Screen::Photos,
            nav_epoch: 0,
            screen_epoch: 0,
            screen_direction: 1.0,
            render_epoch: 0,
            card_image_bounds: Rc::new(RefCell::new(HashMap::new())),
            inspector_preview_bounds: Rc::new(RefCell::new(None)),
            viewer_morph: None,
            viewer_morph_epoch: 0,
            viewer_return_source: None,
            viewer_nav_epoch: 0,
            viewer_nav_direction: 1.0,
            viewer_nav_started_at: Instant::now(),
            viewer_open_started_at: Instant::now(),
            viewer_box: ViewerBoxState {
                epoch: 0,
                started_at: Instant::now(),
                from_size: (0.0, 0.0),
                to_aspect: 16.0 / 10.0,
                to_details_width: 0.0,
                duration: Duration::ZERO,
            },
            viewer_panel: ViewerPanelState {
                epoch: 0,
                started_at: Instant::now(),
                from_width: 0.0,
                to_width: 0.0,
                duration: Duration::ZERO,
            },
            inspector_collapsed: initial_settings.ui_inspector_collapsed,
            inspector_epoch: 0,
            hovered_nav: None,
            daemon_age_secs: None,
            daemon_supervisor: None,
            daemon_pid: None,
            ipc_endpoint: None,
            ipc_connected: false,
            ipc_error: None,
            daemon_paused: false,
            scan_progress: None,
            last_library_revision: initial_library_revision,
            library: PhotoBrowser::new(empty_index.clone()),
            search_view: PhotoBrowser::new(empty_index),
            library_dirty: false,
            library_error: None,
            count_debounce: CountDebounce::default(),
            selected: None,
            selected_ids: Vec::new(),
            selection_anchor: None,
            selection_epoch: 0,
            selection_changed_at: None,
            selected_photo: None,
            selected_players: Vec::new(),
            selected_tags: Vec::new(),
            selected_derivatives: Vec::new(),
            compressed_ids: HashSet::new(),
            duplicate_ids: HashSet::new(),
            thumbnails: ThumbnailCache::default(),
            grid_epoch: 0,
            hovered_photo: None,
            intro_seen: HashMap::new(),
            photo_grid_focus,
            import_dialog_focus,
            quick_look_open: false,
            quick_look_epoch: 0,
            quick_look_return_focus: None,
            quick_look_focus_index: 0,
            quick_look_details_open: false,
            quick_look_details_epoch: 0,
            quick_look_photo_ids: Vec::new(),
            quick_look_index: 0,
            quick_look_close_focus,
            quick_look_prev_focus,
            quick_look_next_focus,
            quick_look_details_focus,
            quick_look_favorite_focus,
            quick_look_compress_focus,
            quick_look_trash_focus,
            quick_look_add_tag_focus,
            quick_look_note_edit_focus,
            quick_look_note_save_focus,
            quick_look_note_cancel_focus,
            preview: None,
            preview_key: None,
            preview_attempt: None,
            preview_queue: PreviewQueue::default(),
            previous_preview: None,
            previous_color: None,
            profiles: Vec::new(),
            profile_index: 0,
            profile_suffix_input,
            _profile_suffix_subscription: profile_suffix_subscription,
            manual_compress: None,
            stats: LibraryStats::default(),
            settings: AppSettings::default(),
            update: update_view::UpdateState::new(health_token, database_ok, demo, db_path),
            jobs: Vec::new(),
            waiting_jobs: Vec::new(),
            active_job_count: 0,
            waiting_job_count: 0,
            daemon_errors: Vec::new(),
            failed_job_count: 0,
            failed_jobs: Vec::new(),
            failed_job_names: HashMap::new(),
            import_report: None,
            import_dialog: None,
            monitor_world: None,
            monitor_instance: None,
            monitor_players: Vec::new(),
            daemon_active: false,
            world_input,
            player_input,
            tag_input,
            tag_edit_input,
            note_edit_input,
            note_edit_id: None,
            photo_root_input,
            log_dir_input,
            import_dir_input,
            search_active: false,
            searching: false,
            search_request: 0,
            applied_search: None,
            pending_search_anchor: None,
            pending_layout_anchor_library: None,
            pending_layout_anchor_search: None,
            search_refresh_pending: false,
            search_favorite: false,
            search_days: Some(7),
            delete_armed: None,
            daemon_shutdown_armed: false,
            message: None,
            demo,
            demo_assets,
        };
        app.ensure_profiles();
        app.reload_all(true, cx);
        if app.library_error.is_none() && app.library.index.total == 0 {
            message.get_or_insert_with(|| {
                "写真がありません。VRChatの写真フォルダを設定するか、--demo で起動してください。"
                    .to_string()
            });
        }
        if let Some(notice) = app.update.notice() {
            message.get_or_insert_with(|| notice.message.to_string());
        }
        app.message = message;
        if let Some(id) = app.library.index.entries.first().map(|entry| entry.id) {
            app.select_photo(id, cx);
            // The grid is not mounted until the first frame, so focus it once
            // the initial render has attached it to the focus tree.
            let photo_grid_focus = app.photo_grid_focus.clone();
            window.defer(cx, move |window, cx| {
                window.focus(&photo_grid_focus, cx);
            });
        }
        if !app.demo
            && !app.daemon_active
            && !updating
            && let Err(error) = spawn_daemon_process()
        {
            app.message = Some(format!("デーモンを起動できません: {error}"));
        }
        app.start_auto_refresh(cx);
        if !app.demo {
            app.start_ipc(cx);
        }
        app.start_update_timers(cx);
        if updating {
            cx.on_next_frame(window, move |app, window, cx| {
                app.complete_update_startup(window, cx);
            });
        }
        if timing_enabled() {
            let started = process_start();
            cx.on_next_frame(window, move |_, _, _| {
                log_timing("first frame (from process start)", started)
            });
        }
        app
    }

    fn start_auto_refresh(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                if this.update(cx, |this, cx| this.refresh_live(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_ipc(&mut self, cx: &mut Context<Self>) {
        let Some(data_dir) = vrc_photo_core::data_dir() else {
            return;
        };
        let endpoint = match Endpoint::for_data_dir(&data_dir) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                tracing::warn!(%error, "cannot resolve daemon IPC endpoint");
                return;
            }
        };
        self.ipc_endpoint = Some(endpoint.clone());
        let (tx, rx) = async_channel::unbounded();
        ipc_bridge::start(endpoint, tx);
        cx.spawn(async move |this, cx| {
            while let Ok(incoming) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.handle_ipc_incoming(incoming, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn handle_ipc_incoming(&mut self, incoming: ipc_bridge::Incoming, cx: &mut Context<Self>) {
        match incoming {
            ipc_bridge::Incoming::Connected { status, daemon_pid } => {
                self.ipc_connected = true;
                self.daemon_pid = Some(daemon_pid);
                self.ipc_error = None;
                self.daemon_shutdown_armed = false;
                self.refresh_monitor();
                self.apply_daemon_status(status);
            }
            ipc_bridge::Incoming::Disconnected(error) => {
                let was_connected = self.ipc_connected;
                self.ipc_connected = false;
                // Keep the last PID: the daemon may still be alive with a dead
                // IPC server. The update helper will wait for it (or time out)
                // instead of replacing files under a running process.
                self.ipc_error = Some(error);
                self.daemon_paused = false;
                self.daemon_shutdown_armed = false;
                self.scan_progress = None;
                if was_connected {
                    self.refresh_monitor();
                }
            }
            ipc_bridge::Incoming::Event(event) => self.handle_ipc_event(event, cx),
        }
        cx.notify();
    }

    fn apply_daemon_status(&mut self, status: StatusSnapshot) {
        self.daemon_paused = status.paused;
        self.scan_progress = status.scanning.then(|| ScanProgressView {
            phase: status.scan_phase.unwrap_or(IpcScanPhase::Walking),
            processed: status.scan_processed,
            total: status.scan_total,
            indexed: status.scan_indexed,
            root: status.scan_root,
            roots: status.scan_roots,
        });
        self.daemon_active = daemon_liveness(self.daemon_age_secs, self.ipc_connected);
    }

    fn handle_ipc_event(&mut self, event: IpcEvent, cx: &mut Context<Self>) {
        match event {
            IpcEvent::ScanStarted { roots } => {
                self.scan_progress = Some(ScanProgressView {
                    phase: IpcScanPhase::Walking,
                    processed: 0,
                    total: None,
                    indexed: 0,
                    root: usize::from(roots > 0),
                    roots,
                });
            }
            IpcEvent::ScanProgress {
                phase,
                processed,
                total,
                indexed,
                root,
                roots,
            } => {
                self.scan_progress = Some(ScanProgressView {
                    phase,
                    processed,
                    total,
                    indexed,
                    root,
                    roots,
                });
            }
            IpcEvent::ScanFinished { .. } => {
                self.scan_progress = None;
                self.refresh_live(cx);
            }
            IpcEvent::CompressionProgress { .. } => {
                self.refresh_live(cx);
            }
            IpcEvent::DaemonErrorChanged { .. } => {
                self.reload_job_state();
            }
            IpcEvent::StatusChanged { status } => {
                self.apply_daemon_status(status);
                self.refresh_monitor();
            }
        }
    }

    fn wake_daemon(&self, request: IpcRequest, cx: &mut Context<Self>) {
        if !self.ipc_connected {
            return;
        }
        let Some(endpoint) = self.ipc_endpoint.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ipc_bridge::request(endpoint, request) })
                .await;
            if let Err(error) = result {
                tracing::debug!(%error, "daemon IPC wake failed");
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn send_wake_commands(&self, cx: &mut Context<Self>) {
        self.wake_daemon(IpcRequest::WakeCommands, cx);
    }

    fn send_wake_jobs(&self, cx: &mut Context<Self>) {
        self.wake_daemon(IpcRequest::WakeJobs, cx);
    }

    fn send_ipc_request(
        &mut self,
        request: IpcRequest,
        success: Option<String>,
        failure: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !self.ipc_connected {
            if let Some(message) = failure {
                self.message = Some(message);
            }
            cx.notify();
            return;
        }
        let Some(endpoint) = self.ipc_endpoint.clone() else {
            self.message = Some("IPCの接続先を解決できません".to_string());
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ipc_bridge::request(endpoint, request) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        if let Some(message) = success {
                            this.message = Some(message);
                        }
                    }
                    Err(error) => {
                        this.message =
                            Some(failure.unwrap_or_else(|| ipc_bridge::error_message(&error)));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn request_daemon_rescan(&mut self, cx: &mut Context<Self>) {
        match self.db.request_daemon_command(DaemonCommand::Rescan) {
            Ok(_) => {
                self.send_wake_commands(cx);
                self.message = Some(if self.ipc_connected {
                    "再スキャンを要求しました。デーモンが処理します。".to_string()
                } else {
                    "再スキャンを要求しました。デーモン起動後に実行されます。".to_string()
                });
            }
            Err(error) => {
                self.message = Some(format!("再スキャンを要求できません: {error}"));
            }
        }
        cx.notify();
    }

    fn toggle_daemon_pause(&mut self, cx: &mut Context<Self>) {
        if !self.ipc_connected {
            self.message = Some("デーモン停止中です".to_string());
            cx.notify();
            return;
        }
        let paused = !self.daemon_paused;
        let success = if paused {
            "監視を一時停止しました"
        } else {
            "監視を再開しました"
        };
        self.send_ipc_request(
            IpcRequest::SetPaused { paused },
            Some(success.to_string()),
            None,
            cx,
        );
    }

    fn toggle_daemon_shutdown(&mut self, cx: &mut Context<Self>) {
        if !self.ipc_connected {
            self.message = Some("デーモン停止中です".to_string());
            cx.notify();
            return;
        }
        if !self.daemon_shutdown_armed {
            self.daemon_shutdown_armed = true;
            self.message = Some("もう一度「デーモンを終了」を押すと終了します".to_string());
            cx.notify();
            return;
        }
        self.daemon_shutdown_armed = false;
        self.send_ipc_request(
            IpcRequest::Shutdown,
            Some("デーモンを終了しました".to_string()),
            None,
            cx,
        );
    }

    fn refresh_live(&mut self, cx: &mut Context<Self>) {
        let count = self.db.count_photos();
        let revision = self.db.library_revision();
        if let Ok(stats) = self.db.stats() {
            self.stats = stats;
        }
        self.reload_job_state();
        self.refresh_monitor();
        if let Some(id) = self.selected {
            self.load_selected_meta(id);
            self.refresh_stale_preview(cx);
        }

        // Path rebinds and replacement status changes keep the same photo
        // count. The daemon bumps this revision so the UI can invalidate its
        // page cache even when `count_photos` is unchanged.
        if let Ok(revision) = revision
            && revision != self.last_library_revision
        {
            if self.screen == Screen::Photos {
                if self.reload_library(true, cx) {
                    self.last_library_revision = revision;
                    self.count_debounce.reset();
                } else {
                    self.mark_library_dirty();
                }
            } else {
                self.last_library_revision = revision;
                self.mark_library_dirty();
                if self.screen == Screen::Search
                    && let Some(filter) = self.applied_search.clone()
                {
                    self.start_search(filter, cx);
                }
            }
        }

        // Only compare counts when the query succeeds; treating a transient
        // error as zero used to blank the whole library.
        if let Ok(count) = count {
            if count != self.library.index.total as i64 {
                // A burst of files arriving from the daemon must not rebuild
                // the 100k-entry index on every three-second tick. Wait until
                // the observed count has been stable for a quiet period.
                self.mark_library_dirty();
                if self.screen == Screen::Photos
                    && self
                        .count_debounce
                        .observe(count, Instant::now(), Duration::from_secs(2))
                    && self.reload_library(true, cx)
                {
                    self.count_debounce.reset();
                }
            } else {
                self.count_debounce.reset();
            }
        }
        cx.notify();
    }

    fn reload_job_state(&mut self) {
        match self.db.list_jobs(20) {
            Ok(jobs) => self.jobs = jobs,
            Err(error) => tracing::warn!(%error, "cannot load recent compression jobs"),
        }
        match self.db.count_active_jobs() {
            Ok(count) => self.active_job_count = count,
            Err(error) => tracing::warn!(%error, "cannot count active compression jobs"),
        }
        match self.db.count_jobs(QueueStatus::Waiting) {
            Ok(count) => self.waiting_job_count = count,
            Err(error) => tracing::warn!(%error, "cannot count waiting compression jobs"),
        }
        match self.db.list_jobs_by_status(QueueStatus::Waiting, 100) {
            Ok(jobs) => self.waiting_jobs = jobs,
            Err(error) => tracing::warn!(%error, "cannot load waiting compression jobs"),
        }
        match self.db.count_jobs(QueueStatus::Failed) {
            Ok(count) => self.failed_job_count = count,
            Err(error) => tracing::warn!(%error, "cannot count failed compression jobs"),
        }
        match self.db.list_jobs_by_status(QueueStatus::Failed, 100) {
            Ok(jobs) => self.failed_jobs = jobs,
            Err(error) => tracing::warn!(%error, "cannot load failed compression jobs"),
        }
        self.failed_job_names.clear();
        for job in &self.failed_jobs {
            if self.failed_job_names.contains_key(&job.photo_id) {
                continue;
            }
            if let Ok(Some(photo)) = self.db.photo_by_id(job.photo_id) {
                self.failed_job_names.insert(job.photo_id, photo.file_name);
            }
        }
        self.daemon_errors = self.db.daemon_errors().unwrap_or_default();
    }

    fn retry_job(&mut self, id: String, cx: &mut Context<Self>) {
        match self.db.retry_job(&id) {
            Ok(true) => {
                self.message = Some("再試行をキューに戻しました".to_string());
                self.reload_job_state();
                self.send_wake_jobs(cx);
            }
            Ok(false) => {
                self.message = Some(format!("再試行の上限（{MAX_JOB_RETRIES}回）に達しました"));
            }
            Err(error) => {
                tracing::warn!(%error, %id, "cannot retry compression job");
                self.message = Some(format!("再試行できません: {error}"));
            }
        }
        cx.notify();
    }

    fn retry_all_failed_jobs(&mut self, cx: &mut Context<Self>) {
        match self.db.retry_failed_jobs() {
            Ok(count) => {
                self.message = Some(format!("{count} 件を再試行に戻しました（上限到達は除外）"));
                self.reload_job_state();
                self.send_wake_jobs(cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot retry failed compression jobs");
                self.message = Some(format!("再試行できません: {error}"));
            }
        }
        cx.notify();
    }

    fn request_release_waiting_jobs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(description) = release_confirmation(
            self.settings.auto_compress_keep_original,
            self.waiting_job_count,
        ) else {
            self.release_waiting_jobs(cx);
            return;
        };
        let entity = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            let description = description.clone();
            alert
                .title("待機ジョブを圧縮")
                .description(description)
                .confirm()
                .ok_text("圧縮する")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("キャンセル")
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.release_waiting_jobs(cx));
                    true
                })
        });
    }

    fn release_waiting_jobs(&mut self, cx: &mut Context<Self>) {
        let policy = compression_policy(self.settings.auto_compress_keep_original);
        match self.db.release_waiting_jobs(policy) {
            Ok(0) => self.message = Some("未実行のジョブはありません".to_string()),
            Ok(count) => {
                self.message = Some(format!("{count} 件を圧縮キューに戻しました"));
                self.reload_job_state();
                self.send_wake_jobs(cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot release waiting compression jobs");
                self.message = Some(format!("待機ジョブを開始できません: {error}"));
            }
        }
        cx.notify();
    }

    fn request_daemon_command(&mut self, command: DaemonCommand, cx: &mut Context<Self>) {
        match self.db.request_daemon_command(command) {
            Ok(true) => {
                self.message = Some("再試行を要求しました。デーモンが処理します。".to_string())
            }
            Ok(false) => self.message = Some("すでに再試行を要求済みです。".to_string()),
            Err(error) => self.message = Some(format!("再試行を要求できません: {error}")),
        }
        self.send_wake_commands(cx);
        cx.notify();
    }

    fn ensure_profiles(&mut self) {
        self.profiles = self.db.ensure_default_profiles().unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot ensure default compression profiles");
            Vec::new()
        });
    }

    /// Refreshes side panels without touching the library snapshot or pages.
    fn reload_aux(&mut self, cx: &mut Context<Self>) {
        self.profiles = self.db.list_profiles().unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot load compression profiles");
            Vec::new()
        });
        if self.profile_index >= self.profiles.len() {
            self.profile_index = 0;
        }
        self.stats = self.db.stats().unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot load library stats");
            LibraryStats::default()
        });
        self.compressed_ids = self
            .db
            .compressed_photo_ids()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot load compressed photo ids");
                Vec::new()
            })
            .into_iter()
            .collect();
        self.duplicate_ids = self
            .db
            .duplicate_photo_ids()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot load duplicate photo ids");
                Vec::new()
            })
            .into_iter()
            .collect();
        self.reload_job_state();
        self.settings = AppSettings::load(&self.db).unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot load app settings");
            AppSettings::default()
        });
        cx.set_reduce_motion(self.settings.ui_reduce_motion);
        self.refresh_monitor();
        if let Some(id) = self.selected {
            self.load_selected_meta(id);
            self.refresh_stale_preview(cx);
        }
    }

    /// Rebuilds the ordered library snapshot and its page cache.
    ///
    /// With `preserve_anchor`, the photo at the top-left of the first visible
    /// row stays at the top after the rebuild. A root change or an explicit
    /// reset passes `false` to return the grid to the first row.
    ///
    /// On a query error the previous snapshot, pages, and scroll offset stay
    /// mounted and `library_error` exposes the retry action. Returns `true`
    /// when the snapshot was replaced.
    fn reload_library(&mut self, preserve_anchor: bool, cx: &mut Context<Self>) -> bool {
        let anchor = preserve_anchor.then(|| {
            anchor_photo_id(
                &self.library.index,
                list_scroll_top(&self.library.scroll),
                self.library.columns,
                self.card_metrics().row_height,
            )
        });
        let anchor = anchor.flatten();

        let index_start = Instant::now();
        let result = self.db.photo_index(PhotoSort::Newest);
        log_timing("photo_index", index_start);
        let error = apply_library_index(&mut self.library, result);
        if let Some(error) = error {
            self.library_error = Some(error);
            // Leave the mounted view and side panels untouched; the next
            // successful reload repopulates them.
            return false;
        }

        self.library_error = None;
        self.library_dirty = false;
        let columns = self.library.columns.max(1);
        if let Some(row) = anchor.and_then(|id| self.library.index.row_of_id(id)) {
            self.library
                .scroll
                .scroll_to_item_strict(row / columns, ScrollStrategy::Top);
        } else {
            self.library
                .scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
        }

        // An active result set stays mounted until the user clears it or runs
        // a new search; the inactive mirror only needs the fresh snapshot.
        if !self.search_active {
            self.search_view.replace_index(self.library.index.clone());
        }
        self.reload_aux(cx);
        true
    }

    /// Full rebuild used at startup: resets the grid animation state too.
    fn reload_all(&mut self, reset_scroll: bool, cx: &mut Context<Self>) {
        self.grid_epoch = self.grid_epoch.wrapping_add(1);
        self.intro_seen.clear();
        self.hovered_photo = None;
        self.reload_library(!reset_scroll, cx);
    }

    fn mark_library_dirty(&mut self) {
        self.library_dirty = true;
    }

    /// Changes screens without rebuilding the library unless Photos was marked
    /// dirty while another screen was mounted.
    fn switch_screen(&mut self, screen: Screen, cx: &mut Context<Self>) {
        if self.screen != screen {
            self.nav_previous = self.screen;
            self.nav_epoch = self.nav_epoch.wrapping_add(1);
            self.screen_direction = screen_direction(self.screen, screen);
            self.screen_epoch = self.screen_epoch.wrapping_add(1);
        }
        // The two photo grids have separate snapshots, so a selection made in
        // one tab cannot stay meaningful in the other. Keep it when visiting
        // Compression/Settings so the manual-compression action can use it.
        if self.screen != screen
            && matches!(self.screen, Screen::Photos | Screen::Search)
            && matches!(screen, Screen::Photos | Screen::Search)
        {
            self.clear_selection(cx);
        }
        self.screen = screen;
        if screen == Screen::Photos && self.library_dirty {
            self.reload_library(true, cx);
        } else if screen == Screen::Search && !self.search_active {
            // The no-query mirror only needs a refresh when the library
            // snapshot changed while another screen was mounted. A dirty
            // library is rebuilt first so the mirror also sees new photos.
            if self.library_dirty {
                self.reload_library(true, cx);
            } else {
                sync_search_mirror(&mut self.search_view, &self.library);
            }
        }
        cx.notify();
    }

    fn refresh_monitor(&mut self) {
        let now = Utc::now().timestamp();
        self.daemon_age_secs = self
            .db
            .get_setting("daemon_heartbeat")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<i64>().ok())
            .map(|timestamp| (now - timestamp).max(0));
        self.daemon_active = daemon_liveness(self.daemon_age_secs, self.ipc_connected);
        self.daemon_supervisor = self.db.get_setting("daemon_supervisor").ok().flatten();
        self.monitor_world = None;
        self.monitor_instance = None;
        self.monitor_players.clear();
        if let Ok(Some(session)) = self.db.session_at(now) {
            if let Ok(Some(world)) = self.db.world_by_id(session.world_id) {
                self.monitor_world = world.alias.or(world.name);
            }
            self.monitor_instance = session.instance_id;
            if let Ok(players) = self.db.players_at(session.id, now) {
                self.monitor_players = players
                    .into_iter()
                    .map(|player| player.display_name)
                    .collect();
            }
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.quick_look_open {
            return;
        }
        self.sidebar_collapsed = !self.sidebar_collapsed;
        // The width animation recreates the nav subtree, so its animations must
        // render at the current tab instead of replaying the last tab change.
        self.nav_previous = self.screen;
        self.sidebar_motion_epoch = self.sidebar_motion_epoch.wrapping_add(1);
        self.hovered_nav = None;
        self.settings.ui_sidebar_collapsed = self.sidebar_collapsed;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save sidebar state");
        }
        cx.notify();
    }

    fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        if self.quick_look_open {
            return;
        }
        self.inspector_collapsed = !self.inspector_collapsed;
        self.inspector_epoch = self.inspector_epoch.wrapping_add(1);
        self.settings.ui_inspector_collapsed = self.inspector_collapsed;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save inspector state");
        }
        cx.notify();
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_look_open {
            return;
        }
        self.switch_screen(Screen::Search, cx);
        let focus_handle = self.world_input.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        // The search screen may not be mounted yet when the action fires, so
        // retry after the deferred update has attached the input to the tree.
        window.defer(cx, move |window, cx| {
            window.focus(&focus_handle, cx);
        });
        cx.notify();
    }

    fn color_for(&self, photo: &Photo) -> u32 {
        self.thumbnails
            .color(photo.id)
            .unwrap_or_else(|| scene_color(photo.world_name.as_deref().unwrap_or("")))
    }

    fn selected_color(&self) -> u32 {
        self.selected_photo
            .as_ref()
            .map(|photo| self.color_for(photo))
            .unwrap_or(BG)
    }

    fn begin_note_edit(
        &mut self,
        id: i64,
        note: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_edit_id = Some(id);
        let text = note.unwrap_or_default();
        self.note_edit_input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        let focus = self.note_edit_input.read(cx).focus_handle(cx);
        // The textarea mounts on the next frame, after the editor state flips.
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_details_open && this.note_edit_id == Some(id)
            {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    fn save_note(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.note_edit_input.read(cx).value();
        let note = normalize_note(&value);
        if let Err(error) = self.db.set_note(id, note) {
            tracing::warn!(%error, photo_id = id, "cannot save photo note");
            self.message = Some(format!("メモを保存できません: {error}"));
            cx.notify();
            return;
        }
        self.note_edit_id = None;
        self.load_selected_meta(id);
        let focus = self.quick_look_note_edit_focus.clone();
        // The edit action mounts after the textarea disappears.
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_details_open && this.note_edit_id.is_none() {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    fn cancel_note_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.note_edit_id.take().is_none() {
            return;
        }
        let focus = self.quick_look_note_edit_focus.clone();
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_details_open && this.note_edit_id.is_none() {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    fn handle_viewer_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.note_edit_id.is_some() {
            self.cancel_note_edit(window, cx);
        } else {
            self.close_quick_look(window, cx);
        }
    }

    fn actual_source(
        &self,
        id: i64,
        source: ViewerOpenSource,
    ) -> Option<(ViewerOpenSource, Bounds<Pixels>)> {
        let card = current_tagged_bounds(
            self.card_image_bounds.borrow().get(&id).copied(),
            self.render_epoch,
        );
        let inspector =
            current_tagged_bounds(*self.inspector_preview_bounds.borrow(), self.render_epoch);
        let prefer_card = source != ViewerOpenSource::Inspector;
        let chosen = preferred_bounds(prefer_card, card, inspector)?;
        if card == Some(chosen) {
            Some((ViewerOpenSource::Card, chosen))
        } else {
            Some((ViewerOpenSource::Inspector, chosen))
        }
    }

    fn open_quick_look(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        source: ViewerOpenSource,
    ) {
        if self.quick_look_open || self.selected_photo.is_none() {
            return;
        }
        if self
            .viewer_morph
            .as_ref()
            .is_some_and(|morph| morph.closing)
        {
            self.viewer_morph = None;
            self.viewer_morph_epoch = self.viewer_morph_epoch.wrapping_add(1);
        }
        self.message = None;
        self.delete_armed = None;
        self.note_edit_id = None;
        self.quick_look_return_focus = window.focused(cx);
        self.quick_look_photo_ids = self.viewer_source_ids();
        self.quick_look_index = self
            .quick_look_photo_ids
            .iter()
            .position(|id| Some(*id) == self.selected)
            .unwrap_or(0);
        let id = self.selected.expect("selected photo");
        let aspect = self
            .selected_photo
            .as_ref()
            .map(|photo| {
                let (width, height) = photo.effective_dimensions();
                if height > 0 {
                    width as f32 / height as f32
                } else {
                    16.0 / 9.0
                }
            })
            .unwrap_or(16.0 / 9.0);
        let viewport = window.viewport_size();
        let to = viewer_image_rect(viewport, self.quick_look_details_open, aspect);
        let now = Instant::now();
        self.viewer_nav_epoch = 0;
        self.viewer_nav_direction = 1.0;
        self.viewer_nav_started_at = now;
        self.viewer_open_started_at = now;
        let details_width = if self.quick_look_details_open {
            VIEWER_DETAILS_WIDTH
        } else {
            0.0
        };
        let from_size = (f32::from(to.size.width), f32::from(to.size.height));
        self.viewer_box = ViewerBoxState {
            epoch: self.viewer_box.epoch.wrapping_add(1),
            started_at: now,
            from_size,
            to_aspect: aspect,
            to_details_width: details_width,
            duration: Duration::ZERO,
        };
        self.viewer_panel = ViewerPanelState {
            epoch: self.viewer_panel.epoch.wrapping_add(1),
            started_at: now,
            from_width: details_width,
            to_width: details_width,
            duration: Duration::ZERO,
        };
        let resolved = self.actual_source(id, source);
        if let Some((actual_source, _)) = resolved {
            self.viewer_return_source = Some(actual_source);
        }
        if !cx.reduce_motion()
            && let Some((actual_source, from)) = resolved
        {
            let thumbnail = self.thumbnails.get(id);
            let preview = self.preview.clone();
            if thumbnail.is_some() || preview.is_some() {
                self.viewer_morph_epoch = self.viewer_morph_epoch.wrapping_add(1);
                let epoch = self.viewer_morph_epoch;
                let color = self
                    .selected_photo
                    .as_ref()
                    .map(|photo| self.color_for(photo))
                    .unwrap_or(0);
                self.viewer_morph = Some(ViewerMorph {
                    epoch,
                    started_at: Instant::now(),
                    from,
                    to,
                    thumbnail,
                    preview,
                    color,
                    source: actual_source,
                    closing: false,
                    fade_only: false,
                    backdrop_from: 1.0,
                });
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(MOTION_MORPH + Duration::from_millis(60))
                        .await;
                    this.update(cx, |this, cx| {
                        if this.viewer_morph_epoch == epoch
                            && this
                                .viewer_morph
                                .as_ref()
                                .is_some_and(|morph| !morph.closing)
                        {
                            this.viewer_morph = None;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }
        }
        self.quick_look_open = true;
        self.quick_look_epoch = self.quick_look_epoch.wrapping_add(1);
        self.quick_look_focus_index = 0;
        // Remember the last details state for the session, but render it
        // statically: only an explicit toggle animates the panel.
        self.quick_look_details_epoch = 0;
        let close_focus = self.quick_look_close_focus.clone();
        window.focus(&close_focus, cx);
        // The close button mounts with the overlay on this frame, so make the
        // focus stick once the focus tree contains it. Guard the retry so a
        // close (or close+reopen) cannot steal focus back to a stale overlay.
        let epoch = self.quick_look_epoch;
        cx.defer_in(window, move |this, window, cx| {
            if this.quick_look_open && this.quick_look_epoch == epoch {
                window.focus(&close_focus, cx);
            }
        });
        cx.notify();
    }

    fn close_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_look_open {
            return;
        }
        let opening = self
            .viewer_morph
            .as_ref()
            .filter(|morph| !morph.closing)
            .map(|morph| (morph.current_bounds(Instant::now()), morph.source));
        let viewer_rect = if let Some((bounds, _)) = opening {
            bounds
        } else {
            let viewport = window.viewport_size();
            let (width, height) = self.viewer_box.current_size(viewport);
            centered_viewer_rect(viewport, self.viewer_panel.current_width(), width, height)
        };
        let from = viewer_rect;
        let source = opening
            .map(|(_, source)| source)
            .or(self.viewer_return_source)
            .unwrap_or(ViewerOpenSource::Auto);
        let id = self.selected.unwrap_or_default();
        let target = self.actual_source(id, source);
        let (to, target_kind, fade_only) = match target {
            Some((kind, bounds)) => (bounds, kind, false),
            None => (from, source, true),
        };
        let backdrop_from = eased_progress(
            self.viewer_open_started_at,
            Duration::from_millis(240),
            Instant::now(),
        );
        let thumbnail = self.thumbnails.get(id);
        let preview = self.preview.clone();
        let color = self
            .selected_photo
            .as_ref()
            .map(|photo| self.color_for(photo))
            .unwrap_or(0);

        if !cx.reduce_motion() && (thumbnail.is_some() || preview.is_some()) {
            self.viewer_morph_epoch = self.viewer_morph_epoch.wrapping_add(1);
            let epoch = self.viewer_morph_epoch;
            self.viewer_morph = Some(ViewerMorph {
                epoch,
                started_at: Instant::now(),
                from,
                to,
                thumbnail,
                preview,
                color,
                source: target_kind,
                closing: true,
                fade_only,
                backdrop_from,
            });
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(MOTION_MORPH + Duration::from_millis(60))
                    .await;
                this.update(cx, |this, cx| {
                    if this.viewer_morph_epoch == epoch
                        && this
                            .viewer_morph
                            .as_ref()
                            .is_some_and(|morph| morph.closing)
                    {
                        this.viewer_morph = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        } else {
            self.viewer_morph = None;
        }
        self.viewer_return_source = None;
        self.quick_look_open = false;
        self.delete_armed = None;
        self.note_edit_id = None;
        let focus = self
            .quick_look_return_focus
            .take()
            .unwrap_or_else(|| self.photo_grid_focus.clone());
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Closes a viewer whose backing photo vanished (for example, an external
    /// delete picked up by the three-second refresh) and restores focus.
    fn guard_viewer_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_look_open || self.selected_photo.is_some() {
            return;
        }
        self.quick_look_open = false;
        self.delete_armed = None;
        self.note_edit_id = None;
        self.viewer_morph = None;
        self.viewer_morph_epoch = self.viewer_morph_epoch.wrapping_add(1);
        self.viewer_return_source = None;
        if let Some(id) = self.selected.take() {
            self.thumbnails.unpin(id);
        }
        self.selected_ids.clear();
        self.selection_anchor = None;
        self.quick_look_photo_ids.clear();
        self.quick_look_index = 0;
        self.preview = None;
        self.previous_preview = None;
        self.previous_color = None;
        let focus = self
            .quick_look_return_focus
            .take()
            .unwrap_or_else(|| self.photo_grid_focus.clone());
        cx.defer_in(window, move |this, window, cx| {
            if !this.quick_look_open {
                window.focus(&focus, cx);
            }
        });
    }

    fn viewer_source_ids(&self) -> Vec<i64> {
        let browser = if self.screen == Screen::Search && self.search_active {
            &self.search_view
        } else {
            &self.library
        };
        let source: Vec<i64> = browser.index.entries.iter().map(|entry| entry.id).collect();
        match self.selected {
            Some(id) if source.contains(&id) => source,
            Some(id) => vec![id],
            None => Vec::new(),
        }
    }

    fn step_viewer_photo(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_look_open || self.quick_look_photo_ids.is_empty() {
            return;
        }
        if self
            .viewer_morph
            .as_ref()
            .is_some_and(|morph| !morph.closing)
        {
            self.viewer_morph = None;
        }
        let next = step_index(
            self.quick_look_index,
            self.quick_look_photo_ids.len(),
            delta,
        );
        if next == self.quick_look_index {
            return;
        }
        self.quick_look_index = next;
        let focused = window.focused(cx);
        let prev_disabled = next == 0 && focused.as_ref() == Some(&self.quick_look_prev_focus);
        let next_disabled = next + 1 == self.quick_look_photo_ids.len()
            && focused.as_ref() == Some(&self.quick_look_next_focus);
        if prev_disabled || next_disabled {
            self.quick_look_focus_index = 0;
            window.focus(&self.quick_look_close_focus, cx);
        }
        // Keep the top-bar navigation focused; only the library selection and
        // the displayed photo change behind the overlay.
        let viewport = window.viewport_size();
        let from_size = self.viewer_box.current_size(viewport);
        let target_details_width = self.viewer_panel.to_width;
        self.select_photo(self.quick_look_photo_ids[next], cx);
        let new_aspect = self
            .selected_photo
            .as_ref()
            .map(|photo| {
                let (width, height) = photo.effective_dimensions();
                if height > 0 {
                    width as f32 / height as f32
                } else {
                    16.0 / 10.0
                }
            })
            .unwrap_or(16.0 / 10.0);
        let now = Instant::now();
        self.viewer_nav_epoch = self.viewer_nav_epoch.wrapping_add(1);
        self.viewer_nav_direction = if delta > 0 { 1.0 } else { -1.0 };
        self.viewer_nav_started_at = now;
        self.viewer_box = ViewerBoxState {
            epoch: self.viewer_box.epoch.wrapping_add(1),
            started_at: now,
            from_size,
            to_aspect: new_aspect,
            to_details_width: target_details_width,
            duration: MOTION_BASE,
        };
        cx.notify();
    }

    fn toggle_quick_look_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .viewer_morph
            .as_ref()
            .is_some_and(|morph| !morph.closing)
        {
            self.viewer_morph = None;
        }
        let current_width = self.viewer_panel.current_width();
        let viewport = window.viewport_size();
        let from_size = self.viewer_box.current_size(viewport);
        self.quick_look_details_open = !self.quick_look_details_open;
        if !self.quick_look_details_open {
            self.note_edit_id = None;
        }
        let target_width = if self.quick_look_details_open {
            VIEWER_DETAILS_WIDTH
        } else {
            0.0
        };
        let aspect = self
            .selected_photo
            .as_ref()
            .map(|photo| {
                let (width, height) = photo.effective_dimensions();
                if height > 0 {
                    width as f32 / height as f32
                } else {
                    16.0 / 10.0
                }
            })
            .unwrap_or(16.0 / 10.0);
        let now = Instant::now();
        self.viewer_panel = ViewerPanelState {
            epoch: self.viewer_panel.epoch.wrapping_add(1),
            started_at: now,
            from_width: current_width,
            to_width: target_width,
            duration: MOTION_SLOW,
        };
        self.viewer_box = ViewerBoxState {
            epoch: self.viewer_box.epoch.wrapping_add(1),
            started_at: now,
            from_size,
            to_aspect: aspect,
            to_details_width: target_width,
            duration: MOTION_SLOW,
        };
        self.quick_look_details_epoch = self.quick_look_details_epoch.wrapping_add(1);
        let index = self
            .quick_look_focus_order(cx)
            .iter()
            .position(|handle| *handle == self.quick_look_details_focus)
            .unwrap_or(0);
        self.quick_look_focus_index = index;
        window.focus(&self.quick_look_details_focus, cx);
        cx.notify();
    }

    fn quick_look_focus_order(&self, cx: &gpui_kit::App) -> Vec<FocusHandle> {
        let index = self
            .quick_look_index
            .min(self.quick_look_photo_ids.len().saturating_sub(1));
        let mut order = vec![self.quick_look_close_focus.clone()];
        if index > 0 {
            order.push(self.quick_look_prev_focus.clone());
        }
        if index + 1 < self.quick_look_photo_ids.len() {
            order.push(self.quick_look_next_focus.clone());
        }
        order.push(self.quick_look_details_focus.clone());
        if self.quick_look_details_open {
            order.push(self.quick_look_favorite_focus.clone());
            order.push(self.tag_edit_input.read(cx).focus_handle(cx));
            order.push(self.quick_look_add_tag_focus.clone());
            if self.note_edit_id.is_some() {
                order.push(self.note_edit_input.read(cx).focus_handle(cx));
                order.push(self.quick_look_note_save_focus.clone());
                order.push(self.quick_look_note_cancel_focus.clone());
            } else {
                order.push(self.quick_look_note_edit_focus.clone());
            }
            order.push(self.quick_look_compress_focus.clone());
            order.push(self.quick_look_trash_focus.clone());
        }
        order
    }

    fn cycle_quick_look_focus(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let order = self.quick_look_focus_order(cx);
        if order.is_empty() {
            return;
        }
        // Clicks move focus without touching `quick_look_focus_index`, so
        // derive the position from the actual focus before stepping.
        let current = window
            .focused(cx)
            .and_then(|focused| order.iter().position(|handle| *handle == focused))
            .unwrap_or(self.quick_look_focus_index);
        let next = next_focus_index(current, order.len(), reverse);
        self.quick_look_focus_index = next;
        window.focus(&order[next], cx);
    }

    fn trash_selected(&mut self, cx: &mut Context<Self>) {
        let Some(photo) = &self.selected_photo else {
            return;
        };
        let id = photo.id;
        let effective_path = photo.effective_path().to_string();
        let replaced = photo.is_replaced();

        if self.delete_armed != Some(id) {
            self.delete_armed = Some(id);
            if !replaced
                && let Ok(derivatives) = self.db.derivatives_for_photo(id)
                && derivatives
                    .iter()
                    .any(|derivative| Path::new(&derivative.path).is_file())
            {
                self.message = Some(
                    "もう一度押すと元画像を削除し、圧縮版をライブラリ表示に使います".to_string(),
                );
            }
            cx.notify();
            return;
        }

        // A replaced photo's "original" is already gone; the effective path is
        // the compressed derivative that the card is currently displaying.
        let derivative_to_promote = if replaced {
            None
        } else {
            self.db
                .derivatives_for_photo(id)
                .ok()
                .and_then(|derivatives| {
                    derivatives
                        .into_iter()
                        .find(|derivative| Path::new(&derivative.path).is_file())
                })
        };

        if let Some(derivative) = &derivative_to_promote
            && let Err(error) = self.db.set_replaced(id, derivative.id)
        {
            self.delete_armed = None;
            self.message = Some(format!("圧縮版を表示に設定できません: {error}"));
            cx.notify();
            return;
        }

        match trash::delete(&effective_path) {
            Ok(_) => {
                if derivative_to_promote.is_some() {
                    // The row stays visible with the compressed derivative as
                    // its display asset.
                    self.delete_armed = None;
                    let _ = self.db.bump_library_revision();
                    self.reload_library(true, cx);
                    self.message =
                        Some("元画像をゴミ箱へ移動しました（圧縮版を表示中）".to_string());
                } else {
                    if let Err(error) = self.db.mark_missing(id) {
                        tracing::warn!(%error, photo_id = id, "cannot mark trashed photo missing");
                    }
                    let _ = self.db.clear_display_derivative(id);
                    let _ = self.db.bump_library_revision();
                    self.library.remove_photo(id);
                    self.search_view.remove_photo(id);
                    self.thumbnails.unpin(id);
                    self.selected = None;
                    self.selected_ids.clear();
                    self.selection_anchor = None;
                    self.selected_photo = None;
                    self.selected_players.clear();
                    self.selected_tags.clear();
                    self.selected_derivatives.clear();
                    self.preview = None;
                    self.delete_armed = None;
                    self.message = Some("写真をゴミ箱へ移動しました".to_string());
                    self.reload_aux(cx);
                }
            }
            Err(error) => {
                if derivative_to_promote.is_some() {
                    let _ = self.db.restore_present(id);
                }
                self.delete_armed = None;
                self.message = Some(format!("ゴミ箱へ移動できません: {error}"));
            }
        }
        cx.notify();
    }

    fn open_manual_compression(&mut self, photo_ids: Vec<i64>, cx: &mut Context<Self>) {
        let mut ids = Vec::new();
        for id in photo_ids {
            if ids.contains(&id) {
                continue;
            }
            if self.db.photo_by_id(id).ok().flatten().is_some() {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            self.message = Some("先に写真を選択してください".to_string());
            cx.notify();
            return;
        }
        let Some(profile) = self
            .profiles
            .get(self.profile_index)
            .or_else(|| self.profiles.first())
        else {
            self.message = Some("圧縮プロファイルがありません".to_string());
            cx.notify();
            return;
        };
        self.manual_compress = Some(ManualCompressionDialog {
            photo_ids: ids,
            profile_id: profile.id.clone(),
            output: ManualOutputTarget::Profile,
            folder: None,
            keep_original: true,
        });
        cx.notify();
    }

    fn close_manual_compression(&mut self, cx: &mut Context<Self>) {
        self.manual_compress = None;
        cx.notify();
    }

    fn choose_manual_folder(&mut self, cx: &mut Context<Self>) {
        let current = self
            .manual_compress
            .as_ref()
            .and_then(|dialog| dialog.folder.clone());
        let mut picker = rfd::FileDialog::new();
        if let Some(dir) = current {
            picker = picker.set_directory(dir);
        }
        if let Some(dir) = picker.pick_folder()
            && let Some(dialog) = &mut self.manual_compress
        {
            dialog.folder = Some(dir.to_string_lossy().to_string());
            dialog.output = ManualOutputTarget::Folder;
        }
        cx.notify();
    }

    fn confirm_manual_compression(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.manual_compress.clone() else {
            return;
        };
        let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| profile.id == dialog.profile_id)
            .cloned()
        else {
            self.message = Some("選択したプロファイルが見つかりません".to_string());
            cx.notify();
            return;
        };
        if !manual_output_available(dialog.photo_ids.len(), dialog.output) {
            self.message = Some("クリップボード出力は1枚選択時のみ使えます".to_string());
            cx.notify();
            return;
        }

        if dialog.output == ManualOutputTarget::Clipboard {
            let photo_id = dialog.photo_ids[0];
            let Some(photo) = self.db.photo_by_id(photo_id).ok().flatten() else {
                self.manual_compress = None;
                self.message = Some("写真が見つかりません".to_string());
                cx.notify();
                return;
            };
            let source = PathBuf::from(photo.effective_path());
            self.manual_compress = None;
            self.message = Some("クリップボードへコピーしています…".to_string());
            cx.notify();
            cx.spawn(async move |this, cx| {
                let bytes = cx
                    .background_executor()
                    .spawn(async move { compressor::render_clipboard_png(&source, &profile) })
                    .await;
                this.update(cx, |this, cx| {
                    match bytes {
                        Ok(bytes) => {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_image(
                                &gpui_kit::Image::from_bytes(gpui_kit::ImageFormat::Png, bytes),
                            ));
                            this.message =
                                Some("圧縮画像をクリップボードへコピーしました".to_string());
                        }
                        Err(error) => {
                            this.message =
                                Some(format!("クリップボードへコピーできません: {error}"));
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }

        let output_dir = if dialog.output == ManualOutputTarget::Folder {
            let Some(folder) = dialog.folder.as_deref() else {
                self.message = Some("出力フォルダを選択してください".to_string());
                cx.notify();
                return;
            };
            Some(folder)
        } else {
            None
        };
        let policy = compression_policy(dialog.keep_original);
        let mut enqueued = 0usize;
        let mut failed = 0usize;
        for photo_id in &dialog.photo_ids {
            match self.db.enqueue_job_for_existing_profile_with_output_dir(
                *photo_id,
                &profile.id,
                policy,
                output_dir,
            ) {
                Ok(true) => enqueued += 1,
                Ok(false) => failed += 1,
                Err(error) => {
                    failed += 1;
                    tracing::warn!(%error, photo_id, "cannot enqueue manual compression");
                }
            }
        }
        self.manual_compress = None;
        if enqueued > 0 {
            let policy_note = if dialog.keep_original {
                "元画像を保持"
            } else {
                "成功後に元画像をゴミ箱へ移動"
            };
            let failure_note = if failed > 0 {
                format!("（{failed} 件は追加できませんでした）")
            } else {
                String::new()
            };
            self.message = Some(format!(
                "{enqueued} 件を圧縮キューに追加しました（{policy_note}）{failure_note}"
            ));
            self.reload_aux(cx);
            self.send_wake_jobs(cx);
        } else {
            self.message = Some("圧縮ジョブを追加できませんでした".to_string());
        }
        cx.notify();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        // One 100k text query at a time; the Search button is disabled while a
        // request is in flight, but the favorite switch and presets also call
        // this method.
        if self.searching {
            return;
        }
        if !self.selected_ids.is_empty() {
            self.clear_selection(cx);
        }
        let world = self.world_input.read(cx).value().to_string();
        let player = self.player_input.read(cx).value().to_string();
        let tag = self.tag_input.read(cx).value().to_string();
        let from_utc = self
            .search_days
            .map(|days| Utc::now().timestamp() - days * 86_400);
        let filter = PhotoTextFilter {
            world: Some(world),
            player: Some(player),
            tag: Some(tag),
            favorite: self.search_favorite,
            from_utc,
            to_utc: None,
        };
        self.applied_search = Some(filter.clone());
        self.pending_search_anchor = None;
        self.pending_layout_anchor_search = None;
        self.search_refresh_pending = false;
        self.start_search(filter, cx);
    }

    fn start_search(&mut self, filter: PhotoTextFilter, cx: &mut Context<Self>) {
        self.pending_layout_anchor_search = None;
        self.search_request = self.search_request.wrapping_add(1);
        let generation = self.search_request;

        let Some(path) = self.db.path().map(Path::to_path_buf) else {
            // In-memory demo/fallback databases have no second handle to open.
            let result = self.db.photo_index_text(&filter);
            self.apply_search_result(generation, result, cx);
            return;
        };

        self.searching = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let background_filter = filter.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let search_start = Instant::now();
                    let result = Db::open_read_only(path)
                        .and_then(|db| db.photo_index_text(&background_filter));
                    log_timing("search photo_index_text", search_start);
                    result
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.search_request {
                    return;
                }
                match result {
                    Ok(entries) => this.apply_search_result(generation, Ok(entries), cx),
                    // A read-only handle can fail on a locked or WAL-only
                    // database. Query the live connection on the UI thread
                    // rather than leaving the spinner up.
                    Err(_) => {
                        let fallback = this.db.photo_index_text(&filter);
                        this.apply_search_result(generation, fallback, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Applies a completed search to the virtual grid. A stale generation (the
    /// user searched again or cleared the view) is discarded.
    fn apply_search_result(
        &mut self,
        generation: u64,
        result: vrc_photo_core::error::Result<Vec<PhotoIndexEntry>>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.search_request {
            return;
        }
        self.pending_layout_anchor_search = None;
        match result {
            Ok(entries) => {
                self.search_view
                    .replace_index(Arc::new(PhotoIndex::new(entries)));
                let requested_anchor = self.pending_search_anchor.take();
                let anchor = search_anchor_after_result(requested_anchor, &self.search_view.index);
                let columns = self.search_view.columns.max(1);
                let row = anchor.and_then(|id| self.search_view.index.row_of_id(id));
                match row {
                    Some(row) => self
                        .search_view
                        .scroll
                        .scroll_to_item_strict(row / columns, ScrollStrategy::Top),
                    None => self
                        .search_view
                        .scroll
                        .scroll_to_item_strict(0, ScrollStrategy::Top),
                }
                self.search_active = true;
                self.searching = false;
                // A previous failure must not linger above a fresh result.
                self.message = None;
                if self.search_refresh_pending {
                    self.search_refresh_pending = false;
                    self.start_search_refresh(anchor, cx);
                }
            }
            Err(error) => {
                self.pending_search_anchor = None;
                self.search_refresh_pending = false;
                self.searching = false;
                self.message = Some(format!("検索に失敗しました: {error}"));
            }
        }
        cx.notify();
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        if !self.selected_ids.is_empty() {
            self.clear_selection(cx);
        }
        self.search_active = false;
        self.searching = false;
        self.applied_search = None;
        self.pending_search_anchor = None;
        self.pending_layout_anchor_search = None;
        self.search_refresh_pending = false;
        self.message = None;
        // Invalidate an in-flight request so it cannot revive the cleared view.
        self.search_request = self.search_request.wrapping_add(1);
        let index = self.library.index.clone();
        self.search_view.replace_index(index);
        self.search_view
            .scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn refresh_search_after_tag_edit(&mut self, cx: &mut Context<Self>) {
        if !self.search_active {
            return;
        }
        let Some(filter) = self.applied_search.clone() else {
            return;
        };
        if !has_tag_filter(&filter) {
            return;
        }
        if self.searching {
            // The in-flight query may have started before the tag write.
            // Apply its result first, then refresh from the new state.
            self.search_refresh_pending = true;
            return;
        }
        let anchor = anchor_photo_id(
            &self.search_view.index,
            list_scroll_top(&self.search_view.scroll),
            self.search_view.columns,
            self.card_metrics().row_height,
        );
        self.start_search_refresh(anchor, cx);
    }

    fn start_search_refresh(&mut self, anchor: Option<i64>, cx: &mut Context<Self>) {
        let Some(filter) = self.applied_search.clone() else {
            return;
        };
        if !has_tag_filter(&filter) {
            return;
        }
        self.pending_search_anchor = anchor;
        self.start_search(filter, cx);
    }

    fn commit_tags(&mut self, photo_id: i64, tags: Vec<String>, cx: &mut Context<Self>) {
        if let Err(error) = self.db.set_photo_tags(photo_id, &tags) {
            tracing::warn!(%error, photo_id, "cannot save photo tags");
            self.message = Some(format!("タグを保存できません: {error}"));
            cx.notify();
            return;
        }
        self.load_selected_meta(photo_id);
        // A successful write is authoritative even if the metadata read failed.
        if self.selected_tags.is_empty() && !tags.is_empty() {
            self.selected_tags = tags;
        }
        self.refresh_search_after_tag_edit(cx);
        cx.notify();
    }

    fn add_tag(&mut self, photo_id: i64, tag: String, cx: &mut Context<Self>) {
        let tags = match self.db.photo_tags(photo_id) {
            Ok(tags) => tags,
            Err(error) => {
                tracing::warn!(%error, photo_id, "cannot load photo tags");
                self.message = Some(format!("タグを読み込めません: {error}"));
                cx.notify();
                return;
            }
        };
        let Some(tags) = with_tag(&tags, &tag) else {
            return;
        };
        self.commit_tags(photo_id, tags, cx);
    }

    fn remove_tag(&mut self, photo_id: i64, tag: &str, cx: &mut Context<Self>) {
        let tags = match self.db.photo_tags(photo_id) {
            Ok(tags) => tags,
            Err(error) => {
                tracing::warn!(%error, photo_id, "cannot load photo tags");
                self.message = Some(format!("タグを読み込めません: {error}"));
                cx.notify();
                return;
            }
        };
        let Some(tags) = without_tag(&tags, tag) else {
            return;
        };
        self.commit_tags(photo_id, tags, cx);
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let update_badge = self.update.nav_badge();
        let epoch = self.sidebar_motion_epoch;
        let reveal_duration = if epoch == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(320)
        };
        let photo_count = format_count(self.stats.photo_count);
        let compression_count =
            (self.active_job_count > 0).then(|| format_count(self.active_job_count));
        let status_card = (!collapsed).then(|| self.sidebar_status_card(cx));

        let brand_text = v_flex()
            .min_w_0()
            .gap(px(0.))
            .overflow_hidden()
            .whitespace_nowrap()
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child("VRC Photo"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(format!("Manager · {photo_count}枚")),
            )
            .with_animation(
                SharedString::from(format!("sidebar-brand-{epoch}")),
                Animation::new(reveal_duration)
                    .with_easing(ease_motion)
                    .with_max_fps(60.0),
                move |el, t| {
                    el.w(px(sidebar_reveal_width(collapsed, t)))
                        .opacity(if collapsed { 1.0 - t } else { t })
                },
            );

        let body = v_flex()
            .relative()
            .flex_shrink_0()
            .h_full()
            .py(px(16.))
            .px(px(12.))
            .gap(px(3.))
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgba(0xffffff14))
            .child(
                h_flex()
                    .h(px(34.))
                    .px(px(2.))
                    .gap(px(10.))
                    .items_center()
                    .when(collapsed, |this| this.justify_center().gap(px(0.)))
                    .child(
                        div()
                            .size(px(34.))
                            .rounded(px(10.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(linear_gradient(
                                135.,
                                linear_color_stop(rgb(BLUE), 0.),
                                linear_color_stop(rgb(PURPLE), 1.),
                            ))
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(rgb(0xffffff))
                            .child("VP"),
                    )
                    .child(brand_text),
            )
            .child(div().h(px(12.)))
            .child(
                v_flex()
                    .relative()
                    .w_full()
                    .gap(px(NAV_ITEM_GAP))
                    .child(self.nav_highlight())
                    .child(self.nav_item(
                        "写真",
                        Screen::Photos,
                        IconName::GalleryVerticalEnd,
                        Some(photo_count.clone()),
                        cx,
                    ))
                    .child(self.nav_item("検索", Screen::Search, IconName::Search, None, cx))
                    .child(self.nav_item(
                        "圧縮",
                        Screen::Compression,
                        IconName::Minimize,
                        compression_count,
                        cx,
                    ))
                    .child(self.nav_item(
                        "ダッシュボード",
                        Screen::Dashboard,
                        IconName::LayoutDashboard,
                        None,
                        cx,
                    ))
                    .child(self.nav_item(
                        "設定",
                        Screen::Settings,
                        IconName::Settings,
                        update_badge,
                        cx,
                    )),
            )
            .child(div().flex_1())
            .when_some(status_card, |this, card| this.child(card))
            .when(!collapsed, |this| {
                this.child(
                    v_flex()
                        .p(px(11.))
                        .gap(px(3.))
                        .rounded(px(13.))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgba(0xffffff0f))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED_2))
                                .child("ライブラリ"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_semibold()
                                .text_color(rgb(TEXT))
                                .child(format!("{photo_count} 枚")),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(format_bytes(self.stats.total_bytes)),
                        ),
                )
            })
            .child(
                h_flex()
                    .h(px(26.))
                    .gap(px(7.))
                    .items_center()
                    .when(collapsed, |this| this.justify_center())
                    .when(!collapsed, |this| this.px(px(9.)))
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(rgb(if !self.daemon_active {
                                MUTED_2
                            } else if self.daemon_paused {
                                AMBER
                            } else {
                                GREEN
                            })),
                    )
                    .when(!collapsed, |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(if self.daemon_active { MUTED } else { MUTED_2 }))
                                .child(if !self.daemon_active {
                                    "デーモン停止"
                                } else if self.daemon_paused {
                                    "一時停止中"
                                } else {
                                    "監視中"
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(if collapsed { 9. } else { 10. }))
                    .text_color(rgb(MUTED_2))
                    .child(if self.demo_assets.is_some() {
                        "DEMO DATA".to_string()
                    } else {
                        format!("v{}", env!("CARGO_PKG_VERSION"))
                    }),
            );

        body.with_animation(
            ("sidebar", epoch),
            Animation::new(if epoch == 0 {
                Duration::ZERO
            } else {
                Duration::from_millis(440)
            })
            .with_easing(ease_motion)
            .with_max_fps(60.0),
            move |el, t| {
                let from = sidebar_width(!collapsed);
                let to = sidebar_width(collapsed);
                el.w(px(from + (to - from) * t))
            },
        )
        .into_any_element()
    }

    fn sidebar_status_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let import_progress = import_running_progress(self.import_dialog.as_ref());
        let compression = compression_card_status(
            self.active_job_count,
            self.waiting_job_count,
            self.failed_job_count,
        );
        let import = import_card_status(import_progress, self.import_report.as_ref());
        let scan = scan_card_status(
            self.scan_progress.as_ref(),
            self.daemon_active,
            self.daemon_paused,
        );
        let compression_attention = compression_attention(
            self.active_job_count,
            self.waiting_job_count,
            self.failed_job_count,
        );
        let import_attention = import_attention(import_progress, self.import_report.as_ref());
        let scan_attention = scan_attention(
            self.scan_progress.as_ref(),
            self.daemon_active,
            self.daemon_paused,
        );
        let tab = self.sidebar_status_tab;
        let (status, ratio) = match tab {
            SidebarStatusTab::Compression => (&compression, None),
            SidebarStatusTab::Import => (&import, import_progress.map(import_progress_ratio)),
            SidebarStatusTab::Scan => (
                &scan,
                self.scan_progress
                    .as_ref()
                    .map(scan_progress_ratio)
                    .filter(|ratio| *ratio > 0.0),
            ),
        };
        let import_has_report = self.import_report.is_some();

        let tabs = [
            (
                SidebarStatusTab::Compression,
                "圧縮",
                "sidebar-status-tab-compression",
                compression_attention,
            ),
            (
                SidebarStatusTab::Import,
                "取り込み",
                "sidebar-status-tab-import",
                import_attention,
            ),
            (
                SidebarStatusTab::Scan,
                "スキャン",
                "sidebar-status-tab-scan",
                scan_attention,
            ),
        ];
        let tab_row =
            h_flex()
                .gap(px(4.))
                .children(tabs.into_iter().map(|(tab, label, id, attention)| {
                    let active = self.sidebar_status_tab == tab;
                    h_flex()
                        .id(id)
                        .relative()
                        .flex_1()
                        .h(px(20.))
                        .justify_center()
                        .items_center()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(if active {
                            rgba(0x66b3ff33)
                        } else {
                            rgba(0x00000000)
                        })
                        .text_size(px(10.))
                        .font_medium()
                        .cursor_pointer()
                        .when(active, |this| {
                            this.bg(rgb(ACCENT_SOFT)).text_color(rgb(BLUE))
                        })
                        .when(!active, |this| {
                            this.text_color(rgb(MUTED_2))
                                .hover(|this| this.bg(rgb(PANEL_2)).text_color(rgb(TEXT)))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.sidebar_status_tab = tab;
                            cx.notify();
                        }))
                        .when_some(attention, |this, tone| {
                            this.child(
                                div()
                                    .absolute()
                                    .top(px(3.))
                                    .right(px(4.))
                                    .size(px(4.))
                                    .rounded_full()
                                    .bg(rgb(tone.color())),
                            )
                        })
                        .child(label)
                }));

        let mut body = v_flex()
            .h(px(46.))
            .justify_center()
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(status.tone.color()))
                    .child(status.headline.clone()),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .truncate()
                    .child(status.detail.clone()),
            );
        if let Some(ratio) = ratio {
            body = body.child(status_progress_bar(ratio));
        }

        let footer =
            match tab {
                SidebarStatusTab::Compression => {
                    let mut row = h_flex().items_center().h(px(24.)).gap(px(6.));
                    if self.waiting_job_count > 0 {
                        row = row.child(
                            Button::new("sidebar-status-release-waiting")
                                .label("未実行を開始")
                                .small()
                                .primary()
                                .tooltip("未実行のジョブをすべて圧縮します")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.request_release_waiting_jobs(window, cx)
                                })),
                        );
                    }
                    if self.failed_job_count > 0 {
                        row = row.child(
                            Button::new("sidebar-status-retry-failed")
                                .label("再試行")
                                .small()
                                .ghost()
                                .tooltip("上限内の失敗ジョブを再試行します")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.retry_all_failed_jobs(cx)),
                                ),
                        );
                    }
                    if self.waiting_job_count == 0 && self.failed_job_count == 0 {
                        row = row.child(
                            Button::new("sidebar-status-open-queue")
                                .label("キューを開く")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.switch_screen(Screen::Dashboard, cx)
                                })),
                        );
                    }
                    row
                }
                SidebarStatusTab::Import => {
                    h_flex()
                        .items_center()
                        .h(px(24.))
                        .gap(px(6.))
                        .child(if import_has_report {
                            Button::new("sidebar-status-import-result")
                                .label("結果を見る")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.switch_screen(Screen::Photos, cx)
                                }))
                        } else {
                            Button::new("sidebar-status-photos")
                                .label("写真を開く")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.switch_screen(Screen::Photos, cx)
                                }))
                        })
                }
                SidebarStatusTab::Scan => {
                    let can_rescan =
                        self.scan_progress.is_none() && self.daemon_active && !self.daemon_paused;
                    let button = if can_rescan {
                        Button::new("sidebar-status-rescan")
                            .label("再スキャン")
                            .small()
                            .ghost()
                            .tooltip("デーモンに再スキャンを要求します")
                            .on_click(cx.listener(|this, _, _, cx| this.request_daemon_rescan(cx)))
                    } else {
                        Button::new("sidebar-status-dashboard")
                            .label("ダッシュボード")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.switch_screen(Screen::Dashboard, cx)
                            }))
                    };
                    h_flex().items_center().h(px(24.)).gap(px(6.)).child(button)
                }
            };

        v_flex()
            .flex_shrink_0()
            .p(px(9.))
            .gap(px(4.))
            .rounded(px(13.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgba(0xffffff0f))
            .child(
                h_flex()
                    .items_center()
                    .h(px(12.))
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .font_semibold()
                            .text_color(rgb(MUTED_2))
                            .child("進行状況"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(rgb(status.tone.color())),
                    ),
            )
            .child(tab_row)
            .child(body)
            .child(footer)
            .into_any_element()
    }

    fn nav_highlight(&self) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let nav_epoch = self.nav_epoch;
        let from = nav_item_top(nav_index(self.nav_previous));
        let to = nav_item_top(nav_index(self.screen));
        let vertical_duration = if nav_epoch == 0 {
            Duration::ZERO
        } else {
            MOTION_BASE
        };
        let vertical_anim = || {
            Animation::new(vertical_duration)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS)
        };

        let pill = div()
            .absolute()
            .left_0()
            .right_0()
            .h(px(NAV_ITEM_HEIGHT))
            .rounded(px(11.))
            .bg(rgb(ACCENT_SOFT))
            .with_animation(("nav-pill", nav_epoch), vertical_anim(), move |el, t| {
                el.top(px(from + (to - from) * t))
            });

        let sidebar_epoch = self.sidebar_motion_epoch;
        let from_left = nav_bar_left(!collapsed);
        let to_left = nav_bar_left(collapsed);
        let horizontal_anim = Animation::new(if sidebar_epoch == 0 {
            Duration::ZERO
        } else {
            MOTION_SLOW
        })
        .with_easing(ease_motion)
        .with_max_fps(MOTION_FPS);
        let bar = div()
            .absolute()
            .left_0()
            .top_0()
            .w(px(2.))
            .h_full()
            .with_animation(
                ("nav-bar-x", sidebar_epoch),
                horizontal_anim,
                move |el, t| el.left(px(from_left + (to_left - from_left) * t)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(from + 11.0))
                    .left_0()
                    .w(px(2.))
                    .h(px(18.))
                    .rounded_full()
                    .bg(rgb(BLUE))
                    .with_animation(("nav-bar-y", nav_epoch), vertical_anim(), move |el, t| {
                        el.top(px(from + 11.0 + (to - from) * t))
                    }),
            );

        div()
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h(px(5.0 * NAV_ITEM_HEIGHT + 4.0 * NAV_ITEM_GAP))
            .child(pill)
            .child(bar)
            .into_any_element()
    }

    fn nav_item(
        &mut self,
        label: &'static str,
        screen: Screen,
        icon: IconName,
        count: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.screen == screen;
        let was_active = self.nav_previous == screen;
        let hovered = self.hovered_nav == Some(screen);
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let nav_epoch = self.nav_epoch;
        let reveal_duration = if epoch == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(320)
        };
        let color_anim = || {
            Animation::new(MOTION_FAST)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS)
        };

        let icon_element = Icon::new(icon).with_size(px(17.)).with_animation(
            SharedString::from(format!("nav-icon-{label}-{nav_epoch}")),
            color_anim(),
            move |el, t| {
                if hovered && !active {
                    return el.text_color(rgb(TEXT));
                }
                let from = if was_active { BLUE } else { MUTED };
                let to = if active { BLUE } else { MUTED };
                el.text_color(rgb(lerp_color(from, to, t)))
            },
        );

        let label_from = if was_active { 0xeaf4ff } else { MUTED };
        let label_to = if active {
            0xeaf4ff
        } else if hovered {
            TEXT
        } else {
            MUTED
        };
        let label_block = h_flex()
            .min_w_0()
            .items_center()
            .justify_between()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(div().text_size(px(12.)).child(label).with_animation(
                SharedString::from(format!("nav-label-{label}-{nav_epoch}")),
                color_anim(),
                move |el, t| el.text_color(rgb(lerp_color(label_from, label_to, t))),
            ))
            .when_some(count, |this, count| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .font_family("monospace")
                        .child(count)
                        .with_animation(
                            SharedString::from(format!("nav-count-{label}-{nav_epoch}")),
                            color_anim(),
                            move |el, t| {
                                if hovered && !active {
                                    return el.text_color(rgb(TEXT));
                                }
                                let from = if was_active { BLUE } else { MUTED_2 };
                                let to = if active { BLUE } else { MUTED_2 };
                                el.text_color(rgb(lerp_color(from, to, t)))
                            },
                        ),
                )
            })
            .with_animation(
                SharedString::from(format!("sidebar-label-{label}-{epoch}")),
                Animation::new(reveal_duration)
                    .with_easing(ease_motion)
                    .with_max_fps(60.0),
                move |el, t| {
                    el.w(px(sidebar_reveal_width(collapsed, t)))
                        .opacity(if collapsed { 1.0 - t } else { t })
                },
            );

        let from_pad = if collapsed { 11.0 } else { 18.5 };
        let to_pad = if collapsed { 18.5 } else { 11.0 };
        let from_gap = if collapsed { 11.0 } else { 0.0 };
        let to_gap = if collapsed { 0.0 } else { 11.0 };
        let pad_anim = Animation::new(if epoch == 0 {
            Duration::ZERO
        } else {
            MOTION_SLOW
        })
        .with_easing(ease_motion)
        .with_max_fps(MOTION_FPS);
        let content = h_flex()
            .w_full()
            .items_center()
            .with_animation(
                SharedString::from(format!("nav-pad-{label}-{epoch}")),
                pad_anim,
                move |el, t| {
                    el.pl(px(from_pad + (to_pad - from_pad) * t))
                        .gap(px(from_gap + (to_gap - from_gap) * t))
                },
            )
            .child(icon_element)
            .child(label_block);

        h_flex()
            .id(label)
            .relative()
            .h(px(NAV_ITEM_HEIGHT))
            .w_full()
            .rounded(px(11.))
            .cursor_pointer()
            .items_center()
            .when(!active, |this| {
                this.hover(|this| this.bg(rgb(0x161d28)).text_color(rgb(TEXT)))
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                // Hover listeners fire in reverse paint order. Let the entering
                // row win instead of letting the leaving row clear it.
                if *hovered {
                    if this.hovered_nav != Some(screen) {
                        this.hovered_nav = Some(screen);
                        cx.notify();
                    }
                } else if this.hovered_nav == Some(screen) {
                    this.hovered_nav = None;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.switch_screen(screen, cx);
            }))
            .child(content)
            .into_any_element()
    }

    fn nav_tooltip(&self) -> AnyElement {
        let Some(screen) = self.hovered_nav.filter(|_| self.sidebar_collapsed) else {
            return div().into_any_element();
        };
        let index = nav_index(screen);

        div()
            .absolute()
            .left(px(86.))
            .top(px(70.0 + nav_item_top(index)))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(7.))
            .bg(rgb(0x161c26))
            .border_1()
            .border_color(rgb(BORDER_STRONG))
            .text_size(px(10.))
            .text_color(rgb(TEXT))
            .whitespace_nowrap()
            .child(screen.title())
            .with_animation(
                ("nav-tooltip", index),
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                move |el, t| el.opacity(t).relative().left(px(-4.0 * (1.0 - t))),
            )
            .into_any_element()
    }

    fn rail_toggle(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let icon = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronLeft
        };
        let tooltip = if collapsed {
            "サイドバーを展開 (Ctrl+B)"
        } else {
            "サイドバーを最小化 (Ctrl+B)"
        };

        div()
            .absolute()
            .top(px(18.))
            .left(px(sidebar_width(collapsed) - 12.))
            .child(
                Button::new("rail-toggle")
                    .ghost()
                    .icon(icon)
                    .tooltip(tooltip)
                    .size(px(24.))
                    .rounded_full()
                    .bg(rgb(0x131923))
                    .border_1()
                    .border_color(rgb(BORDER_STRONG))
                    .text_color(rgb(MUTED))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
            )
            .with_animation(
                ("rail-toggle", epoch),
                Animation::new(if epoch == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(440)
                })
                .with_easing(ease_motion)
                .with_max_fps(60.0),
                move |el, t| {
                    let from = sidebar_width(!collapsed);
                    let to = sidebar_width(collapsed);
                    el.left(px(from + (to - from) * t - 12.))
                },
            )
            .into_any_element()
    }

    fn screen_header(
        &self,
        title: &'static str,
        subtitle: String,
        message: Option<String>,
        actions: Vec<AnyElement>,
    ) -> AnyElement {
        h_flex()
            .h(px(72.))
            .flex_shrink_0()
            .px(px(26.))
            .gap(px(14.))
            .items_center()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                v_flex()
                    .flex_shrink_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(24.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when_some(message, |this, message| {
                        this.text_size(px(10.))
                            .text_color(rgb(AMBER))
                            .truncate()
                            .child(message)
                    }),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(8.))
                    .children(actions),
            )
            .into_any_element()
    }

    /// One compact row per failure domain. Detail text stays on the target
    /// screen so the bar never truncates the actual reason.
    fn global_alerts(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let import_failed = self
            .import_report
            .as_ref()
            .map(|report| import_issue_counts(&report.issues).0)
            .unwrap_or(0);
        alert_summaries(
            self.failed_job_count,
            self.daemon_errors.len(),
            import_failed,
        )
        .into_iter()
        .enumerate()
        .map(|(index, (target, message))| {
            let (id, action) = match target {
                Screen::Photos => ("alert-photos", "写真を開く"),
                Screen::Compression => ("alert-compression", "圧縮を開く"),
                Screen::Search => ("alert-search", "検索を開く"),
                Screen::Dashboard => ("alert-dashboard", "ダッシュボードを開く"),
                Screen::Settings => ("alert-settings", "設定を開く"),
            };
            h_flex()
                .flex_shrink_0()
                .w_full()
                .items_center()
                .gap(px(10.))
                .px(px(26.))
                .py(px(8.))
                .bg(rgb(PANEL))
                .border_b_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(rgb(RED)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(rgb(RED))
                        .child(message),
                )
                .child(
                    Button::new(id)
                        .label(action)
                        .small()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.switch_screen(target, cx);
                        })),
                )
                .with_animation(
                    ("global-alert", index),
                    Animation::new(MOTION_BASE)
                        .with_easing(ease_motion)
                        .with_max_fps(MOTION_FPS),
                    |el, t| el.opacity(t).relative().top(px(-8.0 * (1.0 - t))),
                )
                .into_any_element()
        })
        .collect()
    }

    fn content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let screen = match self.screen {
            Screen::Photos => self.photos_screen(window, cx).into_any_element(),
            Screen::Compression => self.compression_screen(window, cx).into_any_element(),
            Screen::Search => self.search_screen(window, cx).into_any_element(),
            Screen::Dashboard => self.dashboard_screen(cx).into_any_element(),
            Screen::Settings => self.settings_screen(cx).into_any_element(),
        };
        let enter = Animation::new(if self.screen_epoch == 0 {
            Duration::ZERO
        } else {
            MOTION_BASE
        })
        .with_easing(ease_motion)
        .with_max_fps(MOTION_FPS);
        let direction = self.screen_direction;
        let screen = v_flex().flex_1().min_h_0().child(screen).with_animation(
            ("screen-enter", self.screen_epoch),
            enter,
            move |el, t| {
                el.relative()
                    .left(px(direction * SCREEN_SLIDE * (1.0 - t)))
                    .opacity(t)
            },
        );
        v_flex()
            .flex_1()
            .min_h_0()
            .children(self.global_alerts(cx))
            .child(screen)
            .into_any_element()
    }

    fn quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(photo) = self.selected_photo.clone() else {
            return div().into_any_element();
        };
        let color = self.color_for(&photo);
        let details_open = self.quick_look_details_open;
        let photo_count = self.quick_look_photo_ids.len();
        let index = self.quick_look_index.min(photo_count.saturating_sub(1));
        let prev_enabled = index > 0;
        let next_enabled = index + 1 < photo_count;

        let close_button = self.focusable_text_button(
            "viewer-close",
            "閉じる Esc",
            self.quick_look_close_focus.clone(),
            ButtonState {
                tone: ButtonTone::Primary,
                enabled: true,
            },
            cx,
            |this, window, cx| this.close_quick_look(window, cx),
        );
        let prev_button = self.focusable_text_button(
            "viewer-prev",
            "前へ",
            self.quick_look_prev_focus.clone(),
            ButtonState {
                tone: if prev_enabled {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: prev_enabled,
            },
            cx,
            |this, window, cx| this.step_viewer_photo(-1, window, cx),
        );
        let next_button = self.focusable_text_button(
            "viewer-next",
            "次へ",
            self.quick_look_next_focus.clone(),
            ButtonState {
                tone: if next_enabled {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: next_enabled,
            },
            cx,
            |this, window, cx| this.step_viewer_photo(1, window, cx),
        );
        let details_button = self.focusable_text_button(
            "viewer-details",
            if details_open {
                "詳細を閉じる"
            } else {
                "詳細"
            },
            self.quick_look_details_focus.clone(),
            ButtonState {
                tone: if details_open {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                },
                enabled: true,
            },
            cx,
            |this, window, cx| this.toggle_quick_look_details(window, cx),
        );

        let top_bar = h_flex()
            .h(px(VIEWER_TOP_BAR))
            .flex_shrink_0()
            .px(px(20.))
            .gap(px(10.))
            .items_center()
            .bg(rgba(0x05070a99))
            .border_b_1()
            .border_color(rgba(0xffffff12))
            .key_context("viewer_nav")
            .child(close_button)
            .child(prev_button)
            .child(next_button)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(photo.file_name.clone()),
            )
            .child(details_button)
            .with_animation(
                ("viewer-topbar", self.quick_look_epoch),
                Animation::new(MOTION_BASE)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                |el, t| el.relative().top(px(-8.0 * (1.0 - t))),
            );

        let viewport = window.viewport_size();
        let box_state = self.viewer_box;
        let nav_epoch = self.viewer_nav_epoch;
        let nav_direction = self.viewer_nav_direction;
        let nav_t = if nav_epoch == 0 {
            1.0
        } else {
            viewer_nav_progress(
                self.viewer_nav_started_at,
                Instant::now(),
                cx.reduce_motion(),
            )
        };
        let previous_preview = self.previous_preview.clone();
        let current_preview = self.preview.clone();
        let (start_w, start_h) = box_state.size_at(0.0, viewport);

        let mut image_frame = div()
            .id("viewer-image")
            .relative()
            .flex_shrink_0()
            .w(px(start_w))
            .h(px(start_h))
            .overflow_hidden()
            .rounded(px(10.))
            .bg(rgb(color))
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()));

        if nav_epoch > 0
            && nav_t < 1.0
            && let Some(previous) = previous_preview
        {
            image_frame = image_frame.child(
                gpui_kit::img(previous)
                    .object_fit(ObjectFit::Contain)
                    .size_full()
                    .rounded(px(10.))
                    .opacity(1.0 - nav_t)
                    .relative()
                    .left(px(-nav_direction * VIEWER_NAV_SLIDE * nav_t)),
            );
        }

        let current_offset = if nav_epoch == 0 {
            0.0
        } else {
            nav_direction * VIEWER_NAV_SLIDE * (1.0 - nav_t)
        };
        if let Some(preview) = current_preview {
            image_frame = image_frame.child(
                gpui_kit::img(preview)
                    .object_fit(ObjectFit::Contain)
                    .size_full()
                    .rounded(px(10.))
                    .opacity(if nav_epoch == 0 { 1.0 } else { nav_t })
                    .relative()
                    .left(px(current_offset)),
            );
        } else {
            image_frame = image_frame.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(preview_placeholder_label(
                        self.preview_queue.pending_for(photo.id),
                    )),
            );
        }

        let image_frame = image_frame.with_animation(
            ("viewer-box", box_state.epoch),
            Animation::new(box_state.duration)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS),
            move |el, t| {
                let (width, height) = box_state.size_at(t, viewport);
                el.w(px(width)).h(px(height))
            },
        );

        let opening_morph = self
            .viewer_morph
            .as_ref()
            .is_some_and(|morph| !morph.closing);
        let nav_driver = div().size(px(0.)).with_animation(
            ("viewer-nav-driver", nav_epoch),
            Animation::new(if nav_epoch == 0 {
                Duration::ZERO
            } else {
                MOTION_BASE
            })
            .with_easing(ease_motion)
            .with_max_fps(MOTION_FPS),
            |el, _| el,
        );
        let image_area = div()
            .id("viewer-image-area")
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .p(px(VIEWER_PAD))
            .opacity(if opening_morph { 0.0 } else { 1.0 })
            .on_click(cx.listener(|this, _, window, cx| this.close_quick_look(window, cx)))
            .child(nav_driver)
            .child(image_frame);

        let message = self.message.clone();
        let mut details_panel = v_flex()
            .h_full()
            .flex_shrink_0()
            .overflow_hidden()
            .bg(rgba(0x0e131bdc))
            .when(details_open, |this| {
                this.border_l_1().border_color(rgba(0xffffff12))
            });
        if details_open {
            let mut body = v_flex()
                .id("viewer-details-body")
                .h_full()
                .p(px(16.))
                .gap(px(10.))
                .overflow_y_scrollbar();
            if let Some(message) = message {
                body = body.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(AMBER))
                        .child(message),
                );
            }
            if self.delete_armed == Some(photo.id) {
                body = body.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(AMBER))
                        .child("もう一度押すとOSゴミ箱へ移動します。"),
                );
            }
            let body = body.child(self.detail_body(&photo, cx)).with_animation(
                ("viewer-details-body", self.quick_look_details_epoch),
                Animation::new(if self.quick_look_details_epoch == 0 {
                    Duration::ZERO
                } else {
                    MOTION_FAST
                })
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS),
                |el, t| el.opacity(t).relative().top(px(6.0 * (1.0 - t))),
            );
            details_panel = details_panel.child(body);
        }
        let panel_state = self.viewer_panel;
        let details_panel = details_panel.with_animation(
            ("viewer-panel", panel_state.epoch),
            Animation::new(panel_state.duration)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS),
            move |el, t| el.w(px(panel_state.width_at(t))).overflow_hidden(),
        );

        let content = h_flex()
            .flex_1()
            .min_h_0()
            .child(image_area)
            .child(details_panel);

        v_flex()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .bg(rgb(0x05070a))
            .key_context("quick_look")
            .occlude()
            .child(div().absolute().size_full().bg(rgba((color << 8) | 0x22)))
            .child(top_bar)
            .child(content)
            .with_animation(
                ("viewer", self.quick_look_epoch),
                Animation::new(Duration::from_millis(240)).with_easing(ease_motion),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }

    fn viewer_morph_overlay(&self) -> AnyElement {
        let Some(morph) = self.viewer_morph.as_ref() else {
            return div().into_any_element();
        };
        let from = morph.from;
        let to = morph.to;
        let epoch = morph.epoch;
        let color = morph.color;
        let closing = morph.closing;
        let fade_only = morph.fade_only;
        let backdrop_from = morph.backdrop_from;
        let (old_kind, new_kind, start_radius, end_radius) = morph_image_plan(
            closing,
            morph.source,
            morph.thumbnail.is_some(),
            morph.preview.is_some(),
        );
        let image_for = |kind: MorphImageKind| match kind {
            MorphImageKind::Thumbnail => morph.thumbnail.clone(),
            MorphImageKind::Preview => morph.preview.clone(),
        };
        let Some(old_image) = old_kind.and_then(image_for) else {
            return div().into_any_element();
        };
        let new_image = if fade_only || (!closing && morph.preview.is_none()) {
            // Never present the thumbnail as the full-size incoming image.
            // If the preview is missing, the viewer shows its explicit failure
            // state after the overlay clears.
            None
        } else {
            new_kind.and_then(image_for)
        };
        let old_fit = if closing {
            ObjectFit::Contain
        } else {
            ObjectFit::Cover
        };
        let new_fit = if closing {
            ObjectFit::Cover
        } else {
            ObjectFit::Contain
        };
        let layer_anim = || {
            Animation::new(MOTION_MORPH)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS)
        };
        let old_layer = gpui_kit::img(old_image)
            .object_fit(old_fit)
            .size_full()
            .with_animation(("viewer-morph-old", epoch), layer_anim(), move |el, t| {
                let radius = start_radius + (end_radius - start_radius) * t;
                let opacity = if fade_only { 1.0 } else { 1.0 - t };
                el.opacity(opacity).rounded(px(radius))
            });
        let new_layer = new_image.map(|image| {
            gpui_kit::img(image)
                .object_fit(new_fit)
                .size_full()
                .with_animation(("viewer-morph-new", epoch), layer_anim(), move |el, t| {
                    let radius = start_radius + (end_radius - start_radius) * t;
                    el.opacity(t).rounded(px(radius))
                })
        });
        let frame = div()
            .absolute()
            .overflow_hidden()
            .bg(rgb(color))
            .child(old_layer)
            .when(!fade_only, |this| {
                this.when_some(new_layer, |this, layer| this.child(layer))
            })
            .with_animation(
                ("viewer-morph-frame", epoch),
                Animation::new(MOTION_MORPH)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                move |el, t| {
                    let bounds = lerp_bounds(from, to, t);
                    let radius = start_radius + (end_radius - start_radius) * t;
                    el.opacity(if fade_only { 1.0 - t } else { 1.0 })
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height)
                        .rounded(px(radius))
                },
            );

        let backdrop = closing.then(|| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .size_full()
                .bg(rgb(0x05070a))
                .child(div().absolute().size_full().bg(rgba((color << 8) | 0x22)))
                .with_animation(
                    ("viewer-morph-backdrop", epoch),
                    Animation::new(MOTION_MORPH)
                        .with_easing(ease_motion)
                        .with_max_fps(MOTION_FPS),
                    move |el, t| el.opacity(backdrop_from * (1.0 - t)),
                )
        });

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .when_some(backdrop, |this, backdrop| this.child(backdrop))
            .child(frame)
            .into_any_element()
    }

    fn manual_compression_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = self.manual_compress.clone() else {
            return div().into_any_element();
        };
        let count = dialog.photo_ids.len();
        let selected_profile = self
            .profiles
            .iter()
            .find(|profile| profile.id == dialog.profile_id)
            .cloned();
        let profile_description = selected_profile
            .as_ref()
            .map(|profile| {
                profile
                    .output_dir
                    .clone()
                    .filter(|dir| !dir.trim().is_empty())
                    .unwrap_or_else(|| "プロファイル既定（元画像の隣の compressed/）".to_string())
            })
            .unwrap_or_else(|| "プロファイルを選択してください".to_string());
        let folder_ready = dialog.folder.is_some();
        let can_confirm = match dialog.output {
            ManualOutputTarget::Folder => folder_ready,
            ManualOutputTarget::Profile => selected_profile.is_some(),
            ManualOutputTarget::Clipboard => manual_output_available(count, dialog.output),
        };

        let profiles = self.profiles.clone();
        let profile_rows = profiles.into_iter().map(|profile| {
            let id = profile.id.clone();
            let selected = profile.id == dialog.profile_id;
            manual_option_row(
                SharedString::from(format!("manual-profile-{}", profile.id)),
                profile.name.clone(),
                profile_summary(&profile),
                selected,
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(dialog) = &mut this.manual_compress {
                    dialog.profile_id = id.clone();
                }
                cx.notify();
            }))
        });

        let folder_row = manual_option_row(
            "manual-output-folder",
            "指定したフォルダに出力",
            "実行のたびに出力先を選びます",
            dialog.output == ManualOutputTarget::Folder,
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(dialog) = &mut this.manual_compress {
                dialog.output = ManualOutputTarget::Folder;
            }
            cx.notify();
        }));

        let profile_row = manual_option_row(
            "manual-output-profile",
            "プロファイル指定のフォルダに出力",
            profile_description,
            dialog.output == ManualOutputTarget::Profile,
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(dialog) = &mut this.manual_compress {
                dialog.output = ManualOutputTarget::Profile;
            }
            cx.notify();
        }));

        let clipboard_enabled = manual_output_available(count, ManualOutputTarget::Clipboard);
        let clipboard_row = manual_option_row(
            "manual-output-clipboard",
            "クリップボードに出力",
            if clipboard_enabled {
                "PNG 1枚をクリップボードへコピーします（元画像は保持）"
            } else {
                "クリップボード出力は1枚選択時のみ使えます"
            },
            dialog.output == ManualOutputTarget::Clipboard,
            clipboard_enabled,
        )
        .when(clipboard_enabled, |this| {
            this.on_click(cx.listener(|this, _, _, cx| {
                if let Some(dialog) = &mut this.manual_compress {
                    dialog.output = ManualOutputTarget::Clipboard;
                }
                cx.notify();
            }))
        });

        let keep_row = manual_option_row(
            "manual-keep-original",
            "元画像を保持",
            "圧縮版を別ファイルで出力します",
            dialog.keep_original,
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(dialog) = &mut this.manual_compress {
                dialog.keep_original = true;
            }
            cx.notify();
        }));
        let trash_row = manual_option_row(
            "manual-trash-original",
            "元画像をゴミ箱へ移動",
            "圧縮に成功した場合だけOSゴミ箱へ移動します",
            !dialog.keep_original,
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(dialog) = &mut this.manual_compress {
                dialog.keep_original = false;
            }
            cx.notify();
        }));

        let folder_path = dialog
            .folder
            .clone()
            .unwrap_or_else(|| "フォルダが選択されていません".to_string());
        let confirm_label = if dialog.output == ManualOutputTarget::Clipboard {
            "クリップボードへコピー".to_string()
        } else {
            format!("{count} 件を圧縮")
        };

        let panel = v_flex()
            .id("manual-compression-panel")
            .w(px(580.))
            .max_h(px(720.))
            .gap(px(16.))
            .p(px(22.))
            .rounded(px(18.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER_STRONG))
            .shadow(vec![
                BoxShadow::new(px(0.), px(24.), rgba(0x00000099).into()).blur_radius(px(60.)),
            ])
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .overflow_y_scrollbar()
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("手動圧縮"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED_2))
                            .child(format!("{count} 枚を対象")),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("使用するプロファイル、出力先、元画像の扱いを選んでから実行します。"),
            )
            .child(self.section_label("プロファイル"))
            .child(
                v_flex()
                    .id("manual-profile-list")
                    .max_h(px(190.))
                    .overflow_y_scrollbar()
                    .gap(px(6.))
                    .children(profile_rows),
            )
            .child(self.section_label("出力先"))
            .child(v_flex().gap(px(6.)).child(folder_row).when(
                dialog.output == ManualOutputTarget::Folder,
                |this| {
                    this.child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .pl(px(24.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(10.))
                                    .text_color(rgb(if folder_ready { MUTED } else { AMBER }))
                                    .child(folder_path),
                            )
                            .child(
                                Button::new("manual-choose-folder")
                                    .label("フォルダを選択")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_manual_folder(cx)
                                    })),
                            ),
                    )
                },
            ))
            .child(profile_row)
            .child(clipboard_row)
            .when(dialog.output != ManualOutputTarget::Clipboard, |this| {
                this.child(self.section_label("元画像"))
                    .child(v_flex().gap(px(6.)).child(keep_row).child(trash_row))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("「ゴミ箱へ移動」は圧縮ファイルの検証に成功した場合だけ実行します。"),
                    )
            })
            .when(dialog.output == ManualOutputTarget::Clipboard, |this| {
                this.child(
                    div()
                        .p(px(10.))
                        .rounded(px(8.))
                        .bg(rgb(PANEL_2))
                        .text_size(px(10.))
                        .text_color(rgb(MUTED))
                        .child("クリップボード出力では元画像を変更しません。形式はPNG、リサイズ設定のみ反映します。"),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("manual-cancel")
                            .label("キャンセル")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_manual_compression(cx)
                            })),
                    )
                    .child(
                        Button::new("manual-confirm")
                            .label(confirm_label)
                            .small()
                            .primary()
                            .disabled(!can_confirm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm_manual_compression(cx)
                            })),
                    ),
            );

        let panel = panel.with_animation(
            "manual-compression-panel",
            Animation::new(MOTION_BASE)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS),
            |el, t| el.relative().top(px(10.0 * (1.0 - t))),
        );
        div()
            .id("manual-compression-scrim")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x05070acc))
            .on_click(cx.listener(|this, _, _, cx| this.close_manual_compression(cx)))
            .child(panel)
            .with_animation(
                "manual-compression-scrim",
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }

    fn import_modal_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = self.import_dialog.as_ref() else {
            return div().into_any_element();
        };
        let title = match &dialog.origin {
            ImportDialogOrigin::New => "取り込み",
            ImportDialogOrigin::Retry { .. } => "取り込み · 再試行",
            ImportDialogOrigin::Resume { .. } => "取り込み · 再開",
        };
        let mode = import_mode_label(dialog.mode);
        let dest = dialog.dest.to_string_lossy().to_string();

        let mut panel = v_flex()
            .id("import-dialog-panel")
            .w(px(560.))
            .gap(px(16.))
            .p(px(22.))
            .rounded(px(18.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER_STRONG))
            .shadow(vec![
                BoxShadow::new(px(0.), px(24.), rgba(0x00000099).into()).blur_radius(px(60.)),
            ])
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .track_focus(&self.import_dialog_focus)
            .focus_trap("import-dialog", &self.import_dialog_focus)
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child(title),
                    )
                    .child(self.badge(mode, BLUE, 0x14283d))
                    .when_some(dialog.origin.run_summary(), |this, summary| {
                        this.child(self.badge(&summary, MUTED, PANEL_2))
                    })
                    .child(div().flex_1()),
            );

        match &dialog.phase {
            ImportDialogPhase::Ready => {
                let source = dialog.source.as_ref();
                let has_files = source.is_some_and(|source| !source.files().is_empty());
                let mut body = v_flex()
                    .gap(px(10.))
                    .child(self.section_label("取り込み元"))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                Button::new("import-pick-files")
                                    .label("ファイルを選択")
                                    .small()
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.pick_import_files(cx)),
                                    ),
                            )
                            .child(
                                Button::new("import-pick-folder")
                                    .label("フォルダを選択")
                                    .small()
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.pick_import_folder(cx)),
                                    ),
                            ),
                    )
                    .child(div().text_size(px(10.)).text_color(rgb(MUTED_2)).child(
                        "フォルダはサブフォルダも対象です。compressed フォルダは除外します。",
                    ))
                    .when_some(source.map(|source| source.summary()), |this, summary| {
                        this.child(
                            div()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED))
                                .child(summary),
                        )
                    })
                    .when_some(
                        source.and_then(|source| {
                            let count = source.issues().len();
                            (count > 0).then(|| {
                                format!(
                                    "{count} 件のフォルダを読み取れませんでした（結果に表示します）"
                                )
                            })
                        }),
                        |this, warning| {
                            this.child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(AMBER))
                                    .child(warning),
                            )
                        },
                    )
                    .when_some(dialog.error.clone(), |this, error| {
                        this.child(
                            div()
                                .p(px(10.))
                                .rounded(px(8.))
                                .bg(rgb(0x2a1414))
                                .border_1()
                                .border_color(rgba(0xfb718533))
                                .text_size(px(10.))
                                .text_color(rgb(RED))
                                .child(error),
                        )
                    });
                body = body.child(self.section_label("取り込み先")).child(
                    v_flex()
                        .gap(px(4.))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED))
                                .child(dest.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED_2))
                                .child("設定 > ライブラリ > 取り込み先で変更できます。"),
                        ),
                );
                panel = panel.child(body).child(
                    h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("import-cancel")
                                .label("キャンセル")
                                .small()
                                .ghost()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.cancel_or_close_import(cx)),
                                ),
                        )
                        .child(
                            Button::new("import-start")
                                .label("取り込み開始")
                                .small()
                                .primary()
                                .disabled(!has_files)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.start_import_from_source(cx)),
                                ),
                        ),
                );
            }
            ImportDialogPhase::Running {
                progress,
                cancel_requested,
                ..
            } => {
                let last_source = progress.last_source.as_ref().map(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.to_string_lossy().to_string())
                });
                panel = panel
                    .child(
                        v_flex()
                            .gap(px(10.))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(MUTED))
                                    .child(import_progress_label(progress)),
                            )
                            .child(
                                div()
                                    .h(px(6.))
                                    .w_full()
                                    .rounded_full()
                                    .bg(rgb(PANEL_2))
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(import_progress_ratio(progress)))
                                            .rounded_full()
                                            .bg(rgb(BLUE)),
                                    ),
                            )
                            .when_some(last_source, |this, file| {
                                this.child(
                                    div()
                                        .truncate()
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED_2))
                                        .child(file),
                                )
                            }),
                    )
                    .when_some(
                        dialog.source.as_ref().map(|source| source.summary()),
                        |this, summary| {
                            this.child(
                                div()
                                    .truncate()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(summary),
                            )
                        },
                    )
                    .child(self.section_label("取り込み先"))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(dest.clone()),
                    )
                    .child(
                        h_flex().justify_end().child(
                            Button::new("import-cancel")
                                .label(if *cancel_requested {
                                    "キャンセル中…"
                                } else {
                                    "キャンセル"
                                })
                                .small()
                                .ghost()
                                .disabled(*cancel_requested)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.cancel_or_close_import(cx)),
                                ),
                        ),
                    );
            }
        }

        div()
            .id("import-dialog-scrim")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x05070acc))
            .on_click(cx.listener(|this, _, _, cx| this.cancel_or_close_import(cx)))
            .child(panel)
            .with_animation(
                "import-dialog",
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }

    fn compression_screen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let profile = self.profiles.get(self.profile_index).cloned();
        let profile_count = self.profiles.len();
        let message = self.message.clone();
        let header = self.screen_header(
            "圧縮設定",
            "プロファイルと画質を管理します。手動圧縮の出力先と元画像は実行時に選べます。"
                .to_string(),
            message,
            vec![
                Button::new("add-profile")
                    .label("＋ プロファイル")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let profile = CompressionProfile {
                            name: format!("新規プロファイル {}", this.profiles.len() + 1),
                            auto_enabled: false,
                            ..CompressionProfile::default()
                        };
                        if let Err(error) = this.db.save_profile(&profile) {
                            tracing::warn!(%error, "cannot add compression profile");
                            this.message = Some(format!("プロファイルを追加できません: {error}"));
                        }
                        this.profiles = this.db.list_profiles().unwrap_or_else(|error| {
                            tracing::warn!(%error, "cannot reload compression profiles");
                            Vec::new()
                        });
                        this.profile_index = this
                            .profiles
                            .iter()
                            .position(|item| item.id == profile.id)
                            .unwrap_or(0);
                        cx.notify();
                    }))
                    .into_any_element(),
                Button::new("manual-compress")
                    .label("手動圧縮")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let ids = if this.selected_ids.is_empty() {
                            this.selected.into_iter().collect()
                        } else {
                            this.selected_ids.clone()
                        };
                        this.open_manual_compression(ids, cx);
                    }))
                    .into_any_element(),
            ],
        );

        v_flex().flex_1().min_h_0().child(header).child(
            v_flex()
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .px(px(26.))
                .pt(px(22.))
                .pb(px(26.))
                .gap(px(24.))
                .child(
                    h_flex()
                        .gap(px(24.))
                        .items_start()
                        .child(v_flex().w(px(280.)).gap(px(8.)).children(
                            self.profiles.iter().enumerate().map(|(index, profile)| {
                                let active = index == self.profile_index;
                                let auto = profile.auto_enabled;
                                let title = profile.name.clone();
                                let meta = profile_summary(profile);
                                glass_panel()
                                    .id(SharedString::from(format!("profile-{index}")))
                                    .flex()
                                    .flex_col()
                                    .p(px(14.))
                                    .gap(px(6.))
                                    .bg(rgb(if active { ACCENT_SOFT } else { PANEL }))
                                    .border_color(rgb(if active { BLUE } else { BORDER }))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.profile_index = index;
                                        cx.notify();
                                    }))
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_size(px(12.))
                                                    .font_semibold()
                                                    .text_color(rgb(if active {
                                                        BLUE
                                                    } else {
                                                        MUTED
                                                    }))
                                                    .child(title),
                                            )
                                            .child(self.badge(
                                                if auto { "自動" } else { "手動" },
                                                if auto { GREEN } else { MUTED },
                                                if auto { 0x112a22 } else { PANEL_2 },
                                            ))
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "profile-up-{index}"
                                                    )))
                                                    .w(px(20.))
                                                    .h(px(20.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(px(6.))
                                                    .text_size(px(11.))
                                                    .text_color(rgb(if index == 0 {
                                                        MUTED_2
                                                    } else {
                                                        MUTED
                                                    }))
                                                    .when(index > 0, |this| {
                                                        this.cursor_pointer()
                                                            .hover(|this| {
                                                                this.bg(rgb(PANEL_2))
                                                                    .text_color(rgb(TEXT))
                                                            })
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.move_profile(
                                                                        index, -1, cx,
                                                                    );
                                                                    cx.stop_propagation();
                                                                },
                                                            ))
                                                    })
                                                    .child("↑"),
                                            )
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "profile-down-{index}"
                                                    )))
                                                    .w(px(20.))
                                                    .h(px(20.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(px(6.))
                                                    .text_size(px(11.))
                                                    .text_color(rgb(
                                                        if index + 1 == profile_count {
                                                            MUTED_2
                                                        } else {
                                                            MUTED
                                                        },
                                                    ))
                                                    .when(index + 1 < profile_count, |this| {
                                                        this.cursor_pointer()
                                                            .hover(|this| {
                                                                this.bg(rgb(PANEL_2))
                                                                    .text_color(rgb(TEXT))
                                                            })
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.move_profile(index, 1, cx);
                                                                    cx.stop_propagation();
                                                                },
                                                            ))
                                                    })
                                                    .child("↓"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(if active { MUTED } else { MUTED_2 }))
                                            .child(meta),
                                    )
                                    .with_animation(
                                        SharedString::from(format!(
                                            "profile-card-{}-{active}",
                                            profile.id
                                        )),
                                        Animation::new(MOTION_FAST)
                                            .with_easing(ease_motion)
                                            .with_max_fps(MOTION_FPS),
                                        move |el, t| {
                                            let (bg_from, bg_to) = if active {
                                                (PANEL, ACCENT_SOFT)
                                            } else {
                                                (ACCENT_SOFT, PANEL)
                                            };
                                            let (border_from, border_to) = if active {
                                                (BORDER, BLUE)
                                            } else {
                                                (BLUE, BORDER)
                                            };
                                            el.bg(rgb(lerp_color(bg_from, bg_to, t))).border_color(
                                                rgb(lerp_color(border_from, border_to, t)),
                                            )
                                        },
                                    )
                            }),
                        ))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(px(24.))
                                .when_some(profile, |this, profile| {
                                    let profile_index = self.profile_index;
                                    this.child(
                                        v_flex()
                                            .child(self.profile_editor(&profile, window, cx))
                                            .with_animation(
                                                ("profile-editor", profile_index),
                                                Animation::new(MOTION_FAST)
                                                    .with_easing(ease_motion)
                                                    .with_max_fps(MOTION_FPS),
                                                |el, t| {
                                                    el.opacity(t)
                                                        .relative()
                                                        .top(px(4.0 * (1.0 - t)))
                                                },
                                            ),
                                    )
                                })
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .child(self.stat_card(
                                            "総圧縮数",
                                            self.stats.compressed_count.to_string(),
                                        ))
                                        .child(
                                            self.stat_card(
                                                "写真",
                                                self.stats.photo_count.to_string(),
                                            ),
                                        )
                                        .child(self.stat_card(
                                            "圧縮後合計",
                                            format_bytes(self.db.derivative_bytes().unwrap_or(0)),
                                        )),
                                ),
                        ),
                ),
        )
    }

    fn edit_profile(&mut self, edit: impl FnOnce(&mut CompressionProfile), cx: &mut Context<Self>) {
        if let Some(profile) = self.profiles.get_mut(self.profile_index) {
            edit(profile);
            let profile = profile.clone();
            if let Err(error) = self.db.save_profile(&profile) {
                tracing::warn!(%error, profile = %profile.id, "cannot save compression profile");
                self.message = Some(format!("プロファイルを保存できません: {error}"));
            }
        }
        cx.notify();
    }

    fn move_profile(&mut self, index: usize, delta: isize, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.profiles.iter().map(|p| p.id.clone()).collect();
        let Some(order) = moved_profile_order(&ids, index, delta) else {
            return;
        };
        let target = step_index(index, ids.len(), delta);
        match self.db.reorder_profiles(&order) {
            Ok(()) => {
                self.profiles.swap(index, target);
                if self.profile_index == index {
                    self.profile_index = target;
                } else if self.profile_index == target {
                    self.profile_index = index;
                }
            }
            Err(error) => self.message = Some(format!("並び替えを保存できません: {error}")),
        }
        cx.notify();
    }

    fn duplicate_profile(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(self.profile_index) else {
            return;
        };
        let id = profile.id.clone();
        match self.db.duplicate_profile(&id) {
            Ok(copy) => {
                self.profiles = self.db.list_profiles().unwrap_or_default();
                self.profile_index = self
                    .profiles
                    .iter()
                    .position(|profile| profile.id == copy.id)
                    .unwrap_or(0);
                self.message = Some(format!("「{}」を複製しました（自動適用はOFF）", copy.name));
            }
            Err(error) => self.message = Some(format!("複製に失敗: {error}")),
        }
        cx.notify();
    }

    fn request_delete_profile(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .cloned()
        else {
            return;
        };
        if self.profiles.len() <= 1 {
            self.message = Some("最後のプロファイルは削除できません".to_string());
            cx.notify();
            return;
        }
        match self.db.active_job_count_for_profile(id) {
            Ok(0) => {
                let entity = cx.entity();
                let id = id.to_string();
                let name = profile.name.clone();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let entity = entity.clone();
                    let id = id.clone();
                    alert
                        .title("プロファイルを削除")
                        .description(format!(
                            "「{name}」を削除します。この操作は元に戻せません。"
                        ))
                        .confirm()
                        .ok_text("削除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("キャンセル")
                        .on_ok(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.delete_profile(&id, cx));
                            true
                        })
                });
            }
            Ok(count) => {
                self.message = Some(format!(
                    "「{}」は未完了のジョブが{}件あるため削除できません",
                    profile.name, count
                ));
                cx.notify();
            }
            Err(error) => {
                self.message = Some(format!("削除前の確認に失敗: {error}"));
                cx.notify();
            }
        }
    }

    fn delete_profile(&mut self, id: &str, cx: &mut Context<Self>) {
        let removed_index = self
            .profiles
            .iter()
            .position(|profile| profile.id == id)
            .unwrap_or(self.profile_index);
        let name = self
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| id.to_string());
        match self.db.delete_profile(id) {
            Ok(()) => {
                self.profiles = self.db.list_profiles().unwrap_or_default();
                self.profile_index = removed_index.min(self.profiles.len().saturating_sub(1));
                self.message = Some("プロファイルを削除しました".to_string());
            }
            Err(error) => {
                let message = match self.db.active_job_count_for_profile(id) {
                    Ok(count) if count > 0 => {
                        format!("「{name}」は未完了のジョブが{count}件あるため削除できません")
                    }
                    _ if self.profiles.len() <= 1 => {
                        "最後のプロファイルは削除できません".to_string()
                    }
                    _ => format!("プロファイルを削除できません: {error}"),
                };
                self.message = Some(message);
            }
        }
        cx.notify();
    }

    fn profile_editor(
        &self,
        profile: &CompressionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let profile_id = profile.id.clone();
        let suffix_value = self.profile_suffix_input.read(cx).value();
        if suffix_value.as_ref() != profile.suffix.as_str() {
            self.profile_suffix_input.update(cx, |input, cx| {
                input.set_value(profile.suffix.clone(), window, cx);
            });
        }
        glass_panel()
            .flex()
            .flex_col()
            .p(px(16.))
            .gap(px(14.))
            .bg(rgb(PANEL))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child(profile.name.clone()),
                    )
                    .child(self.badge(&format_label(&profile.format), BLUE, 0x14283d))
                    .child(div().flex_1())
                    .child(
                        Button::new("duplicate-profile")
                            .label("複製")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.duplicate_profile(cx))),
                    )
                    .child(
                        Button::new("delete-profile")
                            .label("削除")
                            .small()
                            .danger()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_delete_profile(&profile_id, window, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        v_flex()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT))
                                    .child("自動圧縮に使う"),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child("新規写真にこのプロファイルを適用します"),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        Switch::new("profile-auto")
                            .checked(profile.auto_enabled)
                            .on_click(cx.listener(move |this, checked, _, cx| {
                                this.edit_profile(|profile| profile.auto_enabled = *checked, cx);
                            })),
                    ),
            )
            .child(self.section_label("形式と画質"))
            .child(
                h_flex().gap(px(4.)).children(
                    [
                        CompressionFormat::Jpeg,
                        CompressionFormat::Webp,
                        CompressionFormat::Avif,
                        CompressionFormat::JpegXl,
                    ]
                    .into_iter()
                    .map(|format| {
                        let active = profile.format == format;
                        let label = format_label(&format);
                        let selected_format = format.clone();
                        div()
                            .id(SharedString::from(format!("format-{}", format.as_str())))
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(8.))
                            .bg(if active {
                                rgb(ACCENT_SOFT)
                            } else {
                                rgba(0x00000000)
                            })
                            .text_size(px(12.))
                            .font_medium()
                            .text_color(if active { rgb(BLUE) } else { rgb(MUTED) })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_profile(
                                    |profile| {
                                        apply_profile_format(profile, selected_format.clone())
                                    },
                                    cx,
                                );
                            }))
                            .child(label)
                    }),
                ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(80.))
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("ロスレス"),
                    )
                    .child(
                        Switch::new("profile-lossless")
                            .checked(profile.lossless)
                            .disabled(!lossless_supported(&profile.format))
                            .on_click(cx.listener(move |this, checked, _, cx| {
                                this.edit_profile(|profile| profile.lossless = *checked, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(lossless_hint(&profile.format)),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(80.))
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("品質"),
                    )
                    .child(
                        Button::new("quality-minus")
                            .label("−")
                            .small()
                            .disabled(profile.lossless)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_profile(
                                    |profile| {
                                        profile.quality = profile.quality.saturating_sub(1).max(1);
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(
                        div()
                            .w(px(36.))
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child(profile.quality.to_string()),
                    )
                    .child(
                        Button::new("quality-plus")
                            .label("＋")
                            .small()
                            .disabled(profile.lossless)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_profile(
                                    |profile| {
                                        profile.quality =
                                            profile.quality.saturating_add(1).min(100);
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(div().text_size(px(10.)).text_color(rgb(MUTED_2)).child(
                        if profile.lossless {
                            "ロスレスでは品質指定を使用しません"
                        } else {
                            "品質を調整"
                        },
                    )),
            )
            .child(self.section_label("リサイズ"))
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(MUTED))
                                    .child("リサイズを有効にする"),
                            )
                            .child(
                                Switch::new("profile-resize")
                                    .checked(profile.resize_enabled)
                                    .on_click(cx.listener(|this, checked, _, cx| {
                                        this.edit_profile(
                                            |profile| profile.resize_enabled = *checked,
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .w(px(96.))
                                        .flex_shrink_0()
                                        .text_size(px(12.))
                                        .text_color(rgb(MUTED))
                                        .child("解像度"),
                                )
                                .children(
                                    [
                                        ("HD", 1280u32, 720u32),
                                        ("FHD", 1920, 1080),
                                        ("2K", 2560, 1440),
                                        ("4K", 3840, 2160),
                                    ]
                                    .into_iter()
                                    .map(
                                        |(label, width, height)| {
                                            let active =
                                                profile.width == width && profile.height == height;
                                            div()
                                                .id(SharedString::from(format!("resize-{label}")))
                                                .px(px(8.))
                                                .py(px(4.))
                                                .rounded(px(8.))
                                                .bg(if active {
                                                    rgb(ACCENT_SOFT)
                                                } else {
                                                    rgba(0x00000000)
                                                })
                                                .text_size(px(10.))
                                                .font_medium()
                                                .text_color(if active {
                                                    rgb(BLUE)
                                                } else {
                                                    rgb(MUTED)
                                                })
                                                .cursor_pointer()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.edit_profile(
                                                        |profile| {
                                                            profile.resize_enabled = true;
                                                            profile.width = width;
                                                            profile.height = height;
                                                        },
                                                        cx,
                                                    );
                                                }))
                                                .child(label)
                                        },
                                    ),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED_2))
                                        .child(format!("{} × {}", profile.width, profile.height)),
                                ),
                        )
                    })
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            v_flex()
                                .gap(px(4.))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(96.))
                                                .flex_shrink_0()
                                                .text_size(px(12.))
                                                .text_color(rgb(MUTED))
                                                .child("リサイズ方法"),
                                        )
                                        .child(
                                            h_flex().gap(px(4.)).children(
                                                [
                                                    (ResizeMode::DownscaleOnly, "大きい場合のみ"),
                                                    (ResizeMode::Always, "毎回"),
                                                ]
                                                .into_iter()
                                                .map(|(mode, label)| {
                                                    let active = profile.resize_mode == mode;
                                                    div()
                                                        .id(SharedString::from(format!(
                                                            "resize-mode-{}",
                                                            mode.as_str()
                                                        )))
                                                        .px(px(8.))
                                                        .py(px(4.))
                                                        .rounded(px(8.))
                                                        .bg(if active {
                                                            rgb(ACCENT_SOFT)
                                                        } else {
                                                            rgba(0x00000000)
                                                        })
                                                        .text_size(px(10.))
                                                        .font_medium()
                                                        .text_color(if active {
                                                            rgb(BLUE)
                                                        } else {
                                                            rgb(MUTED)
                                                        })
                                                        .cursor_pointer()
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.edit_profile(
                                                                    |profile| {
                                                                        profile.resize_mode = mode;
                                                                    },
                                                                    cx,
                                                                );
                                                            },
                                                        ))
                                                        .child(label)
                                                }),
                                            ),
                                        ),
                                )
                                .child(
                                    div()
                                        .pl(px(104.))
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED_2))
                                        .child(match profile.resize_mode {
                                            ResizeMode::DownscaleOnly => {
                                                "指定解像度より大きい画像だけ縮小します"
                                            }
                                            ResizeMode::Always => {
                                                "小さい画像も指定解像度に合わせて拡大します"
                                            }
                                        }),
                                ),
                        )
                    })
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(96.))
                                        .flex_shrink_0()
                                        .text_size(px(12.))
                                        .text_color(rgb(MUTED_2))
                                        .child("アスペクト比維持"),
                                )
                                .child(
                                    Switch::new("profile-aspect")
                                        .checked(profile.maintain_aspect)
                                        .on_click(cx.listener(|this, checked, _, cx| {
                                            this.edit_profile(
                                                |profile| profile.maintain_aspect = *checked,
                                                cx,
                                            );
                                        })),
                                ),
                        )
                    }),
            )
            .child(self.section_label("出力"))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("出力先"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(
                                profile
                                    .output_dir
                                    .clone()
                                    .unwrap_or_else(|| "compressed/".to_string()),
                            ),
                    )
                    .child(
                        Button::new("profile-output-pick")
                            .label("フォルダを選択")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let current = this
                                    .profiles
                                    .get(this.profile_index)
                                    .and_then(|profile| profile.output_dir.clone());
                                let mut picker = rfd::FileDialog::new();
                                if let Some(dir) = current {
                                    picker = picker.set_directory(dir);
                                }
                                if let Some(dir) = picker.pick_folder() {
                                    let path = dir.to_string_lossy().to_string();
                                    this.edit_profile(
                                        move |profile| profile.output_dir = Some(path.clone()),
                                        cx,
                                    );
                                }
                            })),
                    )
                    .when(profile.output_dir.is_some(), |this| {
                        this.child(
                            Button::new("profile-output-clear")
                                .label("既定に戻す")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.edit_profile(|profile| profile.output_dir = None, cx);
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .w(px(86.))
                            .flex_shrink_0()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("サフィックス"),
                    )
                    .child(Input::new(&self.profile_suffix_input).small().flex_1())
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("空欄可"),
                    ),
            )
            .child(
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(rgb(PANEL_2))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child("変更は即時保存されます。サフィックスの使用不可文字は _ に置換されます。手動圧縮の出力先と元画像の扱いは実行時に選べます。"),
            )
    }

    fn queue_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let retryable_jobs = self.failed_jobs.iter().any(job_retry_available);
        glass_panel()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .child(
                h_flex()
                    .px(px(16.))
                    .py(px(14.))
                    .gap(px(8.))
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("圧縮キュー"),
                    )
                    .when(self.active_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("未完了 {}", self.active_job_count),
                            BLUE,
                            0x14283d,
                        ))
                    })
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("未実行 {}", self.waiting_job_count),
                            AMBER,
                            0x2a2412,
                        ))
                    })
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(if self.settings.auto_compress_keep_original {
                            self.badge("自動: 元画像を保持", GREEN, 0x112a22)
                        } else {
                            self.badge("自動: 元画像を置換", RED, 0x2a1414)
                        })
                    })
                    .when(self.failed_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("失敗 {}", self.failed_job_count),
                            RED,
                            0x2a1414,
                        ))
                    })
                    .child(div().flex_1())
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(
                            Button::new("release-waiting-jobs")
                                .label("待機をすべて圧縮")
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.request_release_waiting_jobs(window, cx)
                                })),
                        )
                    })
                    .when(retryable_jobs, |this| {
                        this.child(
                            Button::new("retry-all-failed-jobs")
                                .label("失敗をすべて再試行")
                                .small()
                                .ghost()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.retry_all_failed_jobs(cx)),
                                ),
                        )
                    }),
            )
            .when(self.failed_job_count > 100, |this| {
                this.child(
                    div()
                        .px(px(16.))
                        .py(px(8.))
                        .text_size(px(10.))
                        .text_color(rgb(MUTED_2))
                        .child("失敗は最新の100件のみ表示しています（再試行は上限内のすべてが対象です）"),
                )
            })
            .children(self.waiting_jobs.iter().take(20).map(|job| {
                let status = queue_status_badge(&job.status);
                let profile_name = self
                    .profiles
                    .iter()
                    .find(|profile| profile.id == job.profile_id)
                    .map(|profile| profile.name.clone())
                    .unwrap_or_else(|| "不明なプロファイル".to_string());
                h_flex()
                    .px(px(16.))
                    .py(px(10.))
                    .gap(px(8.))
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(self.badge(status.0, status.1, status.2))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .truncate()
                            .child(format!("photo #{}", job.photo_id)),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(profile_name),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(format!("{:.0}%", job.progress * 100.0)),
                    )
            }))
            .children(
                self.jobs
                    .iter()
                    .filter(|job| !matches!(job.status, QueueStatus::Waiting | QueueStatus::Failed))
                    .take(6)
                    .map(|job| {
                        let status = queue_status_badge(&job.status);
                        let profile_name = self
                            .profiles
                            .iter()
                            .find(|profile| profile.id == job.profile_id)
                            .map(|profile| profile.name.clone())
                            .unwrap_or_else(|| "不明なプロファイル".to_string());
                        h_flex()
                            .px(px(16.))
                            .py(px(10.))
                            .gap(px(8.))
                            .items_center()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(self.badge(status.0, status.1, status.2))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT))
                                    .truncate()
                                    .child(format!("photo #{}", job.photo_id)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(profile_name),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format!("{:.0}%", job.progress * 100.0)),
                            )
                    }),
            )
            .children(self.failed_jobs.iter().map(|job| {
                let name = self
                    .failed_job_names
                    .get(&job.photo_id)
                    .cloned()
                    .unwrap_or_else(|| format!("photo #{}", job.photo_id));
                let profile_name = self
                    .profiles
                    .iter()
                    .find(|profile| profile.id == job.profile_id)
                    .map(|profile| profile.name.clone());
                let error = job.error.clone().unwrap_or_default();
                let retryable = job_retry_available(job);
                let id = job.id.clone();
                v_flex()
                    .px(px(16.))
                    .py(px(10.))
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(self.badge("失敗", RED, 0x2a1414))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT))
                                    .truncate()
                                    .child(name),
                            )
                            .when_some(profile_name, |this, name| {
                                this.child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED_2))
                                        .child(name),
                                )
                            }),
                    )
                    .when(!error.is_empty(), |this| {
                        this.child(
                            div()
                                .w_full()
                                .text_size(px(10.))
                                .text_color(rgb(RED))
                                .child(error),
                        )
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(job_retry_label(job)),
                            )
                            .when(retryable, |this| {
                                this.child(
                                    Button::new(SharedString::from(format!(
                                        "retry-job-{}",
                                        job.id
                                    )))
                                    .label("再試行")
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_job(id.clone(), cx)
                                    })),
                                )
                            }),
                    )
            }))
            .when(
                self.waiting_jobs.is_empty()
                    && self.jobs.is_empty()
                    && self.failed_jobs.is_empty(),
                |this| {
                    this.child(
                        div()
                            .p(px(16.))
                            .text_size(px(12.))
                            .text_color(rgb(MUTED_2))
                            .child("キューは空です"),
                    )
                },
            )
    }

    fn search_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let filters = self.search_filter_panel(cx);
        let message = self.message.clone().or_else(|| self.library_error.clone());
        let total = self.search_view.index.total;
        let count = if self.searching {
            "検索中…".to_string()
        } else {
            format!("{total} 枚")
        };
        h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(filters)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .px(px(26.))
                    .pt(px(26.))
                    .pb(px(26.))
                    .gap(px(18.))
                    .child(
                        v_flex()
                            .flex_shrink_0()
                            .gap(px(4.))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_size(px(24.))
                                            .font_semibold()
                                            .text_color(rgb(TEXT))
                                            .child(count),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(MUTED_2))
                                            .child("検索結果"),
                                    ),
                            )
                            .when_some(message, |this, message| {
                                this.child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(AMBER))
                                        .truncate()
                                        .child(message),
                                )
                            })
                            .child(self.selection_bar(cx)),
                    )
                    .child(self.photo_grid(true, window, cx)),
            )
            .into_any_element()
    }

    fn search_filter_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        glass_panel()
            .flex()
            .flex_col()
            .w(px(290.))
            .flex_shrink_0()
            .my(px(22.))
            .ml(px(26.))
            .p(px(16.))
            .gap(px(14.))
            .rounded(px(20.))
            .bg(rgb(PANEL))
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_size(px(17.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child("絞り込み"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("カテゴリ間はAND、カテゴリ内はOR"),
            )
            .child(self.filter_field("ワールド", Input::new(&self.world_input)))
            .child(self.filter_field("プレイヤー", Input::new(&self.player_input)))
            .child(self.filter_field("タグ", Input::new(&self.tag_input)))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("日時"),
                    )
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(self.preset_button("今日", Some(1), cx))
                            .child(self.preset_button("7日", Some(7), cx))
                            .child(self.preset_button("30日", Some(30), cx))
                            .child(self.preset_button("すべて", None, cx)),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("お気に入りのみ"),
                    )
                    .child(div().flex_1())
                    .child(
                        Switch::new("search-favorite")
                            .checked(self.search_favorite)
                            .on_click(cx.listener(|this, value, _, cx| {
                                this.search_favorite = *value;
                                this.run_search(cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("search-run")
                            .label(if self.searching {
                                "検索中…"
                            } else {
                                "検索"
                            })
                            .small()
                            .primary()
                            .disabled(self.searching)
                            .on_click(cx.listener(|this, _, _, cx| this.run_search(cx))),
                    )
                    .child(
                        Button::new("search-clear")
                            .label("クリア")
                            .small()
                            .secondary()
                            .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx))),
                    ),
            )
            .into_any_element()
    }

    fn open_import_dialog(
        &mut self,
        mode: ImportMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = ImportDialogState {
            origin: ImportDialogOrigin::New,
            mode,
            dest: self.settings.import_dir(),
            source: None,
            error: None,
            phase: ImportDialogPhase::Ready,
        };
        if !reserve_import_dialog(&mut self.import_dialog, dialog) {
            return;
        }
        let focus = self.import_dialog_focus.clone();
        window.defer(cx, move |window, cx| {
            window.focus(&focus, cx);
        });
        cx.notify();
    }

    fn pick_import_files(&mut self, cx: &mut Context<Self>) {
        let Some(files) = rfd::FileDialog::new()
            .add_filter("画像", &["png", "jpg", "jpeg"])
            .pick_files()
        else {
            return;
        };
        if let Some(dialog) = &mut self.import_dialog {
            dialog.error = None;
            dialog.source = Some(ImportSource::Files(files));
        }
        cx.notify();
    }

    fn pick_import_folder(&mut self, cx: &mut Context<Self>) {
        let Some(folder) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let dest = self
            .import_dialog
            .as_ref()
            .map(|dialog| dialog.dest.clone());
        let scan = match importer::collect_folder_sources(&folder, dest.as_deref()) {
            Ok(scan) => scan,
            Err(error) => {
                tracing::warn!(%error, path = %folder.display(), "cannot scan import folder");
                if let Some(dialog) = &mut self.import_dialog {
                    dialog.error = Some(format!("フォルダを読み込めません: {error}"));
                }
                cx.notify();
                return;
            }
        };
        if let Some(dialog) = &mut self.import_dialog {
            dialog.error = scan
                .files
                .is_empty()
                .then(|| "対象画像が見つかりません".to_string());
            dialog.source = Some(ImportSource::Folder {
                folder,
                files: scan.files,
                issues: scan.issues,
            });
        }
        cx.notify();
    }

    fn start_import_from_source(&mut self, cx: &mut Context<Self>) {
        let Some(files) = self
            .import_dialog
            .as_ref()
            .and_then(|dialog| dialog.source.as_ref())
            .map(|source| source.files().to_vec())
        else {
            return;
        };
        self.start_import_run(files, cx);
    }

    fn start_import_run(&mut self, files: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog.as_mut() else {
            return;
        };
        if !matches!(&dialog.phase, ImportDialogPhase::Ready) {
            return;
        }
        if files.is_empty() {
            return;
        }
        let total = files.len();
        let cancel = Arc::new(AtomicBool::new(false));
        dialog.error = None;
        dialog.phase = ImportDialogPhase::Running {
            progress: ImportProgress {
                processed: 0,
                total,
                imported: 0,
                skipped: 0,
                failed: 0,
                partial: 0,
                last_source: None,
            },
            cancel: cancel.clone(),
            cancel_requested: false,
        };
        let dest = dialog.dest.clone();
        let mode = dialog.mode;
        let task_cancel = cancel.clone();
        let observe_cancel = cancel.clone();
        let (tx, rx) = async_channel::unbounded();
        cx.background_executor()
            .spawn(async move {
                let mut observe = |progress: ImportProgress| {
                    if tx.try_send(ImportRunMessage::Progress(progress)).is_err() {
                        observe_cancel.store(true, Ordering::Relaxed);
                    }
                };
                let result = importer::import_files_observed(
                    &files,
                    &dest,
                    mode,
                    Some(&task_cancel),
                    &mut observe,
                )
                .map_err(|error| error.to_string());
                let _ = tx.try_send(ImportRunMessage::Finished(result));
            })
            .detach();
        cx.spawn(async move |this, cx| {
            while let Ok(message) = rx.recv().await {
                let done = this
                    .update(cx, |this, cx| match message {
                        ImportRunMessage::Progress(progress) => {
                            this.update_import_progress(progress, cx);
                            false
                        }
                        ImportRunMessage::Finished(result) => {
                            this.finish_import_run(result, cx);
                            true
                        }
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn update_import_progress(&mut self, progress: ImportProgress, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.import_dialog
            && let ImportDialogPhase::Running {
                progress: current, ..
            } = &mut dialog.phase
        {
            *current = progress;
        }
        cx.notify();
    }

    fn finish_import_run(&mut self, result: Result<ImportResult, String>, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog.take() else {
            return;
        };
        match result {
            Err(error) => match dialog.origin {
                ImportDialogOrigin::New => {
                    self.message = Some(format!("取り込みに失敗: {error}"));
                    self.import_dialog = Some(ImportDialogState {
                        origin: ImportDialogOrigin::New,
                        mode: dialog.mode,
                        dest: dialog.dest,
                        source: dialog.source,
                        error: Some(format!("取り込みに失敗: {error}")),
                        phase: ImportDialogPhase::Ready,
                    });
                }
                ImportDialogOrigin::Retry { .. } => {
                    self.message = Some(format!("再試行に失敗: {error}"));
                }
                ImportDialogOrigin::Resume { .. } => {
                    self.message = Some(format!("再開に失敗: {error}"));
                }
            },
            Ok(result) => {
                let imported = result.imported.len();
                let mut report = match dialog.origin {
                    ImportDialogOrigin::New => {
                        let files = dialog
                            .source
                            .as_ref()
                            .map(|source| source.files().to_vec())
                            .unwrap_or_default();
                        let mut issues = dialog
                            .source
                            .as_ref()
                            .map(|source| source.issues().to_vec())
                            .unwrap_or_default();
                        let remaining = if result.cancelled {
                            import_remaining(&files, result.processed)
                        } else {
                            Vec::new()
                        };
                        issues.extend(result.issues);
                        ImportReportView {
                            mode: dialog.mode,
                            dest: dialog.dest.clone(),
                            total: files.len(),
                            processed: result.processed,
                            imported,
                            skipped: result.skipped,
                            issues,
                            renamed: result.renamed,
                            remaining,
                            cancelled: result.cancelled,
                            scan_failures: Vec::new(),
                            scan_error: None,
                        }
                    }
                    ImportDialogOrigin::Retry { report, sources } => {
                        merge_retry_report(&report, &sources, result)
                    }
                    ImportDialogOrigin::Resume { report, .. } => {
                        merge_resume_report(&report, result)
                    }
                };
                if import_should_index(imported) {
                    report.scan_failures.clear();
                    report.scan_error = None;
                    self.index_import_destination(&mut report, cx);
                }
                self.message = Some(import_summary(&report));
                self.import_report = Some(report);
                self.import_dialog = None;
                cx.notify();
            }
        }
    }

    fn cancel_or_close_import(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog.as_mut() else {
            return;
        };
        let close = match &mut dialog.phase {
            ImportDialogPhase::Ready => true,
            ImportDialogPhase::Running {
                cancel,
                cancel_requested,
                ..
            } => {
                cancel.store(true, Ordering::Relaxed);
                *cancel_requested = true;
                false
            }
        };
        if close {
            self.import_dialog = None;
        }
        cx.notify();
    }

    /// Registers, scans, and links the import destination, then rebuilds the
    /// library. Indexing failures stay on the report instead of aborting the
    /// UI, so the imported files and per-file issues remain visible.
    fn index_import_destination(&mut self, report: &mut ImportReportView, cx: &mut Context<Self>) {
        let dest_string = report.dest.to_string_lossy().to_string();
        if let Err(error) = add_photo_root_and_save(&mut self.settings, &dest_string, |settings| {
            settings.save(&self.db)
        }) {
            tracing::warn!(%error, "cannot save photo roots after import");
            append_scan_error(report, format!("取り込み先の設定を保存できません: {error}"));
        }
        let root_id = match self.db.add_root(&dest_string, "managed") {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(%error, "cannot register import destination");
                append_scan_error(report, format!("取り込み先を登録できません: {error}"));
                return;
            }
        };
        match scanner::scan_and_index(&self.db, root_id, &report.dest) {
            Ok(scan) => {
                report.scan_failures.extend(scan.failures);
                let mut preview_jobs = Vec::new();
                for id in &scan.new_ids {
                    let Ok(Some(photo)) = self.db.photo_by_id(*id) else {
                        continue;
                    };
                    preview_jobs.push((preview_key(&photo), PathBuf::from(photo.effective_path())));
                    if let Err(error) =
                        thumbnail::ensure_thumbnail(*id, Path::new(photo.effective_path()))
                    {
                        tracing::warn!(%error, %id, "cannot generate thumbnail after import");
                    }
                }
                self.warm_previews(preview_jobs, cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot scan import destination");
                append_scan_error(report, format!("取り込み先をスキャンできません: {error}"));
            }
        }
        if let Err(error) = linker::relink_unlinked(&self.db, 1000) {
            tracing::warn!(%error, "cannot link imported photos");
            append_scan_error(report, format!("写真の紐付けに失敗しました: {error}"));
        }
        if !self.reload_library(true, cx)
            && let Some(error) = self.library_error.clone()
        {
            append_scan_error(report, error);
        }
        // The daemon may not have watched the new import destination yet.
        // A rescan makes it reload settings and rebuild its recursive watchers.
        match self.db.request_daemon_command(DaemonCommand::Rescan) {
            Ok(_) => self.send_wake_commands(cx),
            Err(error) => {
                tracing::warn!(%error, "cannot request rescan for import destination");
            }
        }
    }

    /// Generates preview cache entries for newly imported photos without
    /// blocking the import report or the library reload.
    fn warm_previews(&mut self, jobs: Vec<(PreviewKey, PathBuf)>, cx: &mut Context<Self>) {
        if jobs.is_empty() {
            return;
        }
        let cache_dir = preview_cache_dir(
            self.demo,
            self.demo_assets
                .as_ref()
                .map(|assets| assets.preview_dir.clone()),
            preview::previews_dir(),
        );
        let Some(cache_dir) = cache_dir else {
            return;
        };
        cx.background_executor()
            .spawn(async move {
                for (key, source) in jobs {
                    let start = Instant::now();
                    let _ =
                        preview::ensure_preview_in(key.photo_id, key.variant, &source, &cache_dir);
                    log_timing("preview warm", start);
                }
            })
            .detach();
    }

    /// Opens the import modal in retry mode for the retryable failed sources.
    fn open_retry_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(report) = self.import_report.clone() else {
            return;
        };
        let sources = retryable_sources(&report.issues);
        if sources.is_empty() {
            return;
        }
        let dialog = ImportDialogState {
            origin: ImportDialogOrigin::Retry {
                report: report.clone(),
                sources: sources.clone(),
            },
            mode: report.mode,
            dest: report.dest.clone(),
            source: None,
            error: None,
            phase: ImportDialogPhase::Ready,
        };
        if !reserve_import_dialog(&mut self.import_dialog, dialog) {
            return;
        }
        let focus = self.import_dialog_focus.clone();
        window.defer(cx, move |window, cx| {
            window.focus(&focus, cx);
        });
        self.start_import_run(sources, cx);
    }

    /// Opens the import modal in resume mode for the unprocessed sources.
    fn open_resume_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(report) = self.import_report.clone() else {
            return;
        };
        let sources = report.remaining.clone();
        if sources.is_empty() {
            return;
        }
        let dialog = ImportDialogState {
            origin: ImportDialogOrigin::Resume {
                report: report.clone(),
                sources: sources.clone(),
            },
            mode: report.mode,
            dest: report.dest.clone(),
            source: None,
            error: None,
            phase: ImportDialogPhase::Ready,
        };
        if !reserve_import_dialog(&mut self.import_dialog, dialog) {
            return;
        }
        let focus = self.import_dialog_focus.clone();
        window.defer(cx, move |window, cx| {
            window.focus(&focus, cx);
        });
        self.start_import_run(sources, cx);
    }

    fn import_report_panel(&self, report: &ImportReportView, cx: &mut Context<Self>) -> AnyElement {
        let (failed, partial) = import_issue_counts(&report.issues);
        let retryable = retryable_sources(&report.issues);
        let mode = import_mode_label(report.mode);
        let dest = report.dest.to_string_lossy().to_string();
        let scan_failed = report.scan_failures.len();
        let failed_issues: Vec<&ImportIssue> = report
            .issues
            .iter()
            .filter(|issue| issue.kind == ImportIssueKind::Failed)
            .collect();
        let partial_issues: Vec<&ImportIssue> = report
            .issues
            .iter()
            .filter(|issue| issue.kind == ImportIssueKind::PartialMove)
            .collect();

        let mut panel = v_flex()
            .flex_shrink_0()
            .mx(px(26.))
            .mb(px(12.))
            .rounded(px(14.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                h_flex()
                    .px(px(16.))
                    .py(px(10.))
                    .gap(px(10.))
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("取り込み結果"),
                    )
                    .child(self.badge(mode, BLUE, 0x14283d))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(dest),
                    )
                    .when(!retryable.is_empty(), |this| {
                        let button = Button::new("import-retry-failed")
                            .label("失敗分を再試行")
                            .small();
                        let button = if report.remaining.is_empty() {
                            button.primary()
                        } else {
                            button.ghost()
                        };
                        this.child(button.on_click(
                            cx.listener(|this, _, window, cx| this.open_retry_dialog(window, cx)),
                        ))
                    })
                    .when(!report.remaining.is_empty(), |this| {
                        this.child(
                            Button::new("import-resume")
                                .label(format!("残り {} 件を再開", report.remaining.len()))
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_resume_dialog(window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("import-report-close")
                            .label("閉じる")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.import_report = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(8.))
                    .items_center()
                    .child(self.badge(&format!("取込 {}", report.imported), GREEN, 0x102a24))
                    .child(self.badge(&format!("スキップ {}", report.skipped), MUTED, PANEL_2))
                    .when(failed > 0, |this| {
                        this.child(self.badge(&format!("失敗 {failed}"), RED, 0x2a1414))
                    })
                    .when(partial > 0, |this| {
                        this.child(self.badge(&format!("部分移動 {partial}"), AMBER, 0x2b2210))
                    })
                    .when(!report.renamed.is_empty(), |this| {
                        this.child(self.badge(
                            &format!("連番 {}", report.renamed.len()),
                            BLUE,
                            0x14283d,
                        ))
                    })
                    .when(!report.remaining.is_empty(), |this| {
                        this.child(self.badge(
                            &format!("未処理 {}", report.remaining.len()),
                            AMBER,
                            0x2b2210,
                        ))
                    })
                    .when(scan_failed > 0, |this| {
                        this.child(self.badge(
                            &format!("スキャン失敗 {scan_failed}"),
                            RED,
                            0x2a1414,
                        ))
                    }),
            );

        if !failed_issues.is_empty() {
            panel = panel.child(
                v_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(self.section_label("失敗したファイル"))
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .max_h(px(150.))
                            .overflow_y_scrollbar()
                            .children(failed_issues.iter().map(|issue| {
                                let source = issue.source.to_string_lossy().to_string();
                                v_flex()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(10.))
                                            .text_color(rgb(RED))
                                            .child(source),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(MUTED))
                                            .child(issue.message.clone()),
                                    )
                            })),
                    ),
            );
        }

        if !partial_issues.is_empty() {
            panel = panel.child(
                v_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(self.section_label("移動元を削除できなかったファイル"))
                    .children(partial_issues.iter().map(|issue| {
                        let source = issue.source.to_string_lossy().to_string();
                        let target = issue
                            .target
                            .as_ref()
                            .map(|target| target.to_string_lossy().to_string());
                        v_flex()
                            .gap(px(2.))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(10.))
                                            .text_color(rgb(AMBER))
                                            .child(source),
                                    )
                                    .when_some(target, |this, target| {
                                        this.child(
                                            div()
                                                .truncate()
                                                .text_size(px(9.))
                                                .text_color(rgb(MUTED_2))
                                                .child(format!("→ {target}")),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED))
                                    .child(issue.message.clone()),
                            )
                    })),
            );
        }

        if !report.renamed.is_empty() {
            panel = panel.child(
                v_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(self.section_label("連番を付けたファイル"))
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .max_h(px(150.))
                            .overflow_y_scrollbar()
                            .children(report.renamed.iter().map(|rename| {
                                v_flex()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(10.))
                                            .text_color(rgb(MUTED))
                                            .child(rename.source.to_string_lossy().to_string()),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(9.))
                                            .text_color(rgb(BLUE))
                                            .child(format!(
                                                "→ {}",
                                                rename.target.to_string_lossy()
                                            )),
                                    )
                            })),
                    ),
            );
        }

        if !report.scan_failures.is_empty() {
            panel = panel.child(
                v_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(self.section_label("スキャンできなかったファイル"))
                    .children(report.scan_failures.iter().map(|failure| {
                        v_flex()
                            .gap(px(2.))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(10.))
                                    .text_color(rgb(RED))
                                    .child(failure.path.to_string_lossy().to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED))
                                    .child(failure.message.clone()),
                            )
                    })),
            );
        }

        if let Some(error) = report.scan_error.clone() {
            panel = panel.child(
                div()
                    .mx(px(16.))
                    .my(px(8.))
                    .p(px(8.))
                    .rounded(px(8.))
                    .bg(rgb(0x2a1414))
                    .border_1()
                    .border_color(rgba(0xfb718533))
                    .text_size(px(10.))
                    .text_color(rgb(RED))
                    .child(error),
            );
        }

        panel
            .with_animation(
                "import-report",
                Animation::new(MOTION_BASE)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                |el, t| el.opacity(t).relative().top(px(-8.0 * (1.0 - t))),
            )
            .into_any_element()
    }

    fn dashboard_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.screen_header(
                "ダッシュボード",
                "ライブラリ、圧縮キュー、ログ監視の状況をまとめて確認できます。".to_string(),
                self.message.clone(),
                vec![
                    Button::new("dashboard-refresh")
                        .label("再読込")
                        .small()
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.reload_aux(cx);
                            cx.notify();
                        }))
                        .into_any_element(),
                ],
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .px(px(26.))
                    .pt(px(22.))
                    .pb(px(26.))
                    .gap(px(24.))
                    .child(
                        h_flex()
                            .gap(px(16.))
                            .child(self.stat_card(
                                "写真",
                                format!("{} 枚", self.stats.photo_count),
                            ))
                            .child(self.stat_card(
                                "未完了ジョブ",
                                format!("{} 件", self.active_job_count),
                            ))
                            .child(self.stat_card(
                                "未実行",
                                format!("{} 件", self.waiting_job_count),
                            ))
                            .child(self.stat_card(
                                "デーモン",
                                if self.daemon_active {
                                    daemon_state_label(true, self.daemon_paused).to_string()
                                } else if self.settings.log_dir.is_some() {
                                    "停止中".to_string()
                                } else {
                                    "未設定".to_string()
                                },
                            )),
                    )
                    .child(self.queue_panel(cx))
                    .child(self.daemon_card(cx))
                    .when(!self.daemon_errors.is_empty(), |this| {
                        this.child(self.daemon_error_card(cx))
                    })
                    .child(
                        h_flex()
                            .gap(px(24.))
                            .items_start()
                            .child(
                                glass_panel()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .p(px(16.))
                                    .gap(px(10.))
                                    .bg(rgb(PANEL))
                                    .child(
                                        div()
                                            .text_size(px(17.))
                                            .font_semibold()
                                            .text_color(rgb(TEXT))
                                            .child("現在のセッション"),
                                    )
                                    .child(self.meta_row(
                                        "ワールド",
                                        self.monitor_world
                                            .clone()
                                            .unwrap_or_else(|| "—".to_string()),
                                    ))
                                    .child(self.meta_row(
                                        "インスタンス",
                                        self.monitor_instance
                                            .clone()
                                            .unwrap_or_else(|| "—".to_string()),
                                    ))
                                    .child(self.meta_row(
                                        "同接",
                                        format!("{} 人", self.monitor_players.len()),
                                    ))
                                    .child(self.section_label(if self.monitor_players.is_empty() {
                                        "プレイヤー情報はまだありません"
                                    } else {
                                        "入室中のプレイヤー"
                                    }))
                                    .children(self.monitor_players.iter().map(|player| {
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(MUTED))
                                            .child(player.clone())
                                    })),
                            )
                            .child(
                                glass_panel()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .p(px(16.))
                                    .gap(px(10.))
                                    .bg(rgb(PANEL))
                                    .child(
                                        div()
                                            .text_size(px(17.))
                                            .font_semibold()
                                            .text_color(rgb(TEXT))
                                            .child("監視パス"),
                                    )
                                    .children(self.settings.photo_roots.iter().map(|path| {
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(10.))
                                            .font_family("monospace")
                                            .text_color(rgb(MUTED))
                                            .child(path.clone())
                                    }))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(10.))
                                            .font_family("monospace")
                                            .text_color(rgb(MUTED))
                                            .child(
                                                self.settings
                                                    .log_dir
                                                    .clone()
                                                    .unwrap_or_else(|| "ログフォルダ未設定".to_string()),
                                            ),
                                    ),
                            ),
                    )
                    .child(self.log_card(cx))
                    .child(
                        div()
                            .p(px(14.))
                            .rounded(px(14.))
                            .bg(rgb(0x2a2412))
                            .border_1()
                            .border_color(rgba(0xfbbf2440))
                            .text_size(px(12.))
                            .text_color(rgb(AMBER))
                            .child("VRChat本体は出力ログを24時間で削除します。アプリ側には解析済みイベントだけを保存します。"),
                    ),
            )
    }

    fn daemon_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if !self.daemon_active {
            ("停止中", MUTED, PANEL_2)
        } else if self.daemon_paused {
            ("一時停止中", AMBER, 0x2a2412)
        } else {
            ("稼働中", GREEN, 0x112a22)
        };
        let sync = daemon_sync_label(
            self.daemon_age_secs,
            self.daemon_supervisor.as_deref(),
            self.ipc_connected,
        );
        let ipc = if self.ipc_connected {
            "IPC 接続中"
        } else {
            "IPC 未接続"
        };
        let ipc_detail = ipc_status_label(self.ipc_connected, self.ipc_error.as_deref());
        let ipc_color = if self.ipc_connected { GREEN } else { RED };
        let ipc_bg = if self.ipc_connected {
            0x112a22
        } else {
            0x2a1414
        };
        let scan = self.scan_progress;
        let armed = self.daemon_shutdown_armed;
        let pause_label = if self.daemon_paused {
            "監視を再開"
        } else {
            "監視を一時停止"
        };

        let mut card = glass_panel()
            .flex()
            .flex_col()
            .p(px(16.))
            .gap(px(12.))
            .bg(rgb(PANEL))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("デーモン"),
                    )
                    .child(self.badge(status.0, status.1, status.2))
                    .child(self.badge(ipc, ipc_color, ipc_bg)),
            )
            .child(self.meta_row("最終同期", sync))
            .child(self.meta_row("IPC", ipc_detail))
            .child(self.meta_row(
                "監視",
                daemon_state_label(self.daemon_active, self.daemon_paused).to_string(),
            ));

        if let Some(progress) = scan {
            card = card.child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child(scan_progress_label(&progress)),
                    )
                    .child(
                        div()
                            .h(px(6.))
                            .w_full()
                            .rounded_full()
                            .bg(rgb(PANEL_2))
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(scan_progress_ratio(&progress)))
                                    .rounded_full()
                                    .bg(rgb(BLUE)),
                            ),
                    ),
            );
        }

        card.child(
            h_flex()
                .flex_wrap()
                .gap(px(8.))
                .child(
                    Button::new("daemon-rescan")
                        .label("再スキャン")
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.request_daemon_rescan(cx))),
                )
                .child(
                    Button::new("daemon-pause")
                        .label(pause_label)
                        .small()
                        .disabled(!self.ipc_connected)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_daemon_pause(cx))),
                )
                .child(
                    Button::new("daemon-shutdown")
                        .label(if armed {
                            "もう一度押して終了"
                        } else {
                            "デーモンを終了"
                        })
                        .small()
                        .disabled(!self.ipc_connected)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_daemon_shutdown(cx))),
                ),
        )
    }

    fn daemon_error_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut card = glass_panel()
            .flex()
            .flex_col()
            .p(px(16.))
            .gap(px(10.))
            .bg(rgb(PANEL))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("処理エラー"),
                    )
                    .child(self.badge(&format!("{} 件", self.daemon_errors.len()), RED, 0x2a1414)),
            );
        for error in &self.daemon_errors {
            let command = error.operation.retry_command();
            card = card.child(
                v_flex()
                    .gap(px(8.))
                    .p(px(12.))
                    .rounded(px(10.))
                    .bg(rgb(PANEL_2))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(self.badge(
                                daemon_operation_label(error.operation),
                                RED,
                                0x2a1414,
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(RED))
                                    .child(error.message.clone()),
                            )
                            .child(
                                Button::new(SharedString::from(format!(
                                    "retry-daemon-{}",
                                    error.operation.as_str()
                                )))
                                .label("再試行")
                                .small()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.request_daemon_command(command, cx),
                                )),
                            ),
                    )
                    .when_some(error.detail.clone(), |this, detail| {
                        this.child(
                            div()
                                .w_full()
                                .min_w_0()
                                .p(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(BG))
                                .font_family("monospace")
                                .whitespace_normal()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(detail),
                        )
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format!("検出回数 {}", error.attempts)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format_timestamp(&error.occurred_at)),
                            ),
                    ),
            );
        }
        card.when(!self.daemon_active, |this| {
            this.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("デーモン停止中です。起動後に実行されます。"),
            )
        })
    }

    fn log_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dir = logs_dir();
        let mut card = glass_panel()
            .flex()
            .flex_col()
            .p(px(24.))
            .gap(px(16.))
            .bg(rgb(PANEL))
            .child(
                div()
                    .text_size(px(17.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child("ログ"),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(self.section_label("保存先"))
                    .child(match &dir {
                        Some(dir) => div()
                            .min_w_0()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child(dir.display().to_string()),
                        None => div()
                            .text_size(px(10.))
                            .text_color(rgb(RED))
                            .child("ログフォルダを特定できません"),
                    }),
            )
            .child(
                v_flex()
                    .gap(px(4.))
                    .child(self.section_label("ファイル"))
                    .child(
                        div()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(format!("UI: {UI_LOG_FILE}")),
                    )
                    .child(
                        div()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(format!("デーモン: {DAEMON_LOG_FILE}")),
                    ),
            );
        if dir.is_some() {
            card = card.child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        Button::new("open-logs-folder")
                            .label("ログフォルダを開く")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.open_logs_folder(cx))),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("OSのファイラーで開きます"),
                    ),
            );
        } else {
            card = card.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("ファイルへのログ出力は利用できません。"),
            );
        }
        card
    }

    fn open_logs_folder(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = logs_dir() else {
            self.message = Some("ログフォルダを特定できません".to_string());
            cx.notify();
            return;
        };
        let result = std::fs::create_dir_all(&dir)
            .map_err(|error| error.to_string())
            .and_then(|()| open_path(&dir));
        self.message = Some(match result {
            Ok(()) => format!("ログフォルダを開きました: {}", dir.display()),
            Err(error) => format!("ログフォルダを開けません: {error}"),
        });
        cx.notify();
    }

    fn card_metrics(&self) -> CardMetrics {
        card_metrics(&self.settings)
    }

    fn set_card_size(&mut self, size: CardSize, cx: &mut Context<Self>) {
        if self.settings.ui_card_size == size {
            return;
        }
        self.capture_display_layout_anchors();
        self.settings.ui_card_size = size;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save card size");
        }
        cx.notify();
    }

    /// Remembers the top-left photo before a display change alters columns or
    /// row height. The next render of each grid restores that photo.
    fn capture_display_layout_anchors(&mut self) {
        let metrics = self.card_metrics();
        if self.pending_layout_anchor_library.is_none() {
            self.pending_layout_anchor_library = anchor_photo_id(
                &self.library.index,
                list_scroll_top(&self.library.scroll),
                self.library.columns,
                metrics.row_height,
            );
        }
        if self.pending_layout_anchor_search.is_none() {
            self.pending_layout_anchor_search = anchor_photo_id(
                &self.search_view.index,
                list_scroll_top(&self.search_view.scroll),
                self.search_view.columns,
                metrics.row_height,
            );
        }
    }

    fn set_reduce_motion(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings.ui_reduce_motion = enabled;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save reduce-motion setting");
        }
        cx.set_reduce_motion(enabled);
        cx.notify();
    }

    fn settings_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let autostart_hint =
            autostart_description(self.daemon_active, self.daemon_supervisor.as_deref());
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.screen_header(
                "設定",
                "ライブラリと起動オプション".to_string(),
                self.message.clone(),
                Vec::new(),
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .px(px(26.))
                    .pt(px(22.))
                    .pb(px(26.))
                    .gap(px(24.))
                    .child(
                        glass_panel()
                            .flex()
                            .flex_col()
                            .p(px(24.))
                            .gap(px(16.))
                            .bg(rgb(PANEL))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child("ライブラリ"),
                            )
                            .child(
                                self.filter_field(
                                    "写真フォルダ",
                                    Input::new(&self.photo_root_input),
                                ),
                            )
                            .child(
                                self.filter_field("ログフォルダ", Input::new(&self.log_dir_input)),
                            )
                            .child(self.filter_field(
                                "取り込み先",
                                Input::new(&self.import_dir_input),
                            ))
                            .child(
                                h_flex()
                                    .gap(px(8.))
                                    .items_center()
                                    .child(
                                        Button::new("save-paths")
                                            .label("パスを保存")
                                            .small()
                                            .primary()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                let root = this
                                                    .photo_root_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let log = this
                                                    .log_dir_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let imported = this
                                                    .import_dir_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let previous = this.settings.clone();
                                                this.settings.photo_roots =
                                                    if root.trim().is_empty() {
                                                        Vec::new()
                                                    } else {
                                                        vec![root.trim().to_string()]
                                                    };
                                                this.settings.log_dir = if log.trim().is_empty() {
                                                    None
                                                } else {
                                                    Some(log.trim().to_string())
                                                };
                                                this.settings.managed_dir =
                                                    if imported.trim().is_empty() {
                                                        None
                                                    } else {
                                                        Some(imported.trim().to_string())
                                                    };
                                                let root_changed =
                                                    this.settings.photo_roots != previous.photo_roots;
                                                let paths_changed = root_changed
                                                    || this.settings.log_dir != previous.log_dir
                                                    || this.settings.managed_dir
                                                        != previous.managed_dir;
                                                if let Err(error) = this.settings.save(&this.db) {
                                                    this.settings = previous;
                                                    this.message = Some(format!(
                                                        "設定を保存できません: {error}"
                                                    ));
                                                    tracing::warn!(
                                                        %error,
                                                        "cannot save path settings"
                                                    );
                                                    cx.notify();
                                                    return;
                                                }
                                                if root_changed {
                                                    this.reload_library(false, cx);
                                                }
                                                if paths_changed {
                                                    let requested = this
                                                        .db
                                                        .request_daemon_command(DaemonCommand::Rescan);
                                                    if requested.is_ok() {
                                                        this.send_wake_commands(cx);
                                                    }
                                                    this.message = Some(match requested {
                                                        Ok(_) => "保存しました。デーモンに再スキャンを要求しました（停止中なら起動後に実行）".to_string(),
                                                        Err(error) => {
                                                            tracing::warn!(
                                                                %error,
                                                                "cannot request daemon rescan"
                                                            );
                                                            "保存しました。デーモン再起動後に反映されます。".to_string()
                                                        }
                                                    });
                                                } else {
                                                    this.message = Some("保存しました。".to_string());
                                                }
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(MUTED_2))
                                            .child("保存時に再スキャンを要求します（停止中なら起動後に実行）"),
                                    ),
                            ),
                    )
                    .child(
                        glass_panel()
                            .flex()
                            .flex_col()
                            .p(px(24.))
                            .gap(px(16.))
                            .bg(rgb(PANEL))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child("表示（ライブラリ・検索）"),
                            )
                            .child(self.setting_switch(
                                "圧縮状態",
                                "写真カード左下の圧縮状態ラベルを表示します",
                                self.settings.ui_card_show_status,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_status = value,
                            ))
                            .child(self.setting_switch(
                                "ワールド名",
                                "カードのワールド名を表示します",
                                self.settings.ui_card_show_world,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_world = value,
                            ))
                            .child(self.setting_switch(
                                "日付",
                                "撮影日を表示します",
                                self.settings.ui_card_show_date,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_date = value,
                            ))
                            .child(self.setting_switch(
                                "サイズ",
                                "ファイルサイズを表示します",
                                self.settings.ui_card_show_size,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_size = value,
                            ))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(px(12.))
                                    .py(px(2.))
                                    .child(
                                        v_flex()
                                            .gap(px(2.))
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(rgb(TEXT))
                                                    .child("カードの大きさ"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(MUTED_2))
                                                    .child("ライブラリと検索の両方に適用されます"),
                                            ),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        h_flex()
                                            .gap(px(6.))
                                            .child(self.card_size_button(CardSize::Small, cx))
                                            .child(self.card_size_button(CardSize::Medium, cx))
                                            .child(self.card_size_button(CardSize::Large, cx))
                                            .child(self.card_size_button(CardSize::ExtraLarge, cx)),
                                    ),
                            ),
                    )
                    .child(
                        glass_panel()
                            .flex()
                            .flex_col()
                            .p(px(24.))
                            .gap(px(16.))
                            .bg(rgb(PANEL))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_semibold()
                                    .text_color(rgb(TEXT))
                                    .child("起動と圧縮"),
                            )
                            .child(self.setting_switch(
                                "自動起動",
                                autostart_hint,
                                self.settings.autostart,
                                false,
                                cx,
                                |settings, value| settings.autostart = value,
                            ))
                            .child(self.setting_switch(
                                "トレイ常駐",
                                "UIを閉じても監視を継続",
                                self.settings.tray,
                                false,
                                cx,
                                |settings, value| settings.tray = value,
                            ))
                            .child(self.setting_switch(
                                "自動圧縮",
                                "新規写真に自動で圧縮を適用",
                                self.settings.auto_compress,
                                false,
                                cx,
                                |settings, value| settings.auto_compress = value,
                            ))
                            .when(self.settings.auto_compress, |this| {
                                this.child(self.setting_switch(
                                    "新規写真をすぐに圧縮",
                                    "OFFにすると未実行キューに追加し、ダッシュボードからまとめて実行できます",
                                    self.settings.auto_compress_immediate,
                                    false,
                                    cx,
                                    |settings, value| settings.auto_compress_immediate = value,
                                ))
                            })
                            .child(self.setting_switch(
                                "自動圧縮後に元画像を保持",
                                "OFFにすると自動圧縮の成功後、元画像をOSゴミ箱へ移動します。手動圧縮では実行時に選べます",
                                self.settings.auto_compress_keep_original,
                                false,
                                cx,
                                |settings, value| settings.auto_compress_keep_original = value,
                            ))
                            .when_some(
                                compression_policy_warning(
                                    self.settings.auto_compress_keep_original,
                                    self.settings.auto_compress,
                                ),
                                |this, warning| {
                                    this.child(
                                        div()
                                            .p(px(12.))
                                            .rounded(px(8.))
                                            .bg(rgb(0x2a1414))
                                            .border_1()
                                            .border_color(rgba(0xfb718533))
                                            .text_size(px(10.))
                                            .text_color(rgb(RED))
                                            .child(warning),
                                    )
                                },
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(px(12.))
                                    .py(px(2.))
                                    .child(
                                        v_flex()
                                            .gap(px(2.))
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(rgb(TEXT))
                                                    .child("アニメーションを減らす"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(MUTED_2))
                                                    .child(
                                                        "画面遷移とカードの動きを抑制します",
                                                    ),
                                            ),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Switch::new("reduce-motion")
                                            .checked(self.settings.ui_reduce_motion)
                                            .on_click(cx.listener(|this, value, _, cx| {
                                                this.set_reduce_motion(*value, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(self.update_card(cx))
                    .child(self.log_card(cx))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                Button::new("settings-reload")
                                    .label("設定を再読込")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_aux(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format!(
                                        "言語: {} · サムネイルキャッシュ: {} MB",
                                        self.settings.language, self.settings.thumbnail_cache_mb
                                    )),
                            ),
                    ),
            )
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_epoch = self.render_epoch.wrapping_add(1);
        self.guard_viewer_selection(window, cx);
        let workspace = v_flex().flex_1().min_w_0().h_full();
        let root = h_flex()
            .relative()
            .size_full()
            .bg(rgb(BG))
            .font_family("Noto Sans JP")
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| this.toggle_inspector(cx)))
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &OpenQuickLook, window, cx| {
                this.open_quick_look(window, cx, ViewerOpenSource::Auto)
            }))
            .on_action(cx.listener(|this, _: &CloseQuickLook, window, cx| {
                this.handle_viewer_escape(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ViewerPreviousPhoto, window, cx| {
                this.step_viewer_photo(-1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ViewerNextPhoto, window, cx| {
                this.step_viewer_photo(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &QuickLookNextFocus, window, cx| {
                this.cycle_quick_look_focus(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &QuickLookPrevFocus, window, cx| {
                this.cycle_quick_look_focus(true, window, cx)
            }))
            .child(self.sidebar(cx))
            .child(workspace.child(self.content(window, cx)))
            .child(self.nav_tooltip())
            .child(self.rail_toggle(cx));
        let root = if self.quick_look_open {
            root.child(self.quick_look(window, cx))
        } else {
            root
        };
        let root = if self.manual_compress.is_some() {
            root.child(self.manual_compression_overlay(cx))
        } else {
            root
        };
        let root = if self.import_dialog.is_some() {
            root.child(self.import_modal_overlay(cx))
        } else {
            root
        };
        root.child(self.viewer_morph_overlay())
    }
}

struct DemoAssets {
    #[allow(dead_code)] // Fixture root, kept for inspection and future previews.
    photo_dir: PathBuf,
    thumb_dir: PathBuf,
    preview_dir: PathBuf,
}

fn prepare_demo(reset: bool) -> Result<(Db, DemoAssets), String> {
    let root = std::env::temp_dir().join("vrc-photo-manager-demo");
    if reset {
        match std::fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "demo ディレクトリを削除できません ({}): {error}",
                    root.display()
                ));
            }
        }
    }
    let photo_dir = root.join("photos");
    let thumb_dir = root.join("thumbnails");
    let preview_dir = root.join("previews");
    std::fs::create_dir_all(&photo_dir).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&preview_dir).map_err(|error| error.to_string())?;
    let db = Db::open(root.join("library.db")).map_err(|error| error.to_string())?;
    if db.count_photos().map_err(|error| error.to_string())? == 0 {
        seed_demo(&db, &photo_dir)?;
    }
    Ok((
        db,
        DemoAssets {
            photo_dir,
            thumb_dir,
            preview_dir,
        },
    ))
}

fn demo_thumb_path(assets: &DemoAssets, photo_id: i64) -> PathBuf {
    assets.thumb_dir.join(format!("{photo_id}.png"))
}

fn empty_database() -> Db {
    Db::open_in_memory().expect("fallback in-memory database")
}

fn open_database(options: &LaunchOptions) -> (Db, bool, bool, Option<String>, Option<DemoAssets>) {
    if options.demo {
        return match prepare_demo(options.demo_reset) {
            Ok((db, assets)) => (db, true, false, None, Some(assets)),
            Err(error) => (
                empty_database(),
                true,
                false,
                Some(format!(
                    "デモデータを準備できませんでした（{error}）。空のライブラリで起動します。"
                )),
                None,
            ),
        };
    }
    let path = std::env::var("VRC_PHOTO_DB")
        .ok()
        .map(PathBuf::from)
        .or_else(vrc_photo_core::default_db_path)
        .unwrap_or_else(|| PathBuf::from("vrc-photo-manager.db"));
    match Db::open(&path) {
        Ok(db) => (db, false, true, None, None),
        Err(error) => {
            let message = format!("DBを開けませんでした（{error}）。デモデータで起動します。");
            match prepare_demo(true) {
                Ok((db, assets)) => (db, true, false, Some(message), Some(assets)),
                Err(demo_error) => (
                    empty_database(),
                    true,
                    false,
                    Some(format!(
                        "DBを開けませんでした（{error}）。デモデータも準備できませんでした（{demo_error}）。空のライブラリで起動します。"
                    )),
                    None,
                ),
            }
        }
    }
}

const DEMO_PALETTE: [u32; 8] = [
    0x1b1b3a, 0x2d1b4e, 0x5a2a5e, 0x8e3b5e, 0xc75a4a, 0xe08a4c, 0x4f6d8c, 0x22314f,
];

fn demo_image(index: usize) -> image::RgbaImage {
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 800;
    let top = DEMO_PALETTE[index % DEMO_PALETTE.len()];
    let bottom = DEMO_PALETTE[(index + 1) % DEMO_PALETTE.len()];
    let mut image = image::RgbaImage::from_fn(WIDTH, HEIGHT, |_, y| {
        let t = y as f32 / (HEIGHT - 1) as f32;
        let color = lerp_color(top, bottom, t);
        image::Rgba([
            ((color >> 16) & 0xff) as u8,
            ((color >> 8) & 0xff) as u8,
            (color & 0xff) as u8,
            255,
        ])
    });
    let sun_x = WIDTH as i64 * 7 / 10;
    let sun_y = HEIGHT as i64 * 3 / 10;
    let radius = 90i64;
    for y in (sun_y - radius).max(0)..=(sun_y + radius).min(HEIGHT as i64 - 1) {
        for x in (sun_x - radius).max(0)..=(sun_x + radius).min(WIDTH as i64 - 1) {
            let dx = x - sun_x;
            let dy = y - sun_y;
            if dx * dx + dy * dy <= radius * radius {
                image.put_pixel(x as u32, y as u32, image::Rgba([255, 210, 138, 255]));
            }
        }
    }
    let silhouette = image::Rgba([10, 14, 22, 255]);
    for x in 0..WIDTH {
        let wave = ((x as f32 * 0.013) + index as f32 * 1.7).sin() * 0.5 + 0.5;
        let ridge = ((x as f32 * 0.031) + index as f32 * 0.9).cos() * 0.5 + 0.5;
        let ridge_height = 36.0 + 96.0 * wave * ridge;
        let top = HEIGHT.saturating_sub(ridge_height as u32);
        for y in top..HEIGHT {
            image.put_pixel(x, y, silhouette);
        }
    }
    image
}

fn seed_demo(db: &Db, photo_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(photo_dir).map_err(|error| error.to_string())?;
    let thumb_dir = photo_dir
        .parent()
        .map(|root| root.join("thumbnails"))
        .unwrap_or_else(|| photo_dir.join("thumbnails"));
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    let root = db
        .add_root(&photo_dir.display().to_string(), "reference")
        .map_err(|error| error.to_string())?;
    let worlds = [
        "Midnight Rooftop",
        "The Black Cat",
        "Furality Aqua",
        "Sunset Bar",
        "Neon District",
        "Cozy Cabin",
        "Snowfield",
        "Deep Space",
    ];
    let names = ["Mio", "Kenta", "Lua", "Nox", "Sora", "Yui", "Aki", "Ren"];
    for index in 0..18 {
        let stamp = time::from_system_time(
            SystemTime::now() - Duration::from_secs(index as u64 * 3600 + 600),
        );
        let world = worlds[index % worlds.len()];
        let file_name = format!("VRChat_demo_{index}.png");
        let file_path = photo_dir.join(&file_name);
        demo_image(index)
            .save(&file_path)
            .map_err(|error| format!("{}: {error}", file_path.display()))?;
        let file_size = std::fs::metadata(&file_path)
            .map(|metadata| metadata.len())
            .map_err(|error| error.to_string())?;
        let id = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: file_name.clone(),
                abs_path: file_path.display().to_string(),
                file_name,
                captured_at_utc: stamp.utc,
                captured_at_local: stamp.local,
                tz_offset_sec: stamp.offset_sec,
                width: 1280,
                height: 800,
                file_size,
                mtime: stamp.utc,
                blake3: None,
            })
            .map_err(|error| error.to_string())?;
        let thumb_path = thumb_dir.join(format!("{id}.png"));
        image::open(&file_path)
            .map_err(|error| format!("{}: {error}", file_path.display()))?
            .thumbnail(256, 256)
            .save_with_format(&thumb_path, image::ImageFormat::Png)
            .map_err(|error| format!("{}: {error}", thumb_path.display()))?;
        let world_id = db
            .upsert_world(
                &format!("wrld_demo_{}", index % worlds.len()),
                Some(world),
                Some("demo"),
            )
            .map_err(|error| error.to_string())?;
        db.set_photo_context(&PhotoContext {
            photo_id: id,
            session_id: None,
            world_id: Some(world_id),
            world_name: Some(world.to_string()),
            instance_id: Some(format!("{}~region(jp)", 12000 + index)),
            link_method: "demo".to_string(),
            confidence: 1.0,
            author: Some("TommyZ".to_string()),
            linked_at: Utc::now().to_rfc3339(),
        })
        .map_err(|error| error.to_string())?;
        let players = (0..(3 + index % 5))
            .map(|offset| PhotoPlayer {
                user_id: format!("usr_{}", names[(index + offset) % names.len()]),
                display_name: names[(index + offset) % names.len()].to_string(),
            })
            .collect::<Vec<_>>();
        db.set_photo_players(id, &players)
            .map_err(|error| error.to_string())?;
        db.set_photo_tags(
            id,
            &[
                "夜".to_string(),
                world.split(' ').next().unwrap_or("VRChat").to_string(),
            ],
        )
        .map_err(|error| error.to_string())?;
        if index % 4 == 0 {
            db.set_favorite(id, true)
                .map_err(|error| error.to_string())?;
        }
        if index % 3 == 0 {
            db.add_derivative(
                id,
                &format!("/demo/compressed/{index}.webp"),
                "webp",
                1280,
                800,
                1_800_000,
                "demo",
                "keep",
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Trims the outer whitespace of a note and treats an empty result as "no note".
fn normalize_note(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Returns the tags left after removing `tag`, or `None` when it is absent.
fn without_tag(tags: &[String], tag: &str) -> Option<Vec<String>> {
    if !tags.iter().any(|existing| existing == tag) {
        return None;
    }
    Some(
        tags.iter()
            .filter(|existing| existing.as_str() != tag)
            .cloned()
            .collect(),
    )
}

/// Returns the tags with `tag` appended, or `None` when it is already present.
fn with_tag(tags: &[String], tag: &str) -> Option<Vec<String>> {
    if tags.iter().any(|existing| existing == tag) {
        return None;
    }
    let mut tags = tags.to_vec();
    tags.push(tag.to_string());
    Some(tags)
}

/// Whether an executed search would be affected by a tag edit.
fn has_tag_filter(filter: &PhotoTextFilter) -> bool {
    filter
        .tag
        .as_deref()
        .map(str::trim)
        .is_some_and(|tag| !tag.is_empty())
}

fn queue_status_badge(status: &QueueStatus) -> (&'static str, u32, u32) {
    match status {
        QueueStatus::Waiting => ("未実行", AMBER, 0x2a2412),
        QueueStatus::Pending => ("実行待ち", MUTED, PANEL_2),
        QueueStatus::Processing => ("処理中", BLUE, 0x14283d),
        QueueStatus::Completed => ("完了", GREEN, 0x112a22),
        QueueStatus::Failed => ("失敗", RED, 0x2a1414),
        QueueStatus::Cancelled => ("取消", MUTED, PANEL_2),
    }
}

/// Whether a failed job can return to the queue without exceeding the cap.
fn job_retry_available(job: &CompressionJob) -> bool {
    job.status == QueueStatus::Failed && job.retry_count < MAX_JOB_RETRIES
}

/// Retry usage for a failed job, or the cap explanation once exhausted.
fn job_retry_label(job: &CompressionJob) -> String {
    if job_retry_available(job) {
        format!("再試行 {}/{MAX_JOB_RETRIES}", job.retry_count)
    } else {
        format!("再試行の上限（{MAX_JOB_RETRIES}回）に達しました")
    }
}

/// Import failures and partial moves. Partial moves reached the destination,
/// so they are warnings and never count as failures.
fn import_issue_counts(issues: &[ImportIssue]) -> (usize, usize) {
    let failed = issues
        .iter()
        .filter(|issue| issue.kind == ImportIssueKind::Failed)
        .count();
    let partial = issues
        .iter()
        .filter(|issue| issue.kind == ImportIssueKind::PartialMove)
        .count();
    (failed, partial)
}

/// Sources a retry may try again: only `Failed` issues marked retryable.
fn retryable_sources(issues: &[ImportIssue]) -> Vec<PathBuf> {
    issues
        .iter()
        .filter(|issue| issue.kind == ImportIssueKind::Failed && issue.retryable)
        .map(|issue| issue.source.clone())
        .collect()
}

fn import_mode_label(mode: ImportMode) -> &'static str {
    match mode {
        ImportMode::Copy => "コピー",
        ImportMode::Move => "移動",
    }
}

/// Keeps every hard error that occurred while indexing the destination.
fn append_scan_error(report: &mut ImportReportView, message: String) {
    match &mut report.scan_error {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(&message);
        }
        None => report.scan_error = Some(message),
    }
}

/// Adds `dest` to the settings roots and persists the change. A failed save
/// restores the previous in-memory roots so a later retry attempts the save
/// again instead of treating an unsaved path as persisted.
fn add_photo_root_and_save<E, F>(
    settings: &mut AppSettings,
    dest: &str,
    save: F,
) -> std::result::Result<bool, E>
where
    E: std::fmt::Display,
    F: FnOnce(&AppSettings) -> std::result::Result<(), E>,
{
    if settings.photo_roots.iter().any(|root| root == dest) {
        return Ok(false);
    }
    let previous = settings.photo_roots.clone();
    settings.photo_roots.push(dest.to_string());
    match save(settings) {
        Ok(()) => Ok(true),
        Err(error) => {
            settings.photo_roots = previous;
            Err(error)
        }
    }
}

fn import_progress_label(progress: &ImportProgress) -> String {
    format!(
        "取り込み {}/{} · 取込 {} · スキップ {} · 失敗 {}",
        progress.processed, progress.total, progress.imported, progress.skipped, progress.failed
    )
}

fn import_progress_ratio(progress: &ImportProgress) -> f32 {
    if progress.total == 0 {
        0.0
    } else {
        (progress.processed as f32 / progress.total as f32).clamp(0.0, 1.0)
    }
}

fn import_remaining(files: &[PathBuf], processed: usize) -> Vec<PathBuf> {
    files
        .iter()
        .skip(processed.min(files.len()))
        .cloned()
        .collect()
}

fn import_should_index(imported: usize) -> bool {
    imported > 0
}

/// Claims the single import-dialog slot. A second open request while any run
/// is active is ignored so concurrent runs cannot target the same destination.
fn reserve_import_dialog(slot: &mut Option<ImportDialogState>, next: ImportDialogState) -> bool {
    if slot.is_some() {
        return false;
    }
    *slot = Some(next);
    true
}

fn merge_retry_report(
    previous: &ImportReportView,
    retry_sources: &[PathBuf],
    result: ImportResult,
) -> ImportReportView {
    let attempted: HashSet<&Path> = retry_sources
        .iter()
        .take(result.processed.min(retry_sources.len()))
        .map(PathBuf::as_path)
        .collect();
    let mut merged = previous.clone();
    merged.imported += result.imported.len();
    merged.skipped += result.skipped;
    merged.issues.retain(|issue| {
        !(issue.kind == ImportIssueKind::Failed
            && issue.retryable
            && attempted.contains(issue.source.as_path()))
    });
    merged.issues.extend(result.issues);
    merged.renamed.extend(result.renamed);
    merged
}

fn merge_resume_report(previous: &ImportReportView, result: ImportResult) -> ImportReportView {
    let mut merged = previous.clone();
    merged.imported += result.imported.len();
    merged.skipped += result.skipped;
    merged.processed += result.processed;
    merged.issues.extend(result.issues);
    merged.renamed.extend(result.renamed);
    let consumed = result.processed.min(previous.remaining.len());
    merged.remaining = previous.remaining.iter().skip(consumed).cloned().collect();
    merged.cancelled = !merged.remaining.is_empty();
    merged
}

fn import_summary(report: &ImportReportView) -> String {
    let (failed, _) = import_issue_counts(&report.issues);
    let mut details = Vec::new();
    if report.skipped > 0 {
        details.push(format!("{} 枚スキップ", report.skipped));
    }
    if failed > 0 {
        details.push(format!("失敗 {failed} 件"));
    }
    if !report.renamed.is_empty() {
        details.push(format!("連番 {} 件", report.renamed.len()));
    }
    let mut summary = format!(
        "{} 件中 {} 枚を取り込みました",
        report.total, report.imported
    );
    if !details.is_empty() {
        summary.push_str(&format!("（{}）", details.join("、")));
    }
    if !report.remaining.is_empty() {
        summary.push_str(&format!("。残り {} 件は未処理です", report.remaining.len()));
    }
    summary
}

/// Compact, ordered summaries of every failure that deserves a global alert.
/// The target screen owns the detail, so the bar only carries the count.
fn alert_summaries(
    failed_jobs: i64,
    daemon_errors: usize,
    import_failed: usize,
) -> Vec<(Screen, String)> {
    let mut rows = Vec::new();
    if failed_jobs > 0 {
        rows.push((
            Screen::Dashboard,
            format!("圧縮に {failed_jobs} 件の失敗があります"),
        ));
    }
    if daemon_errors > 0 {
        rows.push((
            Screen::Dashboard,
            format!("デーモンで {daemon_errors} 件の処理エラーがあります"),
        ));
    }
    if import_failed > 0 {
        rows.push((
            Screen::Photos,
            format!("取り込みで {import_failed} 件失敗しました"),
        ));
    }
    rows
}

fn daemon_liveness(age_secs: Option<i64>, ipc_connected: bool) -> bool {
    ipc_connected || age_secs.is_some_and(|age| age < 20)
}

fn daemon_state_label(active: bool, paused: bool) -> &'static str {
    if !active {
        "停止中"
    } else if paused {
        "一時停止中"
    } else {
        "稼働中"
    }
}

fn ipc_status_label(connected: bool, error: Option<&str>) -> String {
    if connected {
        "IPC 接続中".to_string()
    } else if let Some(error) = error {
        format!("IPC 未接続（{error}）")
    } else {
        "IPC 未接続".to_string()
    }
}

fn scan_progress_label(progress: &ScanProgressView) -> String {
    match progress.phase {
        IpcScanPhase::Walking => format!(
            "フォルダ {}/{} · 検索 {} 件",
            progress.root, progress.roots, progress.processed
        ),
        IpcScanPhase::Indexing => format!(
            "フォルダ {}/{} · 索引 {}/{}",
            progress.root,
            progress.roots,
            progress.processed,
            progress
                .total
                .map(|total| total.to_string())
                .unwrap_or_else(|| "?".to_string())
        ),
    }
}

fn scan_progress_ratio(progress: &ScanProgressView) -> f32 {
    let ratio = match progress.phase {
        IpcScanPhase::Walking => 0.0,
        IpcScanPhase::Indexing => progress
            .total
            .filter(|total| *total > 0)
            .map(|total| progress.processed as f32 / total as f32)
            .unwrap_or(0.0),
    };
    ratio.clamp(0.0, 1.0)
}

fn running_job_count(active: i64, waiting: i64) -> i64 {
    (active - waiting).max(0)
}

fn compression_card_status(active: i64, waiting: i64, failed: i64) -> CardStatus {
    let running = running_job_count(active, waiting);
    if waiting > 0 {
        CardStatus {
            headline: format!("未実行 {waiting}"),
            detail: format!("実行中 {running} · 失敗 {failed}"),
            tone: StatusTone::Waiting,
        }
    } else if running > 0 {
        CardStatus {
            headline: format!("実行中 {running}"),
            detail: format!("未実行 0 · 失敗 {failed}"),
            tone: StatusTone::Running,
        }
    } else if failed > 0 {
        CardStatus {
            headline: format!("失敗 {failed}"),
            detail: "未実行 0 · 実行中 0".to_string(),
            tone: StatusTone::Failed,
        }
    } else {
        CardStatus {
            headline: "キューは空です".to_string(),
            detail: "未実行のジョブはありません".to_string(),
            tone: StatusTone::Idle,
        }
    }
}

fn import_card_status(
    progress: Option<&ImportProgress>,
    report: Option<&ImportReportView>,
) -> CardStatus {
    if let Some(progress) = progress {
        return CardStatus {
            headline: "取り込み中".to_string(),
            detail: format!(
                "{}/{} · 取込 {} · 失敗 {}",
                progress.processed, progress.total, progress.imported, progress.failed
            ),
            tone: StatusTone::Running,
        };
    }
    let Some(report) = report else {
        return CardStatus {
            headline: "履歴なし".to_string(),
            detail: "取り込み結果がここに残ります".to_string(),
            tone: StatusTone::Idle,
        };
    };
    let (failed, partial) = import_issue_counts(&report.issues);
    let scan_failed = import_scan_failure_count(report);
    let failed_total = failed + scan_failed;
    if !report.remaining.is_empty() {
        return CardStatus {
            headline: format!("未処理 {}", report.remaining.len()),
            detail: format!(
                "取込 {}/{} · 失敗 {failed_total}",
                report.imported, report.total
            ),
            tone: StatusTone::Waiting,
        };
    }
    if failed_total > 0 {
        return CardStatus {
            headline: format!("失敗 {failed_total}"),
            detail: if scan_failed > 0 {
                format!(
                    "取込 {}/{} · スキャン失敗 {scan_failed}",
                    report.imported, report.total
                )
            } else {
                format!("取込 {}/{}", report.imported, report.total)
            },
            tone: StatusTone::Failed,
        };
    }
    if partial > 0 {
        return CardStatus {
            headline: format!("部分移動 {partial}"),
            detail: format!("取込 {}/{} · 完了", report.imported, report.total),
            tone: StatusTone::Waiting,
        };
    }
    CardStatus {
        headline: format!("完了 {} 枚", report.imported),
        detail: if report.renamed.is_empty() {
            "前回の取り込み".to_string()
        } else {
            format!("前回の取り込み · 連番 {}", report.renamed.len())
        },
        tone: StatusTone::Success,
    }
}

fn import_scan_failure_count(report: &ImportReportView) -> usize {
    report.scan_failures.len() + usize::from(report.scan_error.is_some())
}

fn scan_card_status(
    progress: Option<&ScanProgressView>,
    daemon_active: bool,
    daemon_paused: bool,
) -> CardStatus {
    if let Some(progress) = progress {
        let detail = scan_progress_label(progress);
        let has_total = progress.total.is_some_and(|total| total > 0);
        let headline = if progress.phase == IpcScanPhase::Indexing && has_total {
            format!(
                "{}%",
                (scan_progress_ratio(progress) * 100.0).round() as i32
            )
        } else {
            "走査中…".to_string()
        };
        return CardStatus {
            headline,
            detail,
            tone: StatusTone::Running,
        };
    }
    if !daemon_active {
        return CardStatus {
            headline: "停止中".to_string(),
            detail: "デーモンは接続されていません".to_string(),
            tone: StatusTone::Idle,
        };
    }
    if daemon_paused {
        return CardStatus {
            headline: "一時停止中".to_string(),
            detail: "監視を再開すると走査します".to_string(),
            tone: StatusTone::Waiting,
        };
    }
    CardStatus {
        headline: "待機中".to_string(),
        detail: "変更を監視しています".to_string(),
        tone: StatusTone::Idle,
    }
}

fn compression_attention(active: i64, waiting: i64, failed: i64) -> Option<StatusTone> {
    if failed > 0 {
        Some(StatusTone::Failed)
    } else if running_job_count(active, waiting) > 0 {
        Some(StatusTone::Running)
    } else if waiting > 0 {
        Some(StatusTone::Waiting)
    } else {
        None
    }
}

fn import_attention(
    progress: Option<&ImportProgress>,
    report: Option<&ImportReportView>,
) -> Option<StatusTone> {
    if progress.is_some() {
        return Some(StatusTone::Running);
    }
    let report = report?;
    let (failed, partial) = import_issue_counts(&report.issues);
    if failed > 0 || import_scan_failure_count(report) > 0 {
        Some(StatusTone::Failed)
    } else if !report.remaining.is_empty() || partial > 0 {
        Some(StatusTone::Waiting)
    } else {
        None
    }
}

fn scan_attention(
    progress: Option<&ScanProgressView>,
    daemon_active: bool,
    daemon_paused: bool,
) -> Option<StatusTone> {
    if progress.is_some() {
        Some(StatusTone::Running)
    } else if daemon_active && daemon_paused {
        Some(StatusTone::Waiting)
    } else {
        None
    }
}

fn import_running_progress(dialog: Option<&ImportDialogState>) -> Option<&ImportProgress> {
    match dialog.map(|dialog| &dialog.phase) {
        Some(ImportDialogPhase::Running { progress, .. }) => Some(progress),
        _ => None,
    }
}

fn daemon_sync_label(
    age_secs: Option<i64>,
    supervisor: Option<&str>,
    ipc_connected: bool,
) -> String {
    match age_secs.filter(|age| *age < 20) {
        Some(age) if supervisor == Some("systemd") => {
            format!("最終同期 {age}秒前 · systemd")
        }
        Some(age) => format!("最終同期 {age}秒前"),
        None if ipc_connected => "IPC 接続中".to_string(),
        None => "デーモン停止".to_string(),
    }
}

fn autostart_description(daemon_active: bool, supervisor: Option<&str>) -> &'static str {
    if daemon_active && supervisor == Some("systemd") {
        "systemd 管理で起動中。サービス利用時はOFF推奨"
    } else {
        "OSログイン時にデーモンを起動"
    }
}

fn daemon_operation_label(operation: DaemonOperation) -> &'static str {
    match operation {
        DaemonOperation::Scan => "写真スキャン",
        DaemonOperation::Log => "ログ解析",
        DaemonOperation::Link => "写真の紐付け",
    }
}

fn format_timestamp(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|timestamp| {
            timestamp
                .with_timezone(&chrono::Local)
                .format("%m/%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| raw.to_string())
}

fn nav_index(screen: Screen) -> usize {
    match screen {
        Screen::Photos => 0,
        Screen::Search => 1,
        Screen::Compression => 2,
        Screen::Dashboard => 3,
        Screen::Settings => 4,
    }
}

fn nav_item_top(index: usize) -> f32 {
    index as f32 * (NAV_ITEM_HEIGHT + NAV_ITEM_GAP)
}

fn nav_bar_left(collapsed: bool) -> f32 {
    if collapsed { 5.0 } else { -1.0 }
}

fn screen_direction(from: Screen, to: Screen) -> f32 {
    if nav_index(to) >= nav_index(from) {
        1.0
    } else {
        -1.0
    }
}

fn viewer_image_rect(
    viewport: gpui_kit::Size<Pixels>,
    details_open: bool,
    aspect: f32,
) -> Bounds<Pixels> {
    let details_width = if details_open {
        VIEWER_DETAILS_WIDTH
    } else {
        0.0
    };
    let (width, height) = viewer_image_box_with_details_width(
        viewport.width.into(),
        viewport.height.into(),
        details_width,
        aspect,
    );
    centered_viewer_rect(viewport, details_width, width, height)
}

/// Presentation metrics for one photo card and the uniform grid row it sits in.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CardMetrics {
    image_height: f32,
    row_height: f32,
    min_card_width: f32,
}

/// Derives the card layout from the display settings.
///
/// The text line heights are the values already implicit in the fixed 196 px
/// medium row: 128 image + 8 body gap + 18 world + 3 text gap + 15 meta + 24
/// slack. Hiding a text group removes its height so the grid tightens instead
/// of leaving a blank strip.
fn card_metrics(settings: &AppSettings) -> CardMetrics {
    let (image_height, min_card_width) = match settings.ui_card_size {
        CardSize::Small => (96.0, 140.0),
        CardSize::Medium => (128.0, MIN_CARD_WIDTH),
        CardSize::Large => (168.0, 220.0),
        CardSize::ExtraLarge => (208.0, 280.0),
    };
    let meta_line = settings.ui_card_show_date || settings.ui_card_show_size;
    let mut row_height = image_height + CARD_ROW_SLACK;
    if settings.ui_card_show_world || meta_line {
        row_height += CARD_BODY_GAP;
    }
    if settings.ui_card_show_world {
        row_height += CARD_WORLD_LINE_HEIGHT;
    }
    if meta_line {
        if settings.ui_card_show_world {
            row_height += CARD_TEXT_GAP;
        }
        row_height += CARD_META_LINE_HEIGHT;
    }
    CardMetrics {
        image_height,
        row_height,
        min_card_width,
    }
}

/// Stands in for a photo whose page has not loaded yet. Matches the card's
/// current image height so the uniform row never changes height.
fn grid_placeholder(image_height: f32) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .h(px(image_height))
        .rounded(px(14.))
        .bg(rgb(PANEL_2))
        .border_2()
        .border_color(rgb(BORDER))
        .into_any_element()
}

fn compression_policy_warning(keep_original: bool, auto_compress: bool) -> Option<&'static str> {
    (auto_compress && !keep_original).then_some(
        "自動圧縮で圧縮に成功すると元画像をOSゴミ箱へ移動します。取り消すにはゴミ箱から復元してください。",
    )
}

fn release_confirmation(keep_original: bool, count: i64) -> Option<String> {
    (!keep_original).then(|| {
        format!(
            "待機中の自動圧縮 {count} 件を実行し、成功した元画像をOSゴミ箱へ移動します。取り消すにはゴミ箱から復元してください。"
        )
    })
}

#[cfg(target_os = "windows")]
fn open_path(path: &Path) -> Result<(), String> {
    // explorer.exe reports a non-zero exit code even when it opens the folder,
    // so success is defined by the process starting.
    Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("explorer を起動できません: {error}"))
}

#[cfg(target_os = "macos")]
fn open_path(path: &Path) -> Result<(), String> {
    let status = Command::new("open")
        .arg(path)
        .status()
        .map_err(|error| format!("open を起動できません: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("open が終了コード {status} で失敗しました"))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_path(path: &Path) -> Result<(), String> {
    match open_status("xdg-open", &[], path) {
        Ok(()) => Ok(()),
        Err(first) => {
            open_status("gio", &["open"], path).map_err(|second| format!("{first} / {second}"))
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_status(program: &str, args: &[&str], path: &Path) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .map_err(|error| format!("{program} を起動できません: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} が終了コード {status} で失敗しました"))
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
fn open_path(path: &Path) -> Result<(), String> {
    Err(format!(
        "この環境ではフォルダを開けません: {}",
        path.display()
    ))
}

fn daemon_binary_path() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "vrc-photo-daemon.exe"
    } else {
        "vrc-photo-daemon"
    };
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|parent| parent.join(name))
}

fn spawn_daemon_process() -> Result<(), String> {
    let path = daemon_binary_path().ok_or_else(|| "daemon binary path not found".to_string())?;
    if !path.is_file() {
        return Err(format!("{} が見つかりません", path.display()));
    }
    Command::new(&path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn run_background_supervisor() -> Result<(), String> {
    let path = daemon_binary_path().ok_or_else(|| "daemon binary path not found".to_string())?;
    if !path.is_file() {
        return Err(format!("{} が見つかりません", path.display()));
    }
    let mut child = Command::new(&path)
        .spawn()
        .map_err(|error| error.to_string())?;
    child.wait().map_err(|error| error.to_string())?;
    Ok(())
}

fn register_global_actions(cx: &mut gpui_kit::App, app: Entity<App>) {
    let sidebar = app.downgrade();
    cx.on_action(move |_: &ToggleSidebar, cx| {
        sidebar.update(cx, |app, cx| app.toggle_sidebar(cx)).ok();
    });

    let inspector = app.downgrade();
    cx.on_action(move |_: &ToggleInspector, cx| {
        inspector
            .update(cx, |app, cx| app.toggle_inspector(cx))
            .ok();
    });

    let focus_search = app.downgrade();
    cx.on_action(move |_: &FocusSearch, cx| {
        let Some(window) = cx.active_window() else {
            return;
        };
        let focus_search = focus_search.clone();
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    focus_search
                        .update(cx, |app, cx| app.focus_search(window, cx))
                        .ok();
                })
                .ok();
        });
    });
}

fn main() {
    if logging::init("vrc-photo-manager", "info,vrc_photo_manager=debug").is_none() {
        tracing::warn!("file logging is unavailable; using stderr only");
    }
    if timing_enabled() {
        process_start();
    }
    let options = parse_args(std::env::args_os());
    if options.background {
        if let Err(error) = run_background_supervisor() {
            tracing::error!(%error, "background supervisor failed");
        }
        return;
    }
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            cx.text_system()
                .add_fonts(vec![Cow::Borrowed(
                    include_bytes!("../assets/NotoSansJP.ttf").as_slice(),
                )])
                .expect("load Noto Sans JP");
            gpui_kit::init(cx);
            // gpui_component::init forces the light theme; the app is dark-only.
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                KeyBinding::new("cmd-b", ToggleSidebar, None),
                KeyBinding::new("ctrl-i", ToggleInspector, None),
                KeyBinding::new("cmd-i", ToggleInspector, None),
                KeyBinding::new("ctrl-k", FocusSearch, None),
                KeyBinding::new("cmd-k", FocusSearch, None),
                KeyBinding::new("space", OpenQuickLook, Some("photo_grid")),
                KeyBinding::new("enter", OpenQuickLook, Some("photo_grid")),
                KeyBinding::new("escape", CloseQuickLook, Some("quick_look")),
                KeyBinding::new("left", ViewerPreviousPhoto, Some("viewer_nav")),
                KeyBinding::new("right", ViewerNextPhoto, Some("viewer_nav")),
                KeyBinding::new("tab", QuickLookNextFocus, Some("quick_look")),
                KeyBinding::new("shift-tab", QuickLookPrevFocus, Some("quick_look")),
            ]);
            let (width, height) = options.window_size;
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(width), px(height)), cx)),
                window_min_size: Some(size(px(1080.), px(700.))),
                ..Default::default()
            };
            let (_, app) = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.set_window_title("VRC Photo Manager");
                cx.new(|cx| App::new(window, cx, &options))
            })
            .expect("open window");
            register_global_actions(cx, app);
        });
}

#[cfg(test)]
mod tests {
    // `use super::*` would also import GPUI's `test` attribute macro through
    // `gpui_kit::*`, shadowing the built-in `#[test]` and recursing forever.
    use super::{
        AMBER, BLUE, GREEN, MUTED_2, RED, StatusTone, compression_attention,
        compression_card_status, import_attention, import_card_status, import_running_progress,
        running_job_count, scan_attention, scan_card_status,
    };
    use super::{
        DemoAssets, ImportDialogOrigin, ImportDialogPhase, ImportDialogState, ImportReportView,
        IpcScanPhase, MIN_CARD_WIDTH, ManualOutputTarget, NAV_ITEM_GAP, NAV_ITEM_HEIGHT,
        PHOTO_ROW_HEIGHT, PhotoBrowser, PhotoIndex, ScanProgressView, Screen,
        add_photo_root_and_save, alert_summaries, anchor_photo_id, autostart_description,
        card_metrics, compression_policy_warning, daemon_liveness, daemon_operation_label,
        daemon_state_label, daemon_sync_label, demo_thumb_path, format_timestamp, has_tag_filter,
        import_issue_counts, import_progress_label, import_progress_ratio, import_remaining,
        import_should_index, import_summary, ipc_status_label, job_retry_available,
        job_retry_label, list_scroll_top, manual_output_available, merge_resume_report,
        merge_retry_report, nav_index, nav_item_top, normalize_note, queue_status_badge,
        release_confirmation, reserve_import_dialog, retryable_sources, scan_progress_label,
        scan_progress_ratio, screen_direction, search_anchor_after_result, viewer_image_rect,
        with_tag, without_tag,
    };
    use crate::state::photo_browser::first_visible_photo;
    use crate::state::test_support::{index_entry, test_photo};
    use crate::state::year_rail::rail_scroll_offset;
    use gpui_kit::{ScrollStrategy, UniformListScrollHandle, point, px, size};
    use std::cell::Cell;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use vrc_photo_core::importer::{
        ImportIssue, ImportIssueKind, ImportMode, ImportProgress, ImportRename, ImportResult,
    };
    use vrc_photo_core::model::{CompressionJob, DaemonOperation, PhotoTextFilter, QueueStatus};
    use vrc_photo_core::scanner::ScanFailure;
    use vrc_photo_core::settings::{AppSettings, CardSize};

    #[test]
    fn alert_summaries_follow_priority_order() {
        let rows = alert_summaries(2, 1, 3);
        assert_eq!(rows[0].0, Screen::Dashboard);
        assert!(rows[0].1.contains("2"));
        assert_eq!(rows[1].0, Screen::Dashboard);
        assert_eq!(rows[2].0, Screen::Photos);
        assert!(alert_summaries(0, 0, 0).is_empty());
    }

    #[test]
    fn dashboard_replaces_the_monitor_screen() {
        assert_eq!(Screen::Dashboard.title(), "ダッシュボード");
        assert_eq!(alert_summaries(0, 1, 0)[0].0, Screen::Dashboard);
    }

    #[test]
    fn daemon_operation_labels_cover_every_operation() {
        assert_eq!(
            daemon_operation_label(DaemonOperation::Scan),
            "写真スキャン"
        );
        assert_eq!(daemon_operation_label(DaemonOperation::Log), "ログ解析");
        assert_eq!(
            daemon_operation_label(DaemonOperation::Link),
            "写真の紐付け"
        );
    }

    #[test]
    fn timestamp_formats_rfc3339_and_falls_back_to_raw() {
        let raw = "2026-10-23T03:04:05+00:00";
        let expected = chrono::DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m/%d %H:%M")
            .to_string();
        assert_eq!(format_timestamp(raw), expected);
        assert_eq!(format_timestamp("not a timestamp"), "not a timestamp");
    }

    #[test]
    fn import_issue_counts_separate_failures_and_partial_moves() {
        let issues = vec![
            ImportIssue {
                source: PathBuf::from("/a.png"),
                target: None,
                message: "copy failed".to_string(),
                kind: ImportIssueKind::Failed,
                retryable: true,
            },
            ImportIssue {
                source: PathBuf::from("/b.png"),
                target: Some(PathBuf::from("/dest/b.png")),
                message: "source remains".to_string(),
                kind: ImportIssueKind::PartialMove,
                retryable: false,
            },
        ];
        assert_eq!(import_issue_counts(&issues), (1, 1));
        assert_eq!(retryable_sources(&issues), vec![PathBuf::from("/a.png")]);
    }

    #[test]
    fn failed_import_root_save_is_retried_until_it_succeeds() {
        let mut settings = AppSettings::default();
        let attempts = Cell::new(0);
        let dest = "/photos/imported";

        let result = add_photo_root_and_save(&mut settings, dest, |settings| {
            attempts.set(attempts.get() + 1);
            assert_eq!(settings.photo_roots, vec![dest.to_string()]);
            Err::<(), _>("database is locked".to_string())
        });
        assert!(result.is_err());
        assert!(
            settings.photo_roots.is_empty(),
            "a failed save must restore the in-memory roots"
        );

        let result = add_photo_root_and_save(&mut settings, dest, |settings| {
            attempts.set(attempts.get() + 1);
            assert_eq!(settings.photo_roots, vec![dest.to_string()]);
            Ok::<(), String>(())
        });
        assert_eq!(result, Ok(true));
        assert_eq!(attempts.get(), 2, "the retry must save again");
        assert_eq!(settings.photo_roots, vec![dest.to_string()]);

        let result: Result<bool, String> = add_photo_root_and_save(&mut settings, dest, |_| {
            panic!("an already saved root must not be saved again")
        });
        assert_eq!(result, Ok(false));
    }

    #[test]
    fn nav_index_matches_sidebar_order() {
        assert_eq!(nav_index(Screen::Photos), 0);
        assert_eq!(nav_index(Screen::Search), 1);
        assert_eq!(nav_index(Screen::Compression), 2);
        assert_eq!(nav_index(Screen::Dashboard), 3);
        assert_eq!(nav_index(Screen::Settings), 4);
    }

    #[test]
    fn nav_item_top_uses_item_height_and_gap() {
        assert_eq!(nav_item_top(0), 0.0);
        assert_eq!(nav_item_top(1), NAV_ITEM_HEIGHT + NAV_ITEM_GAP);
        assert_eq!(nav_item_top(4), 4.0 * (NAV_ITEM_HEIGHT + NAV_ITEM_GAP));
    }

    #[test]
    fn screen_direction_follows_sidebar_order() {
        assert_eq!(screen_direction(Screen::Photos, Screen::Settings), 1.0);
        assert_eq!(screen_direction(Screen::Settings, Screen::Photos), -1.0);
        assert_eq!(screen_direction(Screen::Search, Screen::Search), 1.0);
    }

    #[test]
    fn viewer_image_rect_centers_the_box_and_respects_details() {
        let viewport = size(px(1200.), px(800.));
        let closed = viewer_image_rect(viewport, false, 16.0 / 10.0);
        let open = viewer_image_rect(viewport, true, 16.0 / 10.0);
        assert!((f32::from(closed.center().x) - 600.0).abs() < 0.01);
        assert!((f32::from(closed.center().y) - 428.0).abs() < 0.01);
        assert!((f32::from(open.center().x) - 410.0).abs() < 0.01);
        assert!((f32::from(open.center().y) - 428.0).abs() < 0.01);
        assert!(closed.size.width > open.size.width);
    }

    #[test]
    fn demo_thumb_path_uses_png_in_demo_dir() {
        let assets = DemoAssets {
            photo_dir: std::path::PathBuf::from("/tmp/vrc-demo/photos"),
            thumb_dir: std::path::PathBuf::from("/tmp/vrc-demo/thumbnails"),
            preview_dir: std::path::PathBuf::from("/tmp/vrc-demo/previews"),
        };
        let path = demo_thumb_path(&assets, 42);
        assert!(path.ends_with("42.png"));
        assert_eq!(path.parent(), Some(assets.thumb_dir.as_path()));
    }

    #[test]
    fn anchor_photo_id_handles_empty_and_overscrolled_indexes() {
        assert_eq!(
            anchor_photo_id(&PhotoIndex::new(Vec::new()), 0.0, 3, PHOTO_ROW_HEIGHT),
            None
        );
        let index = PhotoIndex::new(vec![index_entry(1, 202601), index_entry(2, 202512)]);
        assert_eq!(
            anchor_photo_id(&index, PHOTO_ROW_HEIGHT * 50.0, 4, PHOTO_ROW_HEIGHT),
            Some(2)
        );
    }

    #[test]
    fn first_visible_photo_floors_the_row_before_applying_columns() {
        // Halfway into row 2 with 3 columns: the top row starts at entry 6.
        // Truncating after multiplying (`2.5 * 3 = 7.5`) would pick entry 7.
        assert_eq!(
            first_visible_photo(PHOTO_ROW_HEIGHT * 2.5, 3, PHOTO_ROW_HEIGHT, 100),
            6
        );
        assert_eq!(first_visible_photo(0.0, 3, PHOTO_ROW_HEIGHT, 100), 0);
        assert_eq!(first_visible_photo(-20.0, 3, PHOTO_ROW_HEIGHT, 100), 0);
        assert_eq!(
            first_visible_photo(PHOTO_ROW_HEIGHT * 50.0, 3, PHOTO_ROW_HEIGHT, 10),
            9
        );
        assert_eq!(first_visible_photo(0.0, 3, PHOTO_ROW_HEIGHT, 0), 0);
    }

    #[test]
    fn anchor_row_remaps_after_an_insertion_before_it() {
        let original = PhotoIndex::new(vec![index_entry(10, 202601), index_entry(11, 202512)]);
        assert_eq!(original.row_of_id(11), Some(1));
        let updated = PhotoIndex::new(vec![
            index_entry(12, 202601),
            index_entry(10, 202601),
            index_entry(11, 202512),
        ]);
        assert_eq!(updated.row_of_id(11), Some(2));
        assert_eq!(
            anchor_photo_id(&updated, PHOTO_ROW_HEIGHT * 2.0 + 1.0, 1, PHOTO_ROW_HEIGHT),
            Some(11)
        );
    }

    #[test]
    fn grid_row_height_matches_the_card_height() {
        assert_eq!(PHOTO_ROW_HEIGHT, 196.0);
    }

    #[test]
    fn default_card_metrics_match_the_current_grid() {
        let metrics = card_metrics(&AppSettings::default());
        assert_eq!(metrics.image_height, 128.0);
        assert_eq!(metrics.row_height, PHOTO_ROW_HEIGHT);
        assert_eq!(metrics.min_card_width, MIN_CARD_WIDTH);
    }

    #[test]
    fn hidden_card_text_shrinks_the_row_but_keeps_the_image() {
        let settings = AppSettings {
            ui_card_show_world: false,
            ui_card_show_date: false,
            ui_card_show_size: false,
            ..Default::default()
        };
        let metrics = card_metrics(&settings);
        assert_eq!(metrics.image_height, 128.0);
        assert_eq!(metrics.row_height, 152.0);
    }

    #[test]
    fn card_metrics_count_only_visible_text_lines() {
        let world_only = AppSettings {
            ui_card_show_date: false,
            ui_card_show_size: false,
            ..Default::default()
        };
        assert_eq!(card_metrics(&world_only).row_height, 178.0);

        let meta_only = AppSettings {
            ui_card_show_world: false,
            ..Default::default()
        };
        assert_eq!(card_metrics(&meta_only).row_height, 175.0);

        let status_hidden = AppSettings {
            ui_card_show_status: false,
            ..Default::default()
        };
        assert_eq!(card_metrics(&status_hidden).row_height, PHOTO_ROW_HEIGHT);
    }

    #[test]
    fn card_size_presets_scale_the_grid() {
        let expected = [
            (CardSize::Small, 96.0, 140.0),
            (CardSize::Medium, 128.0, 170.0),
            (CardSize::Large, 168.0, 220.0),
            (CardSize::ExtraLarge, 208.0, 280.0),
        ];
        for (size, image, min_width) in expected {
            let metrics = card_metrics(&AppSettings {
                ui_card_size: size,
                ..Default::default()
            });
            assert_eq!(metrics.image_height, image);
            assert_eq!(metrics.min_card_width, min_width);
            assert_eq!(metrics.row_height, image + 68.0);
        }
    }

    #[test]
    fn anchor_and_rail_helpers_accept_a_custom_row_height() {
        let index = PhotoIndex::new(vec![
            index_entry(1, 202601),
            index_entry(2, 202601),
            index_entry(3, 202601),
        ]);
        assert_eq!(anchor_photo_id(&index, 153.0, 2, 152.0), Some(3));
        assert_eq!(rail_scroll_offset(3, 3, 152.0, 10_000.0), -152.0);
    }

    #[test]
    fn removing_a_photo_clears_pages_and_rebuilds_year_counts() {
        let entries = vec![
            index_entry(1, 202601),
            index_entry(2, 202601),
            index_entry(3, 202412),
        ];
        let mut browser = PhotoBrowser::new(Arc::new(PhotoIndex::new(entries)));
        let generation = browser.generation;
        browser.insert_page(0, vec![test_photo(1)], generation);
        browser.remove_photo(2);
        assert_eq!(browser.index.total, 2);
        assert_eq!(browser.index.years[0].count, 1);
        assert_eq!(browser.index.years[1].count, 1);
        assert!(browser.pages.get_page(0).is_none());
        assert_eq!(browser.index.row_of_id(1), Some(0));
        assert_eq!(browser.index.row_of_id(2), None);
        // Removing an id that is already gone is a no-op.
        browser.remove_photo(2);
        assert_eq!(browser.index.total, 2);
    }

    #[test]
    fn removing_a_photo_clamps_a_scroll_past_the_last_row() {
        let entries = (1..=10).map(|id| index_entry(id, 202601)).collect();
        let mut browser = PhotoBrowser::new(Arc::new(PhotoIndex::new(entries)));
        browser.columns = 2;
        browser
            .scroll
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(-PHOTO_ROW_HEIGHT * 10.0)));
        browser.remove_photo(1);

        let state = browser.scroll.0.borrow();
        let deferred = state.deferred_scroll_to_item.expect("scroll clamp");
        // 9 photos over 2 columns is 5 rows; the last row index is 4.
        assert_eq!(deferred.item_index, 4);
        assert_eq!(deferred.strategy, ScrollStrategy::Nearest);
    }

    #[test]
    fn list_scroll_top_reads_the_uniform_list_offset() {
        let handle = UniformListScrollHandle::new();
        handle
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(-392.)));
        assert_eq!(list_scroll_top(&handle), 392.0);
        handle
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(12.)));
        assert_eq!(list_scroll_top(&handle), 0.0);
    }

    #[test]
    fn notes_normalize_empty_to_none_and_trim_text() {
        assert_eq!(normalize_note(""), None);
        assert_eq!(normalize_note("  \n\t "), None);
        assert_eq!(normalize_note(" hello \n"), Some("hello"));
        assert_eq!(normalize_note("line1\nline2"), Some("line1\nline2"));
    }

    #[test]
    fn tag_removal_filters_only_the_named_tag() {
        let tags = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(without_tag(&tags, "alpha"), Some(vec!["beta".to_string()]));
        assert_eq!(without_tag(&tags, "missing"), None);
    }

    #[test]
    fn tag_addition_rejects_duplicates() {
        let tags = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(
            with_tag(&tags, "gamma"),
            Some(vec![
                "alpha".to_string(),
                "beta".to_string(),
                "gamma".to_string()
            ])
        );
        assert_eq!(with_tag(&tags, "alpha"), None);
    }

    #[test]
    fn search_anchor_after_result_keeps_existing_or_falls_back_to_first() {
        let index = PhotoIndex::new(vec![index_entry(1, 202601), index_entry(2, 202512)]);
        assert_eq!(search_anchor_after_result(Some(2), &index), Some(2));
        assert_eq!(search_anchor_after_result(Some(9), &index), Some(1));
        assert_eq!(search_anchor_after_result(None, &index), Some(1));
        assert_eq!(
            search_anchor_after_result(Some(1), &PhotoIndex::new(Vec::new())),
            None
        );
    }

    #[test]
    fn search_refresh_requires_a_non_empty_tag_filter() {
        assert!(!has_tag_filter(&PhotoTextFilter::default()));
        assert!(!has_tag_filter(&PhotoTextFilter {
            tag: Some("  ".to_string()),
            ..Default::default()
        }));
        assert!(has_tag_filter(&PhotoTextFilter {
            tag: Some("vrc".to_string()),
            ..Default::default()
        }));
    }

    fn test_job(status: QueueStatus, retry_count: i64) -> CompressionJob {
        CompressionJob {
            id: "job".to_string(),
            photo_id: 1,
            profile_id: "p".to_string(),
            status,
            progress: 0.0,
            error: Some("boom".to_string()),
            output_path: None,
            policy: "keep".to_string(),
            output_dir: None,
            created_at: "now".to_string(),
            started_at: None,
            finished_at: None,
            retry_count,
        }
    }

    #[test]
    fn job_retry_availability_stops_at_the_cap() {
        assert!(job_retry_available(&test_job(QueueStatus::Failed, 0)));
        assert!(job_retry_available(&test_job(QueueStatus::Failed, 2)));
        assert!(!job_retry_available(&test_job(QueueStatus::Failed, 3)));
        assert!(!job_retry_available(&test_job(QueueStatus::Completed, 0)));
        assert!(job_retry_label(&test_job(QueueStatus::Failed, 3)).contains("上限"));
    }

    #[test]
    fn queue_status_badge_distinguishes_waiting_and_pending() {
        assert_eq!(queue_status_badge(&QueueStatus::Waiting).0, "未実行");
        assert_eq!(queue_status_badge(&QueueStatus::Pending).0, "実行待ち");
        assert_eq!(queue_status_badge(&QueueStatus::Processing).0, "処理中");
        assert_eq!(queue_status_badge(&QueueStatus::Completed).0, "完了");
        assert_eq!(queue_status_badge(&QueueStatus::Failed).0, "失敗");
        assert_eq!(queue_status_badge(&QueueStatus::Cancelled).0, "取消");
    }

    #[test]
    fn clipboard_output_is_limited_to_one_photo() {
        assert!(manual_output_available(1, ManualOutputTarget::Clipboard));
        assert!(!manual_output_available(2, ManualOutputTarget::Clipboard));
        assert!(manual_output_available(2, ManualOutputTarget::Folder));
        assert!(manual_output_available(2, ManualOutputTarget::Profile));
    }

    #[test]
    fn compression_policy_warning_is_limited_to_automatic_compression() {
        assert!(compression_policy_warning(true, true).is_none());
        assert!(compression_policy_warning(true, false).is_none());
        assert!(compression_policy_warning(false, false).is_none());

        let automatic = compression_policy_warning(false, true).unwrap();
        assert!(automatic.contains("自動圧縮"));
        assert!(automatic.contains("OSゴミ箱"));
    }

    #[test]
    fn release_confirmation_only_appears_for_replace() {
        assert!(release_confirmation(true, 3).is_none());
        let replace = release_confirmation(false, 3).unwrap();
        assert!(replace.contains("3 件"));
        assert!(replace.contains("OSゴミ箱"));
    }

    #[test]
    fn scan_progress_label_uses_root_and_counts() {
        let progress = ScanProgressView {
            phase: IpcScanPhase::Indexing,
            processed: 50,
            total: Some(100),
            indexed: 49,
            root: 2,
            roots: 3,
        };
        assert_eq!(scan_progress_label(&progress), "フォルダ 2/3 · 索引 50/100");
        assert_eq!(scan_progress_ratio(&progress), 0.5);
    }

    #[test]
    fn ipc_status_label_distinguishes_disconnected_and_mismatch() {
        assert_eq!(ipc_status_label(true, None), "IPC 接続中");
        assert!(ipc_status_label(false, Some("protocol mismatch")).contains("未接続"));
    }

    #[test]
    fn daemon_state_label_prefers_stopped_over_paused() {
        assert_eq!(daemon_state_label(false, true), "停止中");
        assert_eq!(daemon_state_label(true, true), "一時停止中");
        assert_eq!(daemon_state_label(true, false), "稼働中");
    }

    #[test]
    fn daemon_liveness_uses_heartbeat_or_ipc() {
        assert!(daemon_liveness(Some(3), false));
        assert!(daemon_liveness(None, true));
        assert!(!daemon_liveness(Some(30), false));
        assert!(!daemon_liveness(None, false));
    }

    #[test]
    fn daemon_sync_label_includes_systemd_supervisor() {
        assert_eq!(
            daemon_sync_label(Some(5), Some("systemd"), false),
            "最終同期 5秒前 · systemd"
        );
        assert_eq!(
            daemon_sync_label(Some(5), Some("app"), false),
            "最終同期 5秒前"
        );
        assert_eq!(
            daemon_sync_label(Some(30), Some("systemd"), false),
            "デーモン停止"
        );
        assert_eq!(daemon_sync_label(None, None, false), "デーモン停止");
        assert_eq!(daemon_sync_label(None, None, true), "IPC 接続中");
        assert_eq!(daemon_sync_label(Some(30), None, true), "IPC 接続中");
    }

    #[test]
    fn autostart_description_mentions_systemd_only_while_active() {
        assert_eq!(
            autostart_description(true, Some("systemd")),
            "systemd 管理で起動中。サービス利用時はOFF推奨"
        );
        assert_eq!(
            autostart_description(false, Some("systemd")),
            "OSログイン時にデーモンを起動"
        );
        assert_eq!(
            autostart_description(true, Some("app")),
            "OSログイン時にデーモンを起動"
        );
    }

    fn progress_fixture(processed: usize, total: usize) -> ImportProgress {
        ImportProgress {
            processed,
            total,
            imported: processed.saturating_sub(1),
            skipped: 0,
            failed: 0,
            partial: 0,
            last_source: None,
        }
    }

    fn import_report_fixture() -> ImportReportView {
        ImportReportView {
            mode: ImportMode::Copy,
            dest: PathBuf::from("/dest"),
            total: 0,
            processed: 0,
            imported: 0,
            skipped: 0,
            issues: Vec::new(),
            renamed: Vec::new(),
            remaining: Vec::new(),
            cancelled: false,
            scan_failures: Vec::new(),
            scan_error: None,
        }
    }

    #[test]
    fn import_progress_label_and_ratio_use_processed_and_total() {
        let mut progress = progress_fixture(42, 100);
        progress.imported = 40;
        progress.skipped = 1;
        progress.failed = 1;

        assert_eq!(
            import_progress_label(&progress),
            "取り込み 42/100 · 取込 40 · スキップ 1 · 失敗 1"
        );
        let ratio = import_progress_ratio(&progress);
        assert!((ratio - 0.42).abs() < 1e-6);

        let empty = progress_fixture(0, 0);
        assert_eq!(
            import_progress_label(&empty),
            "取り込み 0/0 · 取込 0 · スキップ 0 · 失敗 0"
        );
        assert_eq!(import_progress_ratio(&empty), 0.0);
    }

    #[test]
    fn import_remaining_slices_from_the_processed_prefix() {
        let files = vec![
            PathBuf::from("/a.png"),
            PathBuf::from("/b.png"),
            PathBuf::from("/c.png"),
        ];
        assert_eq!(import_remaining(&files, 0), files);
        assert_eq!(import_remaining(&files, 2), vec![PathBuf::from("/c.png")]);
        assert_eq!(import_remaining(&files, 9), Vec::<PathBuf>::new());
    }

    #[test]
    fn import_index_gate_requires_at_least_one_imported_file() {
        assert!(!import_should_index(0));
        assert!(import_should_index(1));
    }

    fn import_dialog_fixture() -> ImportDialogState {
        ImportDialogState {
            origin: ImportDialogOrigin::New,
            mode: ImportMode::Copy,
            dest: PathBuf::from("/dest"),
            source: None,
            error: None,
            phase: ImportDialogPhase::Ready,
        }
    }

    #[test]
    fn import_dialog_reservation_rejects_a_second_open() {
        let mut slot = None;
        assert!(reserve_import_dialog(&mut slot, import_dialog_fixture()));
        assert!(slot.is_some());
        assert!(!reserve_import_dialog(&mut slot, import_dialog_fixture()));
        assert!(slot.is_some());
    }

    fn retryable_issue(source: &str) -> ImportIssue {
        ImportIssue {
            source: PathBuf::from(source),
            target: None,
            message: "copy failed".to_string(),
            kind: ImportIssueKind::Failed,
            retryable: true,
        }
    }

    fn non_retryable_issue(source: &str) -> ImportIssue {
        ImportIssue {
            retryable: false,
            ..retryable_issue(source)
        }
    }

    fn partial_issue(source: &str) -> ImportIssue {
        ImportIssue {
            kind: ImportIssueKind::PartialMove,
            ..non_retryable_issue(source)
        }
    }

    fn rename_fixture(source: &str, target: &str) -> ImportRename {
        ImportRename {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
        }
    }

    #[test]
    fn retry_merge_drops_attempted_retryable_failures_and_keeps_the_rest() {
        let previous = ImportReportView {
            total: 3,
            processed: 3,
            imported: 1,
            issues: vec![
                retryable_issue("/a.png"),
                non_retryable_issue("/b.png"),
                partial_issue("/c.png"),
            ],
            renamed: vec![rename_fixture("/old.png", "/dest/old-1.png")],
            scan_failures: vec![ScanFailure {
                path: PathBuf::from("/dest"),
                message: "scan failed".to_string(),
            }],
            scan_error: Some("scan error".to_string()),
            ..import_report_fixture()
        };
        let result = ImportResult {
            processed: 1,
            imported: vec![PathBuf::from("/dest/a.png")],
            renamed: vec![rename_fixture("/a.png", "/dest/a-1.png")],
            ..ImportResult::default()
        };

        let merged = merge_retry_report(
            &previous,
            &[PathBuf::from("/a.png"), PathBuf::from("/d.png")],
            result,
        );

        assert_eq!(merged.imported, 2);
        assert_eq!(merged.processed, 3);
        assert_eq!(merged.issues.len(), 2);
        assert!(
            merged
                .issues
                .iter()
                .any(|issue| issue.source.as_path() == Path::new("/b.png"))
        );
        assert!(
            merged
                .issues
                .iter()
                .any(|issue| issue.source.as_path() == Path::new("/c.png"))
        );
        assert!(
            !merged
                .issues
                .iter()
                .any(|issue| issue.source.as_path() == Path::new("/a.png"))
        );
        assert_eq!(merged.renamed.len(), 2);
        assert!(merged.scan_error.is_some());
        assert_eq!(merged.scan_failures.len(), 1);
    }

    #[test]
    fn cancelled_retry_keeps_unattempted_retryable_failures() {
        let previous = ImportReportView {
            issues: vec![retryable_issue("/a.png"), retryable_issue("/d.png")],
            ..import_report_fixture()
        };
        let result = ImportResult {
            processed: 1,
            issues: vec![retryable_issue("/a.png")],
            ..ImportResult::default()
        };

        let merged = merge_retry_report(
            &previous,
            &[PathBuf::from("/a.png"), PathBuf::from("/d.png")],
            result,
        );

        assert_eq!(merged.issues.len(), 2);
        assert!(
            merged
                .issues
                .iter()
                .any(|issue| issue.source.as_path() == Path::new("/d.png"))
        );
        assert_eq!(
            retryable_sources(&merged.issues),
            vec![PathBuf::from("/d.png"), PathBuf::from("/a.png")]
        );
    }

    #[test]
    fn resume_merge_advances_progress_and_slices_remaining() {
        let previous = ImportReportView {
            total: 5,
            processed: 2,
            imported: 2,
            remaining: vec![
                PathBuf::from("/c.png"),
                PathBuf::from("/d.png"),
                PathBuf::from("/e.png"),
            ],
            cancelled: true,
            ..import_report_fixture()
        };
        let result = ImportResult {
            processed: 2,
            imported: vec![PathBuf::from("/dest/c.png")],
            skipped: 1,
            ..ImportResult::default()
        };

        let merged = merge_resume_report(&previous, result);

        assert_eq!(merged.processed, 4);
        assert_eq!(merged.imported, 3);
        assert_eq!(merged.skipped, 1);
        assert_eq!(merged.remaining, vec![PathBuf::from("/e.png")]);
        assert!(merged.cancelled);
    }

    #[test]
    fn resume_merge_clears_cancelled_when_all_remaining_sources_ran() {
        let previous = ImportReportView {
            total: 2,
            processed: 1,
            imported: 1,
            remaining: vec![PathBuf::from("/b.png")],
            cancelled: true,
            ..import_report_fixture()
        };
        let result = ImportResult {
            processed: 1,
            imported: vec![PathBuf::from("/dest/b.png")],
            ..ImportResult::default()
        };

        let merged = merge_resume_report(&previous, result);

        assert_eq!(merged.processed, 2);
        assert_eq!(merged.imported, 2);
        assert!(merged.remaining.is_empty());
        assert!(!merged.cancelled);
    }

    #[test]
    fn import_summary_reports_a_completed_run() {
        let report = ImportReportView {
            total: 3,
            imported: 3,
            ..import_report_fixture()
        };

        assert_eq!(import_summary(&report), "3 件中 3 枚を取り込みました");
    }

    #[test]
    fn import_summary_includes_total_failures_renames_and_remaining() {
        let report = ImportReportView {
            total: 100,
            processed: 40,
            imported: 40,
            skipped: 1,
            issues: vec![non_retryable_issue("/bad.png")],
            renamed: vec![rename_fixture("/old.png", "/dest/old-1.png")],
            remaining: vec![PathBuf::from("/next.png")],
            cancelled: true,
            ..import_report_fixture()
        };

        assert_eq!(
            import_summary(&report),
            "100 件中 40 枚を取り込みました（1 枚スキップ、失敗 1 件、連番 1 件）。残り 1 件は未処理です"
        );
    }

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

        let running = compression_card_status(3, 0, 0);
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
    fn import_card_status_surfaces_scan_failures_and_unprocessed_priority() {
        let scan_only = ImportReportView {
            total: 10,
            processed: 10,
            imported: 10,
            scan_failures: vec![ScanFailure {
                path: PathBuf::from("/dest"),
                message: "scan failed".to_string(),
            }],
            ..import_report_fixture()
        };
        let scan_only_status = import_card_status(None, Some(&scan_only));
        assert_eq!(scan_only_status.headline, "失敗 1");
        assert_eq!(scan_only_status.detail, "取込 10/10 · スキャン失敗 1");
        assert_eq!(scan_only_status.tone, StatusTone::Failed);
        assert_eq!(
            import_attention(None, Some(&scan_only)),
            Some(StatusTone::Failed)
        );

        let hard_error = ImportReportView {
            scan_error: Some("cannot index".to_string()),
            ..import_report_fixture()
        };
        let hard_error_status = import_card_status(None, Some(&hard_error));
        assert_eq!(hard_error_status.headline, "失敗 1");
        assert_eq!(hard_error_status.detail, "取込 0/0 · スキャン失敗 1");
        assert_eq!(hard_error_status.tone, StatusTone::Failed);

        let triple = ImportReportView {
            total: 10,
            processed: 8,
            imported: 7,
            issues: vec![non_retryable_issue("/a.png"), partial_issue("/b.png")],
            remaining: vec![PathBuf::from("/c.png")],
            scan_failures: vec![ScanFailure {
                path: PathBuf::from("/dest"),
                message: "scan failed".to_string(),
            }],
            ..import_report_fixture()
        };
        let triple_status = import_card_status(None, Some(&triple));
        assert_eq!(triple_status.headline, "未処理 1");
        assert_eq!(triple_status.detail, "取込 7/10 · 失敗 2");
        assert_eq!(triple_status.tone, StatusTone::Waiting);
        assert_eq!(
            import_attention(None, Some(&triple)),
            Some(StatusTone::Failed)
        );
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
        assert_eq!(compression_attention(6, 5, 2), Some(StatusTone::Failed));
        assert_eq!(compression_attention(4, 1, 0), Some(StatusTone::Running));
        assert_eq!(compression_attention(2, 2, 0), Some(StatusTone::Waiting));
        assert_eq!(compression_attention(0, 0, 0), None);

        let progress = progress_fixture(1, 10);
        assert_eq!(
            import_attention(Some(&progress), None),
            Some(StatusTone::Running)
        );

        let failed = ImportReportView {
            total: 2,
            processed: 2,
            imported: 1,
            issues: vec![non_retryable_issue("/a.png")],
            remaining: vec![PathBuf::from("/b.png")],
            ..import_report_fixture()
        };
        assert_eq!(
            import_attention(None, Some(&failed)),
            Some(StatusTone::Failed)
        );

        let pending = ImportReportView {
            remaining: vec![PathBuf::from("/b.png")],
            ..import_report_fixture()
        };
        assert_eq!(
            import_attention(None, Some(&pending)),
            Some(StatusTone::Waiting)
        );

        assert_eq!(import_attention(None, None), None);

        let scanning = ScanProgressView {
            phase: IpcScanPhase::Indexing,
            processed: 1,
            total: Some(10),
            indexed: 1,
            root: 1,
            roots: 1,
        };
        assert_eq!(
            scan_attention(Some(&scanning), true, false),
            Some(StatusTone::Running)
        );
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
}
