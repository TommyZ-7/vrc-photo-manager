pub(crate) mod cli;
pub(crate) mod compression;
pub(crate) mod daemon;
pub(crate) mod deletion;
pub(crate) mod demo;
pub(crate) mod import;
pub(crate) mod lifecycle;
pub(crate) mod live;
pub(crate) mod photos;
pub(crate) mod search;
pub(crate) mod viewer;

pub(crate) use cli::LaunchOptions;

use std::cell::{Cell, RefCell};
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
    CompressionJob, CompressionProfile, DaemonError, DeletionCounts, DeletionRequest, Derivative,
    LibraryStats, Photo, PhotoPlayer, PhotoTextFilter,
};
use vrc_photo_core::settings::AppSettings;
use vrc_photo_ipc::Endpoint;

use crate::app::cli::{log_timing, process_start, timing_enabled};
use crate::app::compression::{ManualCompressionDialog, profile_name_update};
use crate::app::daemon::spawn_daemon_process;
use crate::app::demo::{DemoAssets, open_database};
use crate::app::import::{ImportDialogState, ImportReportView};
use crate::app::lifecycle::ScanProgressView;
use crate::state::photo_browser::{CountDebounce, PhotoBrowser, PhotoIndex};
use crate::state::preview::{PreviewKey, PreviewQueue};
use crate::state::selection::PhotoSelection;
use crate::state::thumbnail::ThumbnailCache;
use crate::state::viewer::{ViewerBoxState, ViewerMorph, ViewerOpenSource, ViewerPanelState};
use crate::ui::actions::{
    CloseNotifications, CloseQuickLook, FocusSearch, OpenQuickLook, QuickLookNextFocus,
    QuickLookPrevFocus, ToggleInspector, ToggleSidebar, ViewerNextPhoto, ViewerPreviousPhoto,
};
use crate::ui::chrome::{ChromeLayout, chrome_layout};
use crate::ui::gallery::GalleryView;
use crate::ui::notifications::{ToastSpec, toast_flush_needed};
use crate::ui::palette::resolve;
use crate::ui::theme::{Palette, WORKSPACE_GAP, WORKSPACE_PAD};
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
    pub(crate) library_bounds: CardBoundsMap,
    pub(crate) search_bounds: CardBoundsMap,
    /// Frame counters for the gallery-owned card bounds. Scroll repaints the
    /// grid without re-rendering App, so App's render epoch cannot tag them.
    pub(crate) library_bounds_epoch: Rc<Cell<u64>>,
    pub(crate) search_bounds_epoch: Rc<Cell<u64>>,
    pub(crate) inspector_preview_bounds: InspectorBounds,
    pub(crate) viewer_morph: Option<ViewerMorph>,
    pub(crate) viewer_morph_view: Entity<crate::ui::viewer::ViewerMorphView>,
    pub(crate) viewer_morph_epoch: u64,
    pub(crate) viewer_return_source: Option<ViewerOpenSource>,
    pub(crate) viewer_nav_epoch: u64,
    pub(crate) viewer_nav_direction: f32,
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
    pub(crate) library: Rc<RefCell<PhotoBrowser>>,
    pub(crate) search_view: Rc<RefCell<PhotoBrowser>>,
    pub(crate) library_gallery: Entity<GalleryView>,
    pub(crate) search_gallery: Entity<GalleryView>,
    /// The library index changed while another screen was mounted; the next
    /// visit to Photos rebuilds it.
    pub(crate) library_dirty: bool,
    /// A failed index/page query kept the previous snapshot mounted; retry from
    /// the Photos header.
    pub(crate) library_error: Option<String>,
    pub(crate) count_debounce: CountDebounce,
    /// Quiet-period tracker for daemon revision bursts. A full index rebuild
    /// waits until the revision has stopped moving for `LIBRARY_QUIET_PERIOD`.
    pub(crate) revision_debounce: CountDebounce,
    pub(crate) selected: Option<i64>,
    /// Multi-selection in the current photo tab. `selected` is the primary
    /// item used by the inspector and viewer. An empty selection means nothing
    /// is selected; otherwise the primary is always included.
    pub(crate) selected_ids: PhotoSelection,
    pub(crate) selection_anchor: Option<i64>,
    pub(crate) selected_photo: Option<Photo>,
    pub(crate) selected_players: Vec<PhotoPlayer>,
    pub(crate) selected_tags: Vec<String>,
    pub(crate) selected_derivatives: Vec<Derivative>,
    pub(crate) compressed_ids: HashSet<i64>,
    pub(crate) duplicate_ids: HashSet<i64>,
    pub(crate) thumbnails: Rc<RefCell<ThumbnailCache>>,
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
    pub(crate) profile_name_input: Entity<InputState>,
    pub(crate) _profile_name_subscription: Subscription,
    /// Profile whose name the input currently mirrors. Set when the editor
    /// changes selection so trim/normalization only rewrites the field then.
    pub(crate) profile_name_editing_id: Option<String>,
    pub(crate) profile_name_error: Option<String>,
    /// Requests the editor to restore the persisted name on the next render,
    /// after Enter or when focus leaves the field.
    pub(crate) profile_name_reset_pending: bool,
    pub(crate) profile_suffix_input: Entity<InputState>,
    pub(crate) _profile_suffix_subscription: Subscription,
    pub(crate) profile_output_input: Entity<InputState>,
    pub(crate) _profile_output_subscription: Subscription,
    pub(crate) manual_compress: Option<ManualCompressionDialog>,
    pub(crate) stats: LibraryStats,
    /// Library revision the cached stats were computed for.
    pub(crate) stats_revision: i64,
    pub(crate) settings: AppSettings,
    pub(crate) palette: Palette,
    pub(crate) update: update_view::UpdateState,
    pub(crate) jobs: Vec<CompressionJob>,
    pub(crate) waiting_jobs: Vec<CompressionJob>,
    pub(crate) active_job_count: i64,
    pub(crate) waiting_job_count: i64,
    pub(crate) daemon_errors: Vec<DaemonError>,
    pub(crate) failed_job_count: i64,
    pub(crate) failed_jobs: Vec<CompressionJob>,
    pub(crate) failed_job_names: HashMap<i64, String>,
    pub(crate) deletion_requests: Vec<DeletionRequest>,
    pub(crate) deletion_counts: DeletionCounts,
    pub(crate) deletion_baseline: Option<(i64, i64)>,
    pub(crate) import_report: Option<ImportReportView>,
    /// The import root whose targeted scan is still waiting for its
    /// `ScanFinished` event.
    pub(crate) import_scan_target: Option<String>,
    /// Automatic compression jobs reported by the most recent import scan.
    pub(crate) import_auto_compress_enqueued: usize,
    pub(crate) import_dialog: Option<ImportDialogState>,
    /// Last time the import progress rebuilt the window. The importer reports
    /// once per file, so notifications are coalesced to ~20 fps.
    pub(crate) last_import_progress_notify: Option<Instant>,
    /// A compression refresh timer is already scheduled.
    pub(crate) compression_refresh_scheduled: bool,
    /// The pending compression refresh must also invalidate library pages.
    pub(crate) compression_library_refresh_pending: bool,
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
    pub(crate) search_refresh_pending: bool,
    pub(crate) search_favorite: bool,
    pub(crate) search_days: Option<i64>,
    pub(crate) daemon_shutdown_armed: bool,
    pub(crate) pending_toasts: Vec<ToastSpec>,
    pub(crate) toast_flush_scheduled: bool,
    pub(crate) notification_modal_open: bool,
    pub(crate) notification_modal_epoch: u64,
    pub(crate) import_report_expanded: bool,
    pub(crate) notification_return_focus: Option<FocusHandle>,
    pub(crate) notification_close_focus: FocusHandle,
    pub(crate) notification_modal_focus: FocusHandle,
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

/// The quick-look overlay paints an opaque full-window surface, so the shell
/// underneath is invisible. Skipping it keeps viewer navigation from rebuilding
/// the sidebar, gallery header, and inspector on every notify.
pub(crate) fn should_render_shell(quick_look_open: bool) -> bool {
    !quick_look_open
}

/// Drops card-bounds entries older than the frame before `epoch`. Cards insert
/// their bounds while painting the current frame, so the previous frame must
/// survive until the replacement is painted.
pub(crate) fn retain_recent_bounds(map: &mut HashMap<i64, BoundsFrame>, epoch: u64) {
    let oldest = epoch.wrapping_sub(1);
    map.retain(|_, (frame, _)| *frame >= oldest);
}

/// A repeated library failure must not stack identical toasts on every page
/// load; only a new error message warrants a new notification.
pub(crate) fn should_notify_library_error(current: Option<&str>, next: &str) -> bool {
    current != Some(next)
}

/// Frame cap handed to `Animation::with_max_fps` for the display the window
/// is on.
///
/// A cap at or above the refresh rate can only add wake-up latency on top of
/// the compositor's vsync, so it is delegated to the animation-frame path by
/// returning `0`. A 1% margin keeps 60 in sync with 59.94Hz panels. `None`
/// (the platform does not report an interval) keeps the configured cap.
pub(crate) fn effective_motion_fps(configured: u32, refresh_interval: Option<Duration>) -> f32 {
    if configured == 0 {
        return 0.0;
    }
    let Some(interval) = refresh_interval else {
        return configured as f32;
    };
    let seconds = interval.as_secs_f32();
    if seconds > 0.0 && configured as f32 >= (1.0 / seconds) * 0.99 {
        0.0
    } else {
        configured as f32
    }
}

impl App {
    /// Animation frame cap from settings, resolved against the window's
    /// display. `0` disables throttling.
    pub(crate) fn motion_fps(&self, window: &Window, cx: &Context<Self>) -> f32 {
        effective_motion_fps(self.settings.ui_max_fps, window.refresh_interval(cx))
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
        let initial_palette = resolve(
            &initial_settings.ui_theme_main,
            &initial_settings.ui_theme_accent,
        );
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
        let profile_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("例: SNS共有用"));
        let profile_name_subscription = cx.subscribe(
            &profile_name_input,
            |this, input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let raw = input.read(cx).value().to_string();
                    let Some(id) = this.profile_name_editing_id.clone() else {
                        return;
                    };
                    let Some(current) = this
                        .profiles
                        .iter()
                        .find(|profile| profile.id == id)
                        .map(|profile| profile.name.clone())
                    else {
                        return;
                    };
                    match profile_name_update(&raw, &current) {
                        Ok(Some(name)) => {
                            if this.profile_name_error.take().is_some() {
                                cx.notify();
                            }
                            this.edit_profile_by_id(&id, |profile| profile.name = name, cx);
                        }
                        Ok(None) => {
                            if this.profile_name_error.take().is_some() {
                                cx.notify();
                            }
                        }
                        Err(error) => {
                            let message = error.message();
                            if this.profile_name_error.as_deref() != Some(message.as_str()) {
                                this.profile_name_error = Some(message);
                                cx.notify();
                            }
                        }
                    }
                }
                // Valid edits are already saved on Change. Enter and blur
                // restore the stored value so trims and rejected names do not
                // linger in the field.
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.profile_name_reset_pending = true;
                    cx.notify();
                }
                InputEvent::Focus => {}
            },
        );
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
        let notification_modal_focus = cx.focus_handle();
        let empty_index = Arc::new(PhotoIndex::new(Vec::new()));
        let library_bounds = Rc::new(RefCell::new(HashMap::new()));
        let search_bounds = Rc::new(RefCell::new(HashMap::new()));
        let library_bounds_epoch = Rc::new(Cell::new(0));
        let search_bounds_epoch = Rc::new(Cell::new(0));
        let library = Rc::new(RefCell::new(PhotoBrowser::new(empty_index.clone())));
        let search_view = Rc::new(RefCell::new(PhotoBrowser::new(empty_index)));
        let thumbnails = Rc::new(RefCell::new(ThumbnailCache::default()));
        let app_entity = cx.entity();
        let library_gallery = cx.new(|cx| {
            GalleryView::new(
                app_entity.clone(),
                false,
                library.clone(),
                thumbnails.clone(),
                library_bounds.clone(),
                library_bounds_epoch.clone(),
                cx,
            )
        });
        let search_gallery = cx.new(|cx| {
            GalleryView::new(
                app_entity,
                true,
                search_view.clone(),
                thumbnails.clone(),
                search_bounds.clone(),
                search_bounds_epoch.clone(),
                cx,
            )
        });
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
            library_bounds,
            search_bounds,
            library_bounds_epoch,
            search_bounds_epoch,
            inspector_preview_bounds: Rc::new(RefCell::new(None)),
            viewer_morph: None,
            viewer_morph_view: cx.new(|_| crate::ui::viewer::ViewerMorphView::new(initial_palette)),
            viewer_morph_epoch: 0,
            viewer_return_source: None,
            viewer_nav_epoch: 0,
            viewer_nav_direction: 1.0,
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
            library,
            search_view,
            library_gallery,
            search_gallery,
            library_dirty: false,
            library_error: None,
            count_debounce: CountDebounce::default(),
            revision_debounce: CountDebounce::default(),
            selected: None,
            selected_ids: PhotoSelection::default(),
            selection_anchor: None,
            selected_photo: None,
            selected_players: Vec::new(),
            selected_tags: Vec::new(),
            selected_derivatives: Vec::new(),
            compressed_ids: HashSet::new(),
            duplicate_ids: HashSet::new(),
            thumbnails,
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
            profile_name_input,
            _profile_name_subscription: profile_name_subscription,
            profile_name_editing_id: None,
            profile_name_error: None,
            profile_name_reset_pending: false,
            profile_suffix_input,
            _profile_suffix_subscription: profile_suffix_subscription,
            profile_output_input,
            _profile_output_subscription: profile_output_subscription,
            manual_compress: None,
            stats: LibraryStats::default(),
            stats_revision: -1,
            settings: AppSettings::default(),
            palette: initial_palette,
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
            import_scan_target: None,
            import_auto_compress_enqueued: 0,
            import_dialog: None,
            last_import_progress_notify: None,
            compression_refresh_scheduled: false,
            compression_library_refresh_pending: false,
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
            search_refresh_pending: false,
            search_favorite: false,
            search_days: Some(7),
            daemon_shutdown_armed: false,
            pending_toasts: Vec::new(),
            toast_flush_scheduled: false,
            notification_modal_open: false,
            notification_modal_epoch: 0,
            import_report_expanded: false,
            notification_return_focus: None,
            notification_close_focus,
            notification_modal_focus,
            deletion_requests: Vec::new(),
            deletion_counts: DeletionCounts::default(),
            deletion_baseline: None,
            failure_baseline: None,
            demo,
            demo_assets,
        };
        app.ensure_profiles();
        app.reload_all(true, cx);
        app.apply_palette(app.palette, cx);
        if let Some(message) = message {
            if database_ok {
                app.toast_warning(message, cx);
            } else {
                app.toast_error(message, cx);
            }
        }
        if app.library_error.is_none() && app.library.borrow().index.total == 0 {
            app.toast_info(
                "写真がありません。VRChatの写真フォルダを設定するか、--demo で起動してください。",
                cx,
            );
        }
        if let Some(notice) = app.update.notice() {
            app.toast_info(notice.message.clone(), cx);
        }
        let first_photo_id = app
            .library
            .borrow()
            .index
            .entries
            .first()
            .map(|entry| entry.id);
        if let Some(id) = first_photo_id {
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
        if toast_flush_needed(self.pending_toasts.len(), self.toast_flush_scheduled) {
            self.toast_flush_scheduled = true;
            cx.defer_in(window, |this, window, cx| {
                this.toast_flush_scheduled = false;
                this.flush_pending_toasts(window, cx);
            });
        }
        self.guard_viewer_selection(window, cx);
        let mut root = h_flex()
            .relative()
            .size_full()
            .bg(rgba(self.palette.glass))
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
            }));
        if should_render_shell(self.quick_look_open) {
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
                .child(self.chrome(window, layout, cx))
                .child(v_flex().flex_1().min_h_0().child(self.content(window, cx)));
            // The padding and gaps belong to the shell, not the window root:
            // overlays must use the full window coordinate space (the viewer's
            // morph bounds and centering are captured/computed there).
            let shell = h_flex()
                .relative()
                .size_full()
                .p(px(WORKSPACE_PAD))
                .gap(px(WORKSPACE_GAP))
                .child(self.sidebar(window, layout.content_height, cx))
                .child(main)
                .child(self.nav_tooltip(window, cx))
                .child(self.rail_toggle(window, cx));
            root = root.child(shell);
            if self.manual_compress.is_some() {
                root = root.child(self.manual_compression_overlay(window, cx));
            }
            if self.import_dialog.is_some() {
                root = root.child(self.import_modal_overlay(window, cx));
            }
        } else {
            root = root.child(self.quick_look(window, cx));
        }
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
    use super::{
        any_overlay_open, effective_motion_fps, retain_recent_bounds, should_notify_library_error,
        should_render_shell,
    };
    use crate::ui::notifications::{FailureNotice, failure_transition};
    use gpui_kit::{Bounds, point, px, size};
    use std::time::Duration;

    fn refresh_rate(hz: f32) -> Option<Duration> {
        Some(Duration::from_secs_f32(1.0 / hz))
    }

    #[test]
    fn zero_keeps_animations_uncapped_on_every_display() {
        for refresh in [
            refresh_rate(59.94),
            refresh_rate(60.0),
            refresh_rate(120.0),
            None,
        ] {
            assert_eq!(effective_motion_fps(0, refresh), 0.0);
        }
    }

    #[test]
    fn caps_below_the_refresh_rate_keep_the_timer_cap() {
        assert_eq!(effective_motion_fps(30, refresh_rate(59.94)), 30.0);
        assert_eq!(effective_motion_fps(30, refresh_rate(60.0)), 30.0);
        assert_eq!(effective_motion_fps(30, refresh_rate(120.0)), 30.0);
        assert_eq!(effective_motion_fps(60, refresh_rate(120.0)), 60.0);
        assert_eq!(effective_motion_fps(120, refresh_rate(240.0)), 120.0);
    }

    #[test]
    fn caps_at_or_above_the_refresh_rate_delegate_to_vsync() {
        assert_eq!(effective_motion_fps(60, refresh_rate(60.0)), 0.0);
        assert_eq!(effective_motion_fps(120, refresh_rate(60.0)), 0.0);
        assert_eq!(effective_motion_fps(240, refresh_rate(60.0)), 0.0);
        assert_eq!(effective_motion_fps(120, refresh_rate(120.0)), 0.0);
        assert_eq!(effective_motion_fps(144, refresh_rate(120.0)), 0.0);
    }

    #[test]
    fn sixty_matches_a_59_94_hz_panel() {
        assert_eq!(effective_motion_fps(60, refresh_rate(59.94)), 0.0);
    }

    #[test]
    fn an_unknown_refresh_rate_keeps_the_configured_cap() {
        assert_eq!(effective_motion_fps(30, None), 30.0);
        assert_eq!(effective_motion_fps(60, None), 60.0);
        assert_eq!(effective_motion_fps(120, None), 120.0);
    }

    #[test]
    fn stale_card_bounds_are_pruned_but_the_previous_frame_survives() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)));
        let mut map = std::collections::HashMap::new();
        map.insert(1, (7, bounds));
        map.insert(2, (6, bounds));
        retain_recent_bounds(&mut map, 8);
        assert!(map.contains_key(&1));
        assert!(!map.contains_key(&2));
    }

    #[test]
    fn the_hidden_shell_is_not_rebuilt_while_quick_look_is_open() {
        assert!(!should_render_shell(true));
        assert!(should_render_shell(false));
    }

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
