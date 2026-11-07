pub(crate) mod ai;
pub(crate) mod ai_tag_picker;
pub(crate) mod ai_thresholds;
pub(crate) mod cli;
pub(crate) mod compression;
pub(crate) mod context_menu;
pub(crate) mod daemon;
pub(crate) mod deletion;
pub(crate) mod demo;
pub(crate) mod demo_fixture;
pub(crate) mod discord;
pub(crate) mod file_picker;
pub(crate) mod import;
pub(crate) mod lifecycle;
pub(crate) mod live;
pub(crate) mod photos;
pub(crate) mod search;
pub(crate) mod tag_edit;
pub(crate) mod viewer;

pub(crate) use cli::LaunchOptions;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::base::VirtualListScrollHandle;
use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::*;
use gpui_kit::*;
use vrc_photon_core::ai::{PhotoAi, PhotoAiTagView};
use vrc_photon_core::db::Db;
use vrc_photon_core::model::{
    CompressionJob, CompressionProfile, DaemonError, DeletionCounts, DeletionRequest, Derivative,
    LibraryStats, Photo, PhotoPlayer, PhotoTextFilter, SendHistoryEntry,
};
use vrc_photon_core::settings::{AppSettings, load_discord_webhook};
use vrc_photon_core::thumbnail::ThumbnailCacheUsage;
use vrc_photon_i18n::{tr, tr_args};
use vrc_photon_ipc::Endpoint;

use crate::app::cli::{log_timing, process_start, timing_enabled};
use crate::app::compression::{ManualCompressionDialog, profile_name_update};
use crate::app::daemon::start_daemon;
use crate::app::demo::{DemoAssets, open_database};
use crate::app::import::{ImportDialogState, ImportReportView};
use crate::app::lifecycle::ScanProgressView;
use crate::app::live::LiveRefreshGate;
use crate::state::motion::ToggleMotion;
use crate::state::photo_browser::{CountDebounce, PhotoBrowser, PhotoIndex};
use crate::state::preview::{PreviewKey, PreviewQueue};
use crate::state::selection::PhotoSelection;
use crate::state::thumbnail::{ThumbnailCache, ThumbnailNoticeBatch};
use crate::state::viewer::{
    ViewerBoxState, ViewerMorph, ViewerOpenSource, ViewerPanelState, ViewerZoom, ViewerZoomDrag,
    ZOOM_STEP,
};
use crate::ui::actions::{
    CloseNotifications, CloseQuickLook, FocusSearch, GridAbovePhoto, GridBelowPhoto, GridNextPhoto,
    GridPreviousPhoto, OpenPhotoContextMenu, OpenQuickLook, QuickLookNextFocus, QuickLookPrevFocus,
    ToggleInspector, ToggleSidebar, TrashSelectedPhotos, ViewerNextPhoto, ViewerPreviousPhoto,
    ViewerZoomIn, ViewerZoomOut, ViewerZoomReset,
};
use crate::ui::chrome::{ChromeLayout, chrome_layout};
use crate::ui::gallery::GalleryView;
use crate::ui::notifications::{ToastSpec, toast_flush_needed};
use crate::ui::palette::{accent_colors, main_colors, resolve};
use crate::ui::settings::ThemeAxis;
use crate::ui::theme::{
    MIN_CARD_WIDTH, Palette, WORKSPACE_GAP, WORKSPACE_PAD, content_insets, photo_row_height,
};
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
    Integrations,
    Settings,
}

impl Screen {
    pub(crate) fn title(self) -> String {
        self.title_in(&vrc_photon_i18n::current_locale())
    }

    pub(crate) fn title_in(self, locale: &str) -> String {
        let key = match self {
            Self::Photos => "nav.photos",
            Self::Compression => "nav.compression",
            Self::Search => "nav.search",
            Self::Dashboard => "nav.dashboard",
            Self::Integrations => "nav.integrations",
            Self::Settings => "nav.settings",
        };
        vrc_photon_i18n::tr_in(locale, key)
    }
}

pub struct App {
    pub(crate) db: Db,
    pub(crate) screen: Screen,
    pub(crate) sidebar_collapsed: bool,
    pub(crate) sidebar_status_tab: SidebarStatusTab,
    /// 0 = expanded, 1 = collapsed.
    pub(crate) sidebar_motion: ToggleMotion,
    pub(crate) nav_previous: Screen,
    pub(crate) nav_epoch: u64,
    /// Screen-enter progress: 0 = entering, 1 = settled.
    pub(crate) screen_motion: ToggleMotion,
    pub(crate) screen_direction: f32,
    /// Scroll position of the settings section column, shared with its
    /// scrollbar.
    pub(crate) settings_scroll: ScrollHandle,
    pub(crate) integrations_scroll: ScrollHandle,
    pub(crate) ribbon_collapsed: bool,
    /// 0 = expanded, 1 = collapsed.
    pub(crate) ribbon_motion: ToggleMotion,
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
    pub(crate) zoom: ViewerZoom,
    pub(crate) zoom_drag: Option<ViewerZoomDrag>,
    pub(crate) viewer_frame_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub(crate) viewer_minimap_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub(crate) inspector_collapsed: bool,
    /// 0 = expanded, 1 = collapsed.
    pub(crate) inspector_motion: ToggleMotion,
    pub(crate) hovered_nav: Option<Screen>,
    pub(crate) daemon_age_secs: Option<i64>,
    pub(crate) daemon_supervisor: Option<String>,
    pub(crate) daemon_pid: Option<u32>,
    /// Version of the last daemon status snapshot applied from the event
    /// stream. Snapshots stamped with an older version are stale (read
    /// before a state change but published after it) and must not revive
    /// state the daemon has already moved past.
    pub(crate) last_status_version: u64,
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
    pub(crate) library_error: Option<&'static str>,
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
    pub(crate) selected_ai: Option<PhotoAi>,
    pub(crate) selected_ai_tags: Vec<PhotoAiTagView>,
    /// Bumped whenever the selected AI rows are reloaded. Keeps the cached
    /// inspector block from surviving a tagging result or tag edit.
    pub(crate) ai_meta_revision: u64,
    /// Cached AI metadata block shared by the inspector and Quick Look.
    pub(crate) ai_meta_cache: ai::AiMetaCache,
    pub(crate) selected_derivatives: Vec<Derivative>,
    pub(crate) compressed_ids: HashSet<i64>,
    pub(crate) duplicate_ids: HashSet<i64>,
    pub(crate) thumbnails: Rc<RefCell<ThumbnailCache>>,
    pub(crate) thumbnail_notify_pending: bool,
    /// Thumbnails generated by the UI and not yet reported to the daemon.
    pub(crate) thumbnail_notice: ThumbnailNoticeBatch,
    /// A timer is already queued to flush a partial thumbnail notice batch.
    pub(crate) thumbnail_notice_scheduled: bool,
    pub(crate) photo_grid_focus: FocusHandle,
    pub(crate) search_grid_focus: FocusHandle,
    pub(crate) photo_context_menu: Option<context_menu::PhotoContextMenuState>,
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
    /// Full-resolution Quick Look image. Memory only: never cached on disk.
    pub(crate) full_preview: Option<Arc<RenderImage>>,
    pub(crate) full_preview_key: Option<PreviewKey>,
    pub(crate) full_preview_queue: PreviewQueue,
    pub(crate) previous_preview: Option<Arc<RenderImage>>,
    pub(crate) previous_color: Option<u32>,
    pub(crate) profiles: Vec<CompressionProfile>,
    pub(crate) profile_index: usize,
    pub(crate) profile_name_input: Entity<InputState>,
    pub(crate) _profile_name_subscription: Subscription,
    /// Profile whose name the input currently mirrors. Set when the editor
    /// changes selection so trim/normalization only rewrites the field then.
    pub(crate) profile_name_editing_id: Option<String>,
    pub(crate) profile_name_error: Option<crate::app::compression::ProfileNameError>,
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
    /// AI tagging plugin, model, queue, and transient operation state.
    pub(crate) ai: ai::AiUiState,
    /// A 250 ms event-burst refresh of the AI cards is already queued.
    pub(crate) ai_refresh_scheduled: bool,
    /// The queued AI refresh must also reload the selected inspector photo.
    pub(crate) ai_selected_refresh_pending: bool,
    /// The focused AI chip whose full tooltip is shown inline. Handles are
    /// per-panel, so the inspector and Quick Look cannot light up each other's
    /// chips.
    pub(crate) ai_tag_focused: Option<FocusHandle>,
    /// Focus traps for the AI license and uninstall dialogs.
    pub(crate) ai_license_focus: FocusHandle,
    pub(crate) ai_uninstall_focus: FocusHandle,
    /// Focus restored after the AI dialog closes.
    pub(crate) ai_modal_return_focus: Option<FocusHandle>,
    /// The next render should move focus into the open AI dialog.
    pub(crate) ai_modal_focus_pending: bool,
    /// Search AI-tag picker state and its query subscription.
    pub(crate) ai_tag_picker: ai_tag_picker::AiTagPickerState,
    pub(crate) _ai_tag_picker_query_subscription: Subscription,
    /// Tag-threshold editor state.
    pub(crate) ai_thresholds: ai_thresholds::AiThresholdEditorState,
    pub(crate) _ai_threshold_query_subscription: Subscription,
    /// 0 = body expanded, 1 = collapsed to the card header.
    pub(crate) integrations_auto_motion: ToggleMotion,
    /// 0 = runtime details expanded, 1 = collapsed while AI is disabled.
    pub(crate) integrations_ai_motion: ToggleMotion,
    pub(crate) integrations_clipboard_motion: ToggleMotion,
    pub(crate) integrations_discord_motion: ToggleMotion,
    /// Stored webhook URL. It is never rendered in full: the card masks it via
    /// `mask_webhook_url` and the editor starts empty.
    pub(crate) discord_url: Option<String>,
    pub(crate) discord_url_editing: bool,
    pub(crate) discord_url_input: Entity<InputState>,
    pub(crate) discord_destination_input: Entity<InputState>,
    pub(crate) _discord_destination_subscription: Subscription,
    /// Inline validation message for the webhook editor.
    pub(crate) discord_url_error: Option<SharedString>,
    pub(crate) send_history: Vec<SendHistoryEntry>,
    /// Sends held in automatic `waiting` mode.
    pub(crate) send_waiting_count: i64,
    /// Outbox row of a test send waiting for a terminal state to toast.
    pub(crate) pending_test_send: Option<i64>,
    pub(crate) thumbnail_quality_slider: Entity<SliderState>,
    pub(crate) _thumbnail_quality_subscription: Subscription,
    /// Last measured size of the thumbnail cache, refreshed when the settings
    /// screen opens and after a clear.
    pub(crate) thumbnail_cache_usage: Option<ThumbnailCacheUsage>,
    pub(crate) palette: Palette,
    pub(crate) update: update_view::UpdateState,
    pub(crate) jobs: Vec<CompressionJob>,
    pub(crate) waiting_jobs: Vec<CompressionJob>,
    /// Scroll offset of the virtualized compression queue, shared by the
    /// compression screen and the dashboard panel.
    pub(crate) queue_scroll: VirtualListScrollHandle,
    pub(crate) active_job_count: i64,
    pub(crate) waiting_job_count: i64,
    pub(crate) daemon_errors: Vec<DaemonError>,
    pub(crate) failed_job_count: i64,
    /// Failed jobs still below the retry cap, from a DB aggregate. Retry
    /// affordances read this rather than scanning the loaded page of
    /// failed jobs, which is capped at 100 entries.
    pub(crate) retryable_failed_count: i64,
    pub(crate) failed_jobs: Vec<CompressionJob>,
    pub(crate) failed_job_names: HashMap<i64, String>,
    pub(crate) deletion_requests: Vec<DeletionRequest>,
    pub(crate) deletion_counts: DeletionCounts,
    pub(crate) deletion_baseline: Option<(i64, i64)>,
    pub(crate) import_report: Option<Rc<ImportReportView>>,
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
    /// Coalescing gate for the background live refresh. The three-second
    /// poller and IPC-triggered refreshes share it.
    pub(crate) live_refresh: LiveRefreshGate,
    pub(crate) monitor_world: Option<String>,
    pub(crate) monitor_instance: Option<String>,
    pub(crate) monitor_players: Vec<String>,
    pub(crate) daemon_active: bool,
    pub(crate) world_input: Entity<InputState>,
    pub(crate) player_input: Entity<InputState>,
    pub(crate) tag_input: Entity<InputState>,
    pub(crate) tag_edit_input: Entity<InputState>,
    pub(crate) tag_edit: tag_edit::TagEditState,
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
    /// A tag/AI edit changed the applied search while the Search screen was
    /// hidden. Refresh once when the user returns to Search.
    pub(crate) search_refresh_dirty: bool,
    pub(crate) search_favorite: bool,
    pub(crate) search_days: Option<i64>,
    pub(crate) search_ai_tags: Vec<String>,
    pub(crate) search_people_input: Entity<InputState>,
    pub(crate) daemon_shutdown_armed: bool,
    pub(crate) pending_toasts: Vec<ToastSpec>,
    pub(crate) toast_flush_scheduled: bool,
    pub(crate) notification_modal_open: bool,
    pub(crate) notification_modal_epoch: u64,
    pub(crate) import_report_expanded: bool,
    pub(crate) notification_return_focus: Option<FocusHandle>,
    pub(crate) notification_close_focus: FocusHandle,
    pub(crate) notification_modal_focus: FocusHandle,
    /// Titlebar controls, in tab order: ribbon collapse, then the bell.
    pub(crate) ribbon_collapse_focus: FocusHandle,
    pub(crate) notification_bell_focus: FocusHandle,
    /// Ribbon command focus handles, in visual (tab) order.
    pub(crate) ribbon_import_copy_focus: FocusHandle,
    pub(crate) ribbon_import_move_focus: FocusHandle,
    pub(crate) ribbon_reload_focus: FocusHandle,
    pub(crate) card_size_focus: [FocusHandle; 4],
    pub(crate) ribbon_inspector_focus: FocusHandle,
    pub(crate) ribbon_select_all_focus: FocusHandle,
    pub(crate) ribbon_clear_selection_focus: FocusHandle,
    pub(crate) ribbon_trash_focus: FocusHandle,
    pub(crate) ribbon_manual_compress_focus: FocusHandle,
    pub(crate) ribbon_compress_folder_focus: FocusHandle,
    pub(crate) ribbon_compress_profile_focus: FocusHandle,
    pub(crate) ribbon_compress_clipboard_focus: FocusHandle,
    pub(crate) ribbon_release_waiting_focus: FocusHandle,
    /// Theme swatch focus handles, one per palette color, per axis.
    pub(crate) theme_main_focus: Vec<FocusHandle>,
    pub(crate) theme_accent_focus: Vec<FocusHandle>,
    /// Swatch that currently holds keyboard focus, if any. Drives the
    /// focused color name shown in the theme axis heading, since GPUI
    /// tooltips only appear on hover.
    pub(crate) theme_focused: Option<(ThemeAxis, usize)>,
    pub(crate) _theme_focus_subscriptions: Vec<Subscription>,
    pub(crate) _context_menu_activation: Subscription,
    pub(crate) _context_menu_bounds: Subscription,
    pub(crate) failure_baseline: Option<(i64, usize)>,
    pub(crate) demo: bool,
    pub(crate) demo_assets: Option<DemoAssets>,
}

/// True while any full-window overlay owns the input tree: the quick-look
/// viewer, the manual-compression dialog, the import dialog, an AI plugin
/// dialog, or the notification modal. Global shortcuts that reshape the shell
/// must not fire through these layers.
pub(crate) fn any_overlay_open(
    quick_look_open: bool,
    manual_compression_open: bool,
    import_dialog_open: bool,
    ai_modal_open: bool,
    notification_modal_open: bool,
) -> bool {
    quick_look_open
        || manual_compression_open
        || import_dialog_open
        || ai_modal_open
        || notification_modal_open
}

/// AI surfaces that must all block global shortcuts and each other.
pub(crate) fn ai_overlay_open(
    license_open: bool,
    uninstall_open: bool,
    picker_open: bool,
    thresholds_open: bool,
) -> bool {
    license_open || uninstall_open || picker_open || thresholds_open
}

/// Whether a global shortcut (sidebar / inspector / search / quick look)
/// must stay inert. Alert dialogs from `open_alert_dialog` live in the
/// component dialog layer, outside `any_overlay_open`, so action handlers
/// combine both signals through this predicate.
pub(crate) fn global_shortcut_blocked(overlay_open: bool, dialog_open: bool) -> bool {
    overlay_open || dialog_open
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
            ai_overlay_open(
                self.ai.license_modal_open,
                self.ai.uninstall_modal_open,
                self.ai_tag_picker.open,
                self.ai_thresholds.open,
            ),
            self.notification_modal_open,
        ) || self.tag_edit.open
    }

    /// Whether global shortcuts must stay inert right now: a full-window
    /// overlay is open, or an alert dialog owns the component dialog layer.
    pub(crate) fn global_shortcuts_blocked(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        global_shortcut_blocked(self.overlay_open(), window.has_active_dialog(cx))
    }

    pub(crate) fn note_library_error(&mut self, error: &'static str, cx: &mut Context<Self>) {
        let notify = should_notify_library_error(self.library_error, error);
        self.library_error = Some(error);
        if notify {
            self.toast_actionable(NotificationType::Error, tr!("app.library_error"), cx);
        }
    }

    pub(crate) fn set_language(&mut self, code: &str, window: &mut Window, cx: &mut Context<Self>) {
        let code = vrc_photon_i18n::normalize(code);
        if self.settings.language == code {
            return;
        }
        let previous = self.settings.clone();
        self.settings.language = code.clone();
        if let Err(error) = self.settings.save(&self.db) {
            self.settings = previous;
            self.toast_error(tr_args!("settings.language.save_failed", error = error), cx);
            cx.notify();
            return;
        }
        vrc_photon_i18n::set_locale(&code);
        self.update_input_placeholders(window, cx);
        self.library.borrow_mut().invalidate_labels();
        self.search_view.borrow_mut().invalidate_labels();
        // The connection detail was formatted when it arrived; the next
        // heartbeat/reconnect repopulates it in the new language.
        self.ipc_error = None;
        cx.notify();
    }

    fn update_input_placeholders(&self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = [
            (&self.world_input, "input.world"),
            (&self.player_input, "input.player"),
            (&self.tag_input, "input.tag"),
            (&self.tag_edit_input, "input.tag_add"),
            (&self.tag_edit.input, "input.tag_add"),
            (&self.photo_root_input, "input.photo_root"),
            (&self.log_dir_input, "input.log_dir"),
            (&self.import_dir_input, "input.import_dir"),
            (&self.profile_name_input, "input.profile_name"),
            (&self.profile_suffix_input, "input.profile_suffix"),
            (&self.profile_output_input, "input.profile_output"),
            (&self.search_people_input, "search.ai.people_placeholder"),
            (
                &self.ai_tag_picker.query,
                "ai.tags.picker.search_placeholder",
            ),
            (
                &self.ai_thresholds.query,
                "settings.ai.thresholds.search_placeholder",
            ),
            (
                &self.discord_url_input,
                "integrations.discord.webhook.placeholder",
            ),
            (
                &self.discord_destination_input,
                "integrations.discord.destination.placeholder",
            ),
        ];
        for (input, key) in inputs {
            input.update(cx, |state, cx| {
                state.set_placeholder(vrc_photon_i18n::tr(key), window, cx);
            });
        }
        self.note_edit_input.update(cx, |state, cx| {
            state.set_placeholder(vrc_photon_i18n::tr("input.note"), window, cx);
        });
    }

    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        options: &LaunchOptions,
    ) -> Self {
        let (db, demo, database_ok, message, demo_assets) = open_database(options);
        let db_path = db.path().and_then(|path| std::fs::canonicalize(path).ok());
        let health_token = std::env::var("VRC_PHOTON_HEALTH_TOKEN").ok();
        let updating = health_token.is_some();
        let mut initial_settings = AppSettings::load(&db).unwrap_or_else(|error| {
            // `AppSettings::load` backs up malformed JSON before returning, so
            // defaults only fill this first frame.
            tracing::warn!(%error, "cannot load app settings; using defaults for this frame");
            AppSettings::default()
        });
        initial_settings.language = vrc_photon_i18n::set_locale(&initial_settings.language);
        let initial_palette = resolve(
            &initial_settings.ui_theme_main,
            &initial_settings.ui_theme_accent,
        );
        let initial_library_revision = db.library_revision().unwrap_or(0);
        cx.set_reduce_motion(initial_settings.ui_reduce_motion);
        let world_input = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("input.world")));
        let player_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("input.player")));
        let tag_input = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("input.tag")));
        let search_people_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(tr!("search.ai.people_placeholder")));
        let ai_tag_picker_query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!("ai.tags.picker.search_placeholder"))
        });
        let ai_tag_picker_query_subscription =
            cx.subscribe(&ai_tag_picker_query, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
        let ai_tag_picker_focus = cx.focus_handle();
        let ai_tag_picker =
            ai_tag_picker::AiTagPickerState::new(ai_tag_picker_query, ai_tag_picker_focus);
        let ai_threshold_query = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("settings.ai.thresholds.search_placeholder"))
        });
        let ai_threshold_query_subscription =
            cx.subscribe(&ai_threshold_query, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
        let ai_threshold_focus = cx.focus_handle();
        let ai_thresholds =
            ai_thresholds::AiThresholdEditorState::new(ai_threshold_query, ai_threshold_focus);
        let tag_edit_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("input.tag_add")));
        let tag_edit_dialog_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("input.tag_add")));
        let tag_edit = tag_edit::TagEditState::new(tag_edit_dialog_input, cx.focus_handle());
        let note_edit_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(tr!("input.note"))
                .auto_grow(2, 6)
        });
        let photo_root_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("input.photo_root"))
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
                .placeholder(tr!("input.log_dir"))
                .default_value(initial_settings.log_dir.clone().unwrap_or_default())
        });
        let import_dir_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("input.import_dir"))
                .default_value(initial_settings.import_dir().display().to_string())
        });
        let profile_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("input.profile_name")));
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
                            if this.profile_name_error != Some(error) {
                                this.profile_name_error = Some(error);
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
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("input.profile_suffix")));
        let profile_suffix_subscription = cx.subscribe(
            &profile_suffix_input,
            |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    this.edit_profile(|profile| profile.suffix = value.clone(), cx);
                }
            },
        );
        let profile_output_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("input.profile_output")));
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
        let thumbnail_quality_slider = cx.new(|_| {
            SliderState::new()
                .min(f32::from(vrc_photon_core::thumbnail::THUMBNAIL_QUALITY_MIN))
                .max(f32::from(vrc_photon_core::thumbnail::THUMBNAIL_QUALITY_MAX))
                .step(1.0)
                .default_value(f32::from(initial_settings.thumbnail_quality))
        });
        let thumbnail_quality_subscription = cx.subscribe(
            &thumbnail_quality_slider,
            |this, _, event: &SliderEvent, cx| match event {
                SliderEvent::Change(value) => {
                    let quality = value.end().round().clamp(10.0, 100.0) as u8;
                    if this.settings.thumbnail_quality != quality {
                        this.settings.thumbnail_quality = quality;
                        cx.notify();
                    }
                }
                SliderEvent::Release(value) => {
                    let quality = value.end().round().clamp(10.0, 100.0) as u8;
                    if this.settings.thumbnail_quality != quality {
                        this.settings.thumbnail_quality = quality;
                    }
                    this.save_thumbnail_settings(cx);
                }
            },
        );
        let initial_discord_url = load_discord_webhook(&db).ok().flatten();
        let discord_url_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!("integrations.discord.webhook.placeholder"))
        });
        let discord_destination_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("integrations.discord.destination.placeholder"))
                .default_value(initial_settings.discord_destination.clone())
        });
        // Enter and blur commit the label; typing alone stays in the input and
        // is not persisted until one of those events arrives.
        let discord_destination_subscription = cx.subscribe(
            &discord_destination_input,
            |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.save_discord_destination(cx);
                }
            },
        );
        let send_history = db.send_history(100).unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot load the Discord send history");
            Vec::new()
        });
        let send_waiting_count = db.count_waiting_sends().unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot count waiting Discord sends");
            0
        });
        let photo_grid_focus = cx.focus_handle();
        let search_grid_focus = cx.focus_handle();
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
        let ai_license_focus = cx.focus_handle();
        let ai_uninstall_focus = cx.focus_handle();
        let ribbon_collapse_focus = cx.focus_handle();
        let notification_bell_focus = cx.focus_handle();
        let ribbon_import_copy_focus = cx.focus_handle();
        let ribbon_import_move_focus = cx.focus_handle();
        let ribbon_reload_focus = cx.focus_handle();
        let card_size_focus = [
            cx.focus_handle(),
            cx.focus_handle(),
            cx.focus_handle(),
            cx.focus_handle(),
        ];
        let ribbon_inspector_focus = cx.focus_handle();
        let ribbon_select_all_focus = cx.focus_handle();
        let ribbon_clear_selection_focus = cx.focus_handle();
        let ribbon_trash_focus = cx.focus_handle();
        let ribbon_manual_compress_focus = cx.focus_handle();
        let ribbon_compress_folder_focus = cx.focus_handle();
        let ribbon_compress_profile_focus = cx.focus_handle();
        let ribbon_compress_clipboard_focus = cx.focus_handle();
        let ribbon_release_waiting_focus = cx.focus_handle();
        let theme_main_focus: Vec<FocusHandle> =
            main_colors().iter().map(|_| cx.focus_handle()).collect();
        let theme_accent_focus: Vec<FocusHandle> =
            accent_colors().iter().map(|_| cx.focus_handle()).collect();
        // Track which theme swatch holds focus so its color name can be
        // shown in the axis heading; subscriptions live as long as the app.
        let mut theme_focus_subscriptions = Vec::new();
        for (axis, handles) in [
            (ThemeAxis::Main, &theme_main_focus),
            (ThemeAxis::Accent, &theme_accent_focus),
        ] {
            for (index, handle) in handles.iter().enumerate() {
                theme_focus_subscriptions.push(cx.on_focus(handle, window, move |this, _, cx| {
                    this.theme_focused = Some((axis, index));
                    cx.notify();
                }));
                theme_focus_subscriptions.push(cx.on_blur(handle, window, move |this, _, cx| {
                    if this.theme_focused == Some((axis, index)) {
                        this.theme_focused = None;
                        cx.notify();
                    }
                }));
            }
        }
        let empty_index = Arc::new(PhotoIndex::new(Vec::new()));
        let library_bounds = Rc::new(RefCell::new(HashMap::new()));
        let search_bounds = Rc::new(RefCell::new(HashMap::new()));
        let library_bounds_epoch = Rc::new(Cell::new(0));
        let search_bounds_epoch = Rc::new(Cell::new(0));
        let initial_row_height = photo_row_height(MIN_CARD_WIDTH, true, true);
        let library = Rc::new(RefCell::new(PhotoBrowser::new(
            empty_index.clone(),
            initial_row_height,
        )));
        let search_view = Rc::new(RefCell::new(PhotoBrowser::new(
            empty_index,
            initial_row_height,
        )));
        let thumbnails = Rc::new(RefCell::new(ThumbnailCache::default()));
        let app_entity = cx.entity();
        let context_menu_activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.close_photo_context_menu_for_lifecycle(window, cx);
            }
        });
        let context_menu_bounds = cx.observe_window_bounds(window, |this, window, cx| {
            this.close_photo_context_menu_for_lifecycle(window, cx);
        });
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
            sidebar_motion: ToggleMotion::settled(if initial_settings.ui_sidebar_collapsed {
                1.0
            } else {
                0.0
            }),
            nav_previous: Screen::Photos,
            nav_epoch: 0,
            screen_motion: ToggleMotion::settled(1.0),
            screen_direction: 1.0,
            settings_scroll: ScrollHandle::new(),
            integrations_scroll: ScrollHandle::new(),
            ribbon_collapsed: initial_settings.ui_ribbon_collapsed,
            ribbon_motion: ToggleMotion::settled(if initial_settings.ui_ribbon_collapsed {
                1.0
            } else {
                0.0
            }),
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
            zoom: ViewerZoom::default(),
            zoom_drag: None,
            viewer_frame_bounds: Rc::new(Cell::new(Bounds::default())),
            viewer_minimap_bounds: Rc::new(Cell::new(Bounds::default())),
            inspector_collapsed: initial_settings.ui_inspector_collapsed,
            inspector_motion: ToggleMotion::settled(if initial_settings.ui_inspector_collapsed {
                1.0
            } else {
                0.0
            }),
            hovered_nav: None,
            daemon_age_secs: None,
            daemon_supervisor: None,
            daemon_pid: None,
            last_status_version: 0,
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
            selected_ai: None,
            selected_ai_tags: Vec::new(),
            ai_meta_revision: 0,
            ai_meta_cache: ai::AiMetaCache::default(),
            selected_derivatives: Vec::new(),
            compressed_ids: HashSet::new(),
            duplicate_ids: HashSet::new(),
            thumbnails,
            thumbnail_notify_pending: false,
            thumbnail_notice: ThumbnailNoticeBatch::default(),
            thumbnail_notice_scheduled: false,
            photo_grid_focus,
            search_grid_focus,
            photo_context_menu: None,
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
            full_preview: None,
            full_preview_key: None,
            full_preview_queue: PreviewQueue::default(),
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
            settings: initial_settings.clone(),
            ai: ai::AiUiState::default(),
            ai_refresh_scheduled: false,
            ai_selected_refresh_pending: false,
            ai_tag_focused: None,
            ai_license_focus,
            ai_uninstall_focus,
            ai_modal_return_focus: None,
            ai_modal_focus_pending: false,
            ai_tag_picker,
            _ai_tag_picker_query_subscription: ai_tag_picker_query_subscription,
            ai_thresholds,
            _ai_threshold_query_subscription: ai_threshold_query_subscription,
            integrations_auto_motion: ToggleMotion::settled(if initial_settings.auto_compress {
                0.0
            } else {
                1.0
            }),
            integrations_ai_motion: ToggleMotion::settled(if initial_settings.ai_tagging_enabled {
                0.0
            } else {
                1.0
            }),
            integrations_clipboard_motion: ToggleMotion::settled(
                if initial_settings.clipboard_enabled {
                    0.0
                } else {
                    1.0
                },
            ),
            integrations_discord_motion: ToggleMotion::settled(
                if initial_settings.discord_enabled {
                    0.0
                } else {
                    1.0
                },
            ),
            discord_url: initial_discord_url,
            discord_url_editing: false,
            discord_url_input,
            discord_destination_input,
            _discord_destination_subscription: discord_destination_subscription,
            discord_url_error: None,
            send_history,
            send_waiting_count,
            pending_test_send: None,
            thumbnail_quality_slider,
            _thumbnail_quality_subscription: thumbnail_quality_subscription,
            thumbnail_cache_usage: None,
            palette: initial_palette,
            update: update_view::UpdateState::new(health_token, database_ok, demo, db_path),
            jobs: Vec::new(),
            waiting_jobs: Vec::new(),
            queue_scroll: VirtualListScrollHandle::new(),
            active_job_count: 0,
            waiting_job_count: 0,
            daemon_errors: Vec::new(),
            failed_job_count: 0,
            retryable_failed_count: 0,
            failed_jobs: Vec::new(),
            failed_job_names: HashMap::new(),
            import_report: None,
            import_scan_target: None,
            import_auto_compress_enqueued: 0,
            import_dialog: None,
            last_import_progress_notify: None,
            compression_refresh_scheduled: false,
            compression_library_refresh_pending: false,
            live_refresh: LiveRefreshGate::default(),
            monitor_world: None,
            monitor_instance: None,
            monitor_players: Vec::new(),
            daemon_active: false,
            world_input,
            player_input,
            tag_input,
            tag_edit_input,
            tag_edit,
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
            search_refresh_dirty: false,
            search_favorite: false,
            search_days: crate::app::search::DEFAULT_SEARCH_DAYS,
            search_ai_tags: Vec::new(),
            search_people_input,
            daemon_shutdown_armed: false,
            pending_toasts: Vec::new(),
            toast_flush_scheduled: false,
            notification_modal_open: false,
            notification_modal_epoch: 0,
            import_report_expanded: false,
            notification_return_focus: None,
            notification_close_focus,
            notification_modal_focus,
            ribbon_collapse_focus,
            notification_bell_focus,
            ribbon_import_copy_focus,
            ribbon_import_move_focus,
            ribbon_reload_focus,
            card_size_focus,
            ribbon_inspector_focus,
            ribbon_select_all_focus,
            ribbon_clear_selection_focus,
            ribbon_trash_focus,
            ribbon_manual_compress_focus,
            ribbon_compress_folder_focus,
            ribbon_compress_profile_focus,
            ribbon_compress_clipboard_focus,
            ribbon_release_waiting_focus,
            theme_main_focus,
            theme_accent_focus,
            theme_focused: None,
            _theme_focus_subscriptions: theme_focus_subscriptions,
            _context_menu_activation: context_menu_activation,
            _context_menu_bounds: context_menu_bounds,
            deletion_requests: Vec::new(),
            deletion_counts: DeletionCounts::default(),
            deletion_baseline: None,
            failure_baseline: None,
            demo,
            demo_assets,
        };
        app.ensure_profiles();
        app.ai.apply_snapshot(ai::collect_ai_snapshot(&app.db));
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
            app.toast_info(tr!("app.empty_library"), cx);
        }
        if let Some(notice) = app.update.notice() {
            app.toast_info(notice.message(), cx);
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
        // The supervisor is not loaded into App until the first monitor
        // refresh, so read the recorded value from the database.
        let supervisor = app.db.get_setting("daemon_supervisor").ok().flatten();
        if !app.demo
            && !app.daemon_active
            && !updating
            && let Err(error) = start_daemon(supervisor.as_deref())
        {
            app.toast_error(tr_args!("app.daemon_start_error", error = error), cx);
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
        self.sync_photo_context_menu(window, cx);
        let mut root = h_flex()
            .relative()
            .size_full()
            .bg(rgba(self.palette.glass))
            .font_family("Noto Sans JP")
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
                if !this.global_shortcuts_blocked(window, cx) {
                    this.toggle_sidebar(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, window, cx| {
                if !this.global_shortcuts_blocked(window, cx) {
                    this.toggle_inspector(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &CloseNotifications, window, cx| {
                this.close_notification_modal(window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                if !this.global_shortcuts_blocked(window, cx) {
                    this.focus_search(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &OpenQuickLook, window, cx| {
                if !this.global_shortcuts_blocked(window, cx) {
                    this.open_quick_look(window, cx, ViewerOpenSource::Auto)
                }
            }))
            .on_action(cx.listener(|this, _: &OpenPhotoContextMenu, window, cx| {
                if !this.global_shortcuts_blocked(window, cx) {
                    this.open_photo_context_menu_from_keyboard(window, cx)
                }
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
            .on_action(cx.listener(|this, _: &ViewerZoomIn, window, cx| {
                this.zoom_viewer(ZOOM_STEP, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ViewerZoomOut, window, cx| {
                this.zoom_viewer(1.0 / ZOOM_STEP, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ViewerZoomReset, _window, cx| this.reset_viewer_zoom(cx)),
            )
            .on_action(cx.listener(|this, _: &QuickLookNextFocus, window, cx| {
                this.cycle_quick_look_focus(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &QuickLookPrevFocus, window, cx| {
                this.cycle_quick_look_focus(true, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &GridPreviousPhoto, _, cx| this.move_grid_selection(-1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &GridNextPhoto, _, cx| this.move_grid_selection(1, cx)),
            )
            .on_action(cx.listener(|this, _: &GridAbovePhoto, _, cx| {
                this.move_grid_selection_vertical(-1, cx)
            }))
            .on_action(cx.listener(|this, _: &GridBelowPhoto, _, cx| {
                this.move_grid_selection_vertical(1, cx)
            }))
            .on_action(cx.listener(|this, _: &TrashSelectedPhotos, window, cx| {
                this.trash_selected(window, cx)
            }));
        if should_render_shell(self.quick_look_open) {
            let viewport = window.viewport_size();
            let mut layout: ChromeLayout = chrome_layout(
                f32::from(viewport.width),
                f32::from(viewport.height),
                content_insets(window),
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
                .child(self.rail_toggle(cx));
            root = root.child(shell);
            if self.manual_compress.is_some() {
                root = root.child(self.manual_compression_overlay(window, cx));
            }
            if self.import_dialog.is_some() {
                root = root.child(self.import_modal_overlay(window, cx));
            }
            if self.ai.license_modal_open {
                root = root.child(self.ai_license_overlay(window, cx));
            }
            if self.ai.uninstall_modal_open {
                root = root.child(self.ai_uninstall_overlay(window, cx));
            }
            if self.ai_tag_picker.open {
                root = root.child(self.ai_tag_picker_overlay(window, cx));
            }
            if self.ai_thresholds.open {
                root = root.child(self.ai_thresholds_overlay(window, cx));
            }
            if self.tag_edit.open {
                root = root.child(self.tag_edit_overlay(window, cx));
            }
        } else {
            root = root.child(self.quick_look(window, cx));
        }
        let root = root.child(self.viewer_morph_view.clone());
        let root = if let Some(menu) = self.photo_context_menu_overlay() {
            root.child(menu)
        } else {
            root
        };
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
        Screen, ai_overlay_open, any_overlay_open, effective_motion_fps, global_shortcut_blocked,
        retain_recent_bounds, should_notify_library_error, should_render_shell,
    };
    use crate::ui::notifications::{FailureNotice, failure_transition};
    use gpui_kit::{Bounds, point, px, size};
    use std::time::Duration;

    fn refresh_rate(hz: f32) -> Option<Duration> {
        Some(Duration::from_secs_f32(1.0 / hz))
    }

    #[test]
    fn global_shortcut_blocked_only_by_overlay_or_dialog() {
        assert!(!global_shortcut_blocked(false, false));
        assert!(global_shortcut_blocked(true, false));
        assert!(global_shortcut_blocked(false, true));
        assert!(global_shortcut_blocked(true, true));
    }

    #[test]
    fn screen_titles_translate() {
        assert_eq!(Screen::Photos.title_in("ja"), "写真");
        assert_eq!(Screen::Photos.title_in("en"), "Photos");
        assert_eq!(Screen::Integrations.title_in("ja"), "連携・自動");
        assert_eq!(Screen::Settings.title_in("en"), "Settings");
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
    fn overlay_guard_covers_the_viewer_and_all_modals() {
        assert!(!any_overlay_open(false, false, false, false, false));
        assert!(any_overlay_open(true, false, false, false, false));
        assert!(any_overlay_open(false, true, false, false, false));
        assert!(any_overlay_open(false, false, true, false, false));
        assert!(any_overlay_open(false, false, false, true, false));
        assert!(any_overlay_open(false, false, false, false, true));
    }

    #[test]
    fn ai_overlay_guard_covers_picker_and_thresholds() {
        assert!(!ai_overlay_open(false, false, false, false));
        assert!(ai_overlay_open(true, false, false, false));
        assert!(ai_overlay_open(false, true, false, false));
        assert!(ai_overlay_open(false, false, true, false));
        assert!(ai_overlay_open(false, false, false, true));
    }

    #[test]
    fn quick_look_notification_and_dialogs_compose_as_overlays() {
        let all_open = any_overlay_open(true, true, true, true, true);
        assert!(all_open);
        // Only quick look paints over the shell. `should_render_shell` takes
        // that single flag, so the other dialogs must not be folded into it.
        assert!(!should_render_shell(true));
        assert!(should_render_shell(false));

        // The notification modal and the other dialogs still own the input
        // tree even though only quick look hides the shell underneath.
        let dialogs_only = any_overlay_open(false, true, true, true, true);
        assert!(dialogs_only);
        assert!(should_render_shell(false));
        assert!(global_shortcut_blocked(dialogs_only, false));
        assert!(global_shortcut_blocked(
            any_overlay_open(false, false, false, false, true),
            false
        ));
        assert!(global_shortcut_blocked(
            any_overlay_open(true, false, false, false, false),
            true
        ));
    }

    #[test]
    fn every_ai_modal_feeds_the_global_overlay_guard() {
        for ai_open in [
            ai_overlay_open(true, false, false, false),
            ai_overlay_open(false, true, false, false),
            ai_overlay_open(false, false, true, false),
            ai_overlay_open(false, false, false, true),
        ] {
            assert!(ai_open);
            let overlay = any_overlay_open(false, false, false, ai_open, false);
            assert!(overlay);
            assert!(global_shortcut_blocked(overlay, false));
        }
        assert!(!any_overlay_open(
            false,
            false,
            false,
            ai_overlay_open(false, false, false, false),
            false
        ));
    }

    #[test]
    fn every_screen_has_a_distinct_localized_title() {
        let screens = [
            Screen::Photos,
            Screen::Compression,
            Screen::Search,
            Screen::Dashboard,
            Screen::Integrations,
            Screen::Settings,
        ];
        for locale in ["ja", "en"] {
            let titles: Vec<_> = screens
                .iter()
                .map(|screen| screen.title_in(locale))
                .collect();
            for title in &titles {
                assert!(!title.is_empty(), "empty title in {locale}");
            }
            let mut unique = titles.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                unique.len(),
                titles.len(),
                "screen titles must stay distinct in {locale}"
            );
        }
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
