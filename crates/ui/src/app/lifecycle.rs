//! App lifecycle: startup timers, daemon IPC, library reloads, and navigation.
//!
//! `App::new` stays in `app.rs`; these methods run after construction and keep
//! the live library, job queue, and daemon status in sync.

use std::time::{Duration, Instant};

use gpui_kit::component::notification::NotificationType;
use gpui_kit::*;
use vrc_photo_core::model::{LibraryStats, PhotoSort, QueueStatus};
use vrc_photo_core::settings::AppSettings;
use vrc_photo_ipc::{
    Endpoint, Event as IpcEvent, Request as IpcRequest, ScanPhase as IpcScanPhase,
};

use crate::app::cli::log_timing;
use crate::app::{App, Screen};
use crate::ipc_bridge;
use crate::state::photo_browser::{
    anchor_photo_id, apply_library_index, list_scroll_top, sync_search_mirror,
};
use crate::ui::notifications::{FailureNotice, failure_transition};
use crate::ui::shell::screen_direction;

/// Current daemon scan progress, shown by the sidebar status card and the
/// dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanProgressView {
    pub(crate) phase: IpcScanPhase,
    pub(crate) processed: usize,
    pub(crate) total: Option<usize>,
    pub(crate) indexed: usize,
    pub(crate) root: usize,
    pub(crate) roots: usize,
}

impl App {
    pub(crate) fn start_auto_refresh(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn start_ipc(&mut self, cx: &mut Context<Self>) {
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
                self.reload_job_state(cx);
            }
            IpcEvent::StatusChanged { status } => {
                self.apply_daemon_status(status);
                self.refresh_monitor();
            }
        }
    }

    pub(crate) fn wake_daemon(&self, request: IpcRequest, cx: &mut Context<Self>) {
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

    pub(crate) fn send_ipc_request(
        &mut self,
        request: IpcRequest,
        success: Option<String>,
        failure: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !self.ipc_connected {
            if let Some(message) = failure {
                self.toast_error(message, cx);
            }
            cx.notify();
            return;
        }
        let Some(endpoint) = self.ipc_endpoint.clone() else {
            self.toast_error("IPCの接続先を解決できません", cx);
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
                            this.toast_success(message, cx);
                        }
                    }
                    Err(error) => {
                        this.toast_error(
                            failure.unwrap_or_else(|| ipc_bridge::error_message(&error)),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn refresh_live(&mut self, cx: &mut Context<Self>) {
        let count = self.db.count_photos();
        let revision = self.db.library_revision();
        if let Ok(stats) = self.db.stats() {
            self.stats = stats;
        }
        self.reload_job_state(cx);
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

    pub(crate) fn reload_job_state(&mut self, cx: &mut Context<Self>) {
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
        let failed_count = match self.db.count_jobs(QueueStatus::Failed) {
            Ok(count) => {
                self.failed_job_count = count;
                Some(count)
            }
            Err(error) => {
                tracing::warn!(%error, "cannot count failed compression jobs");
                None
            }
        };
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
        let daemon_count = match self.db.daemon_errors() {
            Ok(errors) => {
                let count = errors.len();
                self.daemon_errors = errors;
                Some(count)
            }
            Err(error) => {
                tracing::warn!(%error, "cannot load daemon errors");
                None
            }
        };
        // Only transition the baseline after both counts were read. A failed
        // read would otherwise make pre-existing failures look new on the next
        // successful refresh.
        if let (Some(failed), Some(daemon)) = (failed_count, daemon_count) {
            let next = (failed, daemon);
            for notice in failure_transition(self.failure_baseline, next) {
                match notice {
                    FailureNotice::Compression { count } => self.toast_actionable(
                        NotificationType::Error,
                        format!("圧縮に {count} 件の失敗があります"),
                        cx,
                    ),
                    FailureNotice::Daemon { count } => self.toast_actionable(
                        NotificationType::Error,
                        format!("デーモンで {count} 件の処理エラーがあります"),
                        cx,
                    ),
                }
            }
            self.failure_baseline = Some(next);
        }
    }

    pub(crate) fn ensure_profiles(&mut self) {
        self.profiles = self.db.ensure_default_profiles().unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot ensure default compression profiles");
            Vec::new()
        });
    }

    /// Refreshes side panels without touching the library snapshot or pages.
    pub(crate) fn reload_aux(&mut self, cx: &mut Context<Self>) {
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
        self.reload_job_state(cx);
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
    pub(crate) fn reload_library(&mut self, preserve_anchor: bool, cx: &mut Context<Self>) -> bool {
        let anchor = preserve_anchor.then(|| {
            anchor_photo_id(
                &self.library.index,
                list_scroll_top(&self.library.scroll),
                self.library.columns,
                self.library.row_height,
            )
        });
        let anchor = anchor.flatten();

        let index_start = Instant::now();
        let result = self.db.photo_index(PhotoSort::Newest);
        log_timing("photo_index", index_start);
        let error = apply_library_index(&mut self.library, result);
        if let Some(error) = error {
            self.note_library_error(error, cx);
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

    /// Full rebuild used at startup.
    pub(crate) fn reload_all(&mut self, reset_scroll: bool, cx: &mut Context<Self>) {
        self.reload_library(!reset_scroll, cx);
    }

    fn mark_library_dirty(&mut self) {
        self.library_dirty = true;
    }

    /// Changes screens without rebuilding the library unless Photos was marked
    /// dirty while another screen was mounted.
    pub(crate) fn switch_screen(&mut self, screen: Screen, cx: &mut Context<Self>) {
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

    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.overlay_open() {
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

    pub(crate) fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        if self.overlay_open() {
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

    pub(crate) fn toggle_ribbon_collapsed(&mut self, cx: &mut Context<Self>) {
        self.set_ribbon_collapsed(!self.ribbon_collapsed, cx);
    }

    fn set_ribbon_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.ribbon_collapsed = collapsed;
        self.ribbon_epoch = self.ribbon_epoch.wrapping_add(1);
        self.settings.ui_ribbon_collapsed = collapsed;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save ribbon state");
        }
        cx.notify();
    }

    pub(crate) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay_open() {
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
}
