pub(crate) mod cli;
pub(crate) mod compression;
pub(crate) mod daemon;
pub(crate) mod demo;
pub(crate) mod import;
pub(crate) mod lifecycle;
pub(crate) mod photos;
pub(crate) mod search;
pub(crate) mod viewer;

pub(crate) use cli::LaunchOptions;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::*;
use gpui_kit::*;
use vrc_photo_core::db::Db;
use vrc_photo_core::model::{
    CompressionJob, CompressionProfile, DaemonError, Derivative, LibraryStats, Photo, PhotoPlayer,
    PhotoTextFilter,
};
use vrc_photo_core::settings::AppSettings;
use vrc_photo_ipc::Endpoint;

use crate::app::cli::{log_timing, process_start, timing_enabled};
use crate::app::compression::ManualCompressionDialog;
use crate::app::daemon::spawn_daemon_process;
use crate::app::demo::{DemoAssets, open_database};
use crate::app::import::{ImportDialogState, ImportReportView};
use crate::app::lifecycle::ScanProgressView;
use crate::state::photo_browser::{CountDebounce, PhotoBrowser, PhotoIndex};
use crate::state::preview::{PreviewKey, PreviewQueue};
use crate::state::thumbnail::ThumbnailCache;
use crate::state::viewer::{ViewerBoxState, ViewerMorph, ViewerOpenSource, ViewerPanelState};
use crate::ui::actions::{
    CloseNotifications, CloseQuickLook, FocusSearch, OpenQuickLook, QuickLookNextFocus,
    QuickLookPrevFocus, ToggleInspector, ToggleSidebar, ViewerNextPhoto, ViewerPreviousPhoto,
};
use crate::ui::chrome::{ChromeLayout, chrome_layout};
use crate::ui::notifications::ToastSpec;
use crate::ui::theme::{GLASS_WINDOW, WORKSPACE_GAP, WORKSPACE_PAD};
use crate::update_view;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidebarStatusTab {
    Compression,
    Import,
    Scan,
}

pub(crate) type BoundsFrame = (u64, Bounds<Pixels>);
pub(crate) type CardBoundsMap = Rc<RefCell<HashMap<i64, BoundsFrame>>>;
pub(crate) type InspectorBounds = Rc<RefCell<Option<BoundsFrame>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Screen {
    Photos,
    Compression,
    Search,
    Dashboard,
    Settings,
}

impl Screen {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Photos => "写真",
            Self::Compression => "圧縮設定",
            Self::Search => "検索",
            Self::Dashboard => "ダッシュボード",
            Self::Settings => "設定",
        }
    }
}

pub struct App {
    pub(crate) db: Db,
    pub(crate) screen: Screen,
    pub(crate) sidebar_collapsed: bool,
    pub(crate) sidebar_status_tab: SidebarStatusTab,
    pub(crate) sidebar_motion_epoch: u64,
    pub(crate) nav_previous: Screen,
    pub(crate) nav_epoch: u64,
    pub(crate) screen_epoch: u64,
    pub(crate) screen_direction: f32,
    /// Scroll position of the settings section column. The category card uses
    /// it both to jump to a section and to highlight the current one.
    pub(crate) settings_scroll: ScrollHandle,
    /// Last section the user jumped to. Sections near the bottom cannot always
    /// reach the top of the column, so the highlight falls back to this when
    /// the scroll position alone would point at an earlier section.
    pub(crate) ribbon_collapsed: bool,
    pub(crate) ribbon_epoch: u64,
    pub(crate) render_epoch: u64,
    pub(crate) card_image_bounds: CardBoundsMap,
    pub(crate) inspector_preview_bounds: InspectorBounds,
    pub(crate) viewer_morph: Option<ViewerMorph>,
    pub(crate) viewer_morph_view: Entity<crate::ui::viewer::ViewerMorphView>,
    pub(crate) viewer_morph_epoch: u64,
    pub(crate) viewer_return_source: Option<ViewerOpenSource>,
    pub(crate) viewer_nav_epoch: u64,
    pub(crate) viewer_nav_direction: f32,
    pub(crate) viewer_nav_started_at: Instant,
    pub(crate) viewer_open_started_at: Instant,
    pub(crate) viewer_box: ViewerBoxState,
    pub(crate) viewer_panel: ViewerPanelState,
    pub(crate) inspector_collapsed: bool,
    pub(crate) inspector_epoch: u64,
    pub(crate) hovered_nav: Option<Screen>,
    pub(crate) daemon_age_secs: Option<i64>,
    pub(crate) daemon_supervisor: Option<String>,
    pub(crate) daemon_pid: Option<u32>,
    pub(crate) ipc_endpoint: Option<Endpoint>,
    pub(crate) ipc_connected: bool,
    pub(crate) ipc_error: Option<String>,
    pub(crate) daemon_paused: bool,
    pub(crate) scan_progress: Option<ScanProgressView>,
    /// Last daemon-side library revision observed by the UI. Path changes and
    /// replacement status changes do not alter the photo count, so the count
    /// alone cannot tell the grid to rebuild.
    pub(crate) last_library_revision: i64,
    pub(crate) library: PhotoBrowser,
    pub(crate) search_view: PhotoBrowser,
    /// The library index changed while another screen was mounted; the next
    /// visit to Photos rebuilds it.
    pub(crate) library_dirty: bool,
    /// A failed index/page query kept the previous snapshot mounted; retry from
    /// the Photos header.
    pub(crate) library_error: Option<String>,
    pub(crate) count_debounce: CountDebounce,
    pub(crate) selected: Option<i64>,
    /// Multi-selection in the current photo tab. `selected` is the primary
    /// item used by the inspector and viewer. An empty list means nothing is
    /// selected; otherwise the primary is always included.
    pub(crate) selected_ids: Vec<i64>,
    pub(crate) selection_anchor: Option<i64>,
    pub(crate) selected_photo: Option<Photo>,
    pub(crate) selected_players: Vec<PhotoPlayer>,
    pub(crate) selected_tags: Vec<String>,
    pub(crate) selected_derivatives: Vec<Derivative>,
    pub(crate) compressed_ids: HashSet<i64>,
    pub(crate) duplicate_ids: HashSet<i64>,
    pub(crate) thumbnails: ThumbnailCache,
    pub(crate) thumbnail_notify_pending: bool,
    pub(crate) photo_grid_focus: FocusHandle,
    pub(crate) import_dialog_focus: FocusHandle,
    pub(crate) quick_look_open: bool,
    pub(crate) quick_look_epoch: u64,
    pub(crate) quick_look_return_focus: Option<FocusHandle>,
    pub(crate) quick_look_focus_index: usize,
    pub(crate) quick_look_details_open: bool,
    pub(crate) quick_look_details_epoch: u64,
    pub(crate) quick_look_photo_ids: Vec<i64>,
    pub(crate) quick_look_index: usize,
    pub(crate) quick_look_close_focus: FocusHandle,
    pub(crate) quick_look_prev_focus: FocusHandle,
    pub(crate) quick_look_next_focus: FocusHandle,
    pub(crate) quick_look_details_focus: FocusHandle,
    pub(crate) quick_look_favorite_focus: FocusHandle,
    pub(crate) quick_look_folder_focus: FocusHandle,
    pub(crate) quick_look_compress_focus: FocusHandle,
    pub(crate) quick_look_trash_focus: FocusHandle,
    pub(crate) quick_look_add_tag_focus: FocusHandle,
    pub(crate) quick_look_note_edit_focus: FocusHandle,
    pub(crate) quick_look_note_save_focus: FocusHandle,
    pub(crate) quick_look_note_cancel_focus: FocusHandle,
    pub(crate) preview: Option<Arc<RenderImage>>,
    pub(crate) preview_key: Option<PreviewKey>,
    pub(crate) preview_attempt: Option<PreviewKey>,
    pub(crate) preview_queue: PreviewQueue,
    pub(crate) previous_preview: Option<Arc<RenderImage>>,
    pub(crate) previous_color: Option<u32>,
    pub(crate) profiles: Vec<CompressionProfile>,
    pub(crate) profile_index: usize,
    pub(crate) profile_suffix_input: Entity<InputState>,
    pub(crate) _profile_suffix_subscription: Subscription,
    pub(crate) profile_output_input: Entity<InputState>,
    pub(crate) _profile_output_subscription: Subscription,
    pub(crate) manual_compress: Option<ManualCompressionDialog>,
    pub(crate) stats: LibraryStats,
    pub(crate) settings: AppSettings,
    pub(crate) update: update_view::UpdateState,
    pub(crate) jobs: Vec<CompressionJob>,
    pub(crate) waiting_jobs: Vec<CompressionJob>,
    pub(crate) active_job_count: i64,
    pub(crate) waiting_job_count: i64,
    pub(crate) daemon_errors: Vec<DaemonError>,
    pub(crate) failed_job_count: i64,
    pub(crate) failed_jobs: Vec<CompressionJob>,
    pub(crate) failed_job_names: HashMap<i64, String>,
    pub(crate) import_report: Option<ImportReportView>,
    pub(crate) import_dialog: Option<ImportDialogState>,
    pub(crate) monitor_world: Option<String>,
    pub(crate) monitor_instance: Option<String>,
    pub(crate) monitor_players: Vec<String>,
    pub(crate) daemon_active: bool,
    pub(crate) world_input: Entity<InputState>,
    pub(crate) player_input: Entity<InputState>,
    pub(crate) tag_input: Entity<InputState>,
    pub(crate) tag_edit_input: Entity<InputState>,
    pub(crate) note_edit_input: Entity<TextareaState>,
    pub(crate) note_edit_id: Option<i64>,
    pub(crate) photo_root_input: Entity<InputState>,
    pub(crate) log_dir_input: Entity<InputState>,
    pub(crate) import_dir_input: Entity<InputState>,
    pub(crate) search_active: bool,
    pub(crate) searching: bool,
    pub(crate) search_request: u64,
    pub(crate) applied_search: Option<PhotoTextFilter>,
    pub(crate) pending_search_anchor: Option<i64>,
    pub(crate) pending_layout_anchor_library: Option<i64>,
    pub(crate) pending_layout_anchor_search: Option<i64>,
    pub(crate) search_refresh_pending: bool,
    pub(crate) search_favorite: bool,
    pub(crate) search_days: Option<i64>,
    pub(crate) daemon_shutdown_armed: bool,
    pub(crate) window_handle: AnyWindowHandle,
    pub(crate) pending_toasts: Vec<ToastSpec>,
    pub(crate) toasts_ready: bool,
    pub(crate) notification_modal_open: bool,
    pub(crate) notification_modal_epoch: u64,
    pub(crate) import_report_expanded: bool,
    pub(crate) notification_return_focus: Option<FocusHandle>,
    pub(crate) notification_close_focus: FocusHandle,
    pub(crate) failure_baseline: Option<(i64, usize)>,
    pub(crate) demo: bool,
    pub(crate) demo_assets: Option<DemoAssets>,
}

/// True while any full-window overlay owns the input tree: the quick-look
/// viewer, the manual-compression dialog, or the import dialog. Global
/// shortcuts that reshape the shell must not fire through these layers.
pub(crate) fn any_overlay_open(
    quick_look_open: bool,
    manual_compression_open: bool,
    import_dialog_open: bool,
    notification_modal_open: bool,
) -> bool {
    quick_look_open || manual_compression_open || import_dialog_open || notification_modal_open
}

/// A repeated library failure must not stack identical toasts on every page
/// load; only a new error message warrants a new notification.
pub(crate) fn should_notify_library_error(current: Option<&str>, next: &str) -> bool {
    current != Some(next)
}

impl App {
    /// Animation frame cap from settings. `0` disables throttling.
    pub(crate) fn motion_fps(&self) -> f32 {
        self.settings.ui_max_fps as f32
    }

    pub(crate) fn overlay_open(&self) -> bool {
        any_overlay_open(
            self.quick_look_open,
            self.manual_compress.is_some(),
            self.import_dialog.is_some(),
            self.notification_modal_open,
        )
    }

    pub(crate) fn note_library_error(&mut self, error: String, cx: &mut Context<Self>) {
        let notify = should_notify_library_error(self.library_error.as_deref(), &error);
        self.library_error = Some(error);
        if notify {
            self.toast_actionable(
                NotificationType::Error,
                "ライブラリを読み込めませんでした。通知から再読込できます",
                cx,
            );
        }
    }

    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        options: &LaunchOptions,
    ) -> Self {
        let (db, demo, database_ok, message, demo_assets) = open_database(options);
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
        let profile_output_input = cx
            .new(|cx| InputState::new(window, cx).placeholder("compressed または /path/to/output"));
        let profile_output_subscription = cx.subscribe(
            &profile_output_input,
            |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().trim().to_string();
                    this.edit_profile(
                        |profile| {
                            profile.output_dir = if value.is_empty() {
                                None
                            } else {
                                Some(value.clone())
                            }
                        },
                        cx,
                    );
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
        let quick_look_folder_focus = cx.focus_handle();
        let quick_look_compress_focus = cx.focus_handle();
        let quick_look_trash_focus = cx.focus_handle();
        let quick_look_add_tag_focus = cx.focus_handle();
        let quick_look_note_edit_focus = cx.focus_handle();
        let quick_look_note_save_focus = cx.focus_handle();
        let quick_look_note_cancel_focus = cx.focus_handle();
        let notification_close_focus = cx.focus_handle();
        let empty_index = Arc::new(PhotoIndex::new(Vec::new()));
        let window_handle = window.window_handle();
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
            settings_scroll: ScrollHandle::new(),
            ribbon_collapsed: initial_settings.ui_ribbon_collapsed,
            ribbon_epoch: 0,
            render_epoch: 0,
            card_image_bounds: Rc::new(RefCell::new(HashMap::new())),
            inspector_preview_bounds: Rc::new(RefCell::new(None)),
            viewer_morph: None,
            viewer_morph_view: cx.new(|_| crate::ui::viewer::ViewerMorphView::new()),
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
            selected_photo: None,
            selected_players: Vec::new(),
            selected_tags: Vec::new(),
            selected_derivatives: Vec::new(),
            compressed_ids: HashSet::new(),
            duplicate_ids: HashSet::new(),
            thumbnails: ThumbnailCache::default(),
            thumbnail_notify_pending: false,
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
            quick_look_folder_focus,
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
            profile_output_input,
            _profile_output_subscription: profile_output_subscription,
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
            daemon_shutdown_armed: false,
            window_handle,
            pending_toasts: Vec::new(),
            toasts_ready: false,
            notification_modal_open: false,
            notification_modal_epoch: 0,
            import_report_expanded: false,
            notification_return_focus: None,
            notification_close_focus,
            failure_baseline: None,
            demo,
            demo_assets,
        };
        app.ensure_profiles();
        app.reload_all(true, cx);
        if let Some(message) = message {
            if database_ok {
                app.toast_warning(message, cx);
            } else {
                app.toast_error(message, cx);
            }
        }
        if app.library_error.is_none() && app.library.index.total == 0 {
            app.toast_info(
                "写真がありません。VRChatの写真フォルダを設定するか、--demo で起動してください。",
                cx,
            );
        }
        if let Some(notice) = app.update.notice() {
            app.toast_info(notice.message.clone(), cx);
        }
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
            app.toast_error(format!("デーモンを起動できません: {error}"), cx);
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
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_epoch = self.render_epoch.wrapping_add(1);
        if !self.toasts_ready {
            self.toasts_ready = true;
            cx.defer_in(window, |this, window, cx| {
                this.flush_pending_toasts(window, cx);
            });
        }
        self.guard_viewer_selection(window, cx);
        let viewport = window.viewport_size();
        let client_inset = window.client_inset().map(f32::from).unwrap_or(0.0);
        let mut layout: ChromeLayout = chrome_layout(
            f32::from(viewport.width),
            f32::from(viewport.height),
            client_inset,
            self.sidebar_collapsed,
        );
        if self.ribbon_collapsed {
            // The collapsed body still has its target height for the expand
            // animation, but the content area can use that space.
            layout.content_height += layout.ribbon_height;
        }
        let main = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap(px(WORKSPACE_GAP))
            .child(self.chrome(layout, cx))
            .child(v_flex().flex_1().min_h_0().child(self.content(window, cx)));
        // The padding and gaps belong to the shell, not the window root:
        // overlays must use the full window coordinate space (the viewer's
        // morph bounds and centering are captured/computed there).
        let shell = h_flex()
            .relative()
            .size_full()
            .p(px(WORKSPACE_PAD))
            .gap(px(WORKSPACE_GAP))
            .child(self.sidebar(layout.content_height, cx))
            .child(main)
            .child(self.nav_tooltip())
            .child(self.rail_toggle(cx));
        let root = h_flex()
            .relative()
            .size_full()
            .bg(rgba(GLASS_WINDOW))
            .font_family("Noto Sans JP")
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| this.toggle_inspector(cx)))
            .on_action(cx.listener(|this, _: &CloseNotifications, window, cx| {
                this.close_notification_modal(window, cx)
            }))
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
            .child(shell);
        let root = if self.quick_look_open {
            root.child(self.quick_look(window, cx))
        } else {
            root
        };
        let root = if self.manual_compress.is_some() {
            root.child(self.manual_compression_overlay(window, cx))
        } else {
            root
        };
        let root = if self.import_dialog.is_some() {
            root.child(self.import_modal_overlay(cx))
        } else {
            root
        };
        let root = root.child(self.viewer_morph_view.clone());
        if self.notification_modal_open {
            root.child(self.notification_modal(window, cx))
        } else {
            root
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{any_overlay_open, should_notify_library_error};
    use crate::ui::notifications::{FailureNotice, failure_transition};

    #[test]
    fn overlay_guard_covers_the_viewer_and_both_modals() {
        assert!(!any_overlay_open(false, false, false, false));
        assert!(any_overlay_open(true, false, false, false));
        assert!(any_overlay_open(false, true, false, false));
        assert!(any_overlay_open(false, false, true, false));
        assert!(any_overlay_open(false, false, false, true));
    }

    #[test]
    fn should_notify_library_error_only_reports_changes() {
        assert!(should_notify_library_error(None, "boom"));
        assert!(!should_notify_library_error(Some("boom"), "boom"));
        assert!(should_notify_library_error(Some("boom"), "different"));
    }

    #[test]
    fn failure_transition_only_reports_new_failures_after_the_baseline() {
        assert!(failure_transition(None, (3, 2)).is_empty());
        assert_eq!(
            failure_transition(Some((3, 2)), (5, 2)),
            vec![FailureNotice::Compression { count: 5 }]
        );
        assert_eq!(
            failure_transition(Some((3, 2)), (3, 4)),
            vec![FailureNotice::Daemon { count: 4 }]
        );
        assert!(failure_transition(Some((5, 4)), (5, 4)).is_empty());
    }
}
