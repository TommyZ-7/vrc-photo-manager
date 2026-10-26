pub(crate) mod cli;

pub(crate) use cli::LaunchOptions;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::*;
use vrc_photo_core::db::Db;
use vrc_photo_core::model::{
    CompressionJob, CompressionProfile, DaemonError, Derivative, LibraryStats, Photo, PhotoPlayer,
    PhotoTextFilter,
};
use vrc_photo_core::settings::AppSettings;
use vrc_photo_ipc::Endpoint;

use crate::update_view;
use crate::{
    CardBoundsMap, CountDebounce, DemoAssets, ImportDialogState, ImportReportView, InspectorBounds,
    ManualCompressionDialog, PhotoBrowser, PreviewKey, PreviewQueue, ScanProgressView,
    SidebarStatusTab, ThumbnailCache, ViewerBoxState, ViewerMorph, ViewerOpenSource,
    ViewerPanelState,
};

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
    pub(crate) render_epoch: u64,
    pub(crate) card_image_bounds: CardBoundsMap,
    pub(crate) inspector_preview_bounds: InspectorBounds,
    pub(crate) viewer_morph: Option<ViewerMorph>,
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
    pub(crate) selection_epoch: u64,
    pub(crate) selection_changed_at: Option<Instant>,
    pub(crate) selected_photo: Option<Photo>,
    pub(crate) selected_players: Vec<PhotoPlayer>,
    pub(crate) selected_tags: Vec<String>,
    pub(crate) selected_derivatives: Vec<Derivative>,
    pub(crate) compressed_ids: HashSet<i64>,
    pub(crate) duplicate_ids: HashSet<i64>,
    pub(crate) thumbnails: ThumbnailCache,
    pub(crate) grid_epoch: u64,
    pub(crate) hovered_photo: Option<i64>,
    pub(crate) intro_seen: HashMap<i64, Instant>,
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
    pub(crate) delete_armed: Option<i64>,
    pub(crate) daemon_shutdown_armed: bool,
    pub(crate) message: Option<String>,
    pub(crate) demo: bool,
    pub(crate) demo_assets: Option<DemoAssets>,
}
