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
    Endpoint, Event as IpcEvent, JobStatus, Request as IpcRequest, ScanPhase as IpcScanPhase,
};

use crate::app::cli::log_timing;
use crate::app::{App, Screen};
use crate::ipc_bridge;
use crate::state::photo_browser::{
    anchor_photo_id, apply_library_index, list_scroll_top, sync_search_mirror,
};
use crate::ui::notifications::{FailureNotice, failure_transition};
use crate::ui::palette::{Palette, apply_component_theme, resolve};
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

/// True when a live snapshot was collected before a revision the UI has
/// already applied. Revisions are monotonic; an older observation must not
/// trigger a reload or count comparison.
pub(crate) fn is_stale_revision(observed: Option<i64>, current: i64) -> bool {
    observed.is_some_and(|observed| observed < current)
}

/// Quiet period a revision or photo count must hold before the UI rebuilds the
/// whole index. Import bursts bump both several times per second.
const LIBRARY_QUIET_PERIOD: Duration = Duration::from_secs(2);

impl App {
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
                self.refresh_library_after_scan(cx);
            }
            IpcEvent::CompressionProgress { status, .. } => {
                let library_changed = matches!(
                    status,
                    JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
                );
                self.schedule_compression_refresh(library_changed, cx);
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

    /// Coalesces compression progress into one refresh per 250 ms. The daemon
    /// reports once at job start and once at completion; a batch of 1,000 jobs
    /// used to rebuild the 100k index once per completion.
    fn schedule_compression_refresh(&mut self, library_changed: bool, cx: &mut Context<Self>) {
        self.compression_library_refresh_pending |= library_changed;
        if self.compression_refresh_scheduled {
            return;
        }
        self.compression_refresh_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            this.update(cx, |this, cx| {
                this.compression_refresh_scheduled = false;
                if this.compression_library_refresh_pending {
                    this.compression_library_refresh_pending = false;
                    this.refresh_after_compression(cx);
                } else {
                    this.reload_job_state(cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Compression changes photo metadata (display derivative, replaced
    /// status) but not the id set or order, so the index snapshot stays valid.
    /// Invalidate the page cache and refresh the side panels instead of
    /// re-querying the 100k-row index.
    fn refresh_after_compression(&mut self, cx: &mut Context<Self>) {
        // Claim the revision before the (short) metadata reload so a poll
        // that lands mid-refresh does not start a full index reload.
        if let Ok(revision) = self.db.library_revision() {
            self.last_library_revision = revision;
        }
        self.revision_debounce.reset();
        self.count_debounce.reset();
        self.library.borrow_mut().invalidate_pages();
        self.search_view.borrow_mut().invalidate_pages();
        self.reload_compression_aux(cx);
        if self.screen != Screen::Photos {
            self.mark_library_dirty();
        }
        cx.notify();
    }

    /// The subset of `reload_aux` that compression can change. Duplicate
    /// hashes, settings, and the session monitor are untouched by a job.
    fn reload_compression_aux(&mut self, cx: &mut Context<Self>) {
        match self.db.stats() {
            Ok(stats) => {
                self.stats = stats;
                self.stats_revision = self.db.library_revision().unwrap_or(-1);
            }
            Err(error) => tracing::warn!(%error, "cannot load library stats"),
        }
        self.compressed_ids = self
            .db
            .compressed_photo_ids()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot load compressed photo ids");
                Vec::new()
            })
            .into_iter()
            .collect();
        self.reload_job_state(cx);
        if let Some(id) = self.selected {
            self.load_selected_meta(id);
            self.refresh_stale_preview(cx);
        }
    }

    pub(crate) fn refresh_live(&mut self, cx: &mut Context<Self>) {
        let count = self.db.count_photos();
        let revision = self.db.library_revision();
        let revision_value = revision.as_ref().ok().copied();
        if let Some(revision) = revision_value
            && revision != self.stats_revision
            && let Ok(stats) = self.db.stats()
        {
            self.stats = stats;
            self.stats_revision = revision;
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
        self.apply_library_change(revision_value, count.ok(), cx);
        cx.notify();
    }

    /// Applies a revision/count observation from the live poller. Returns true
    /// when the visible library was rebuilt or a search was re-run.
    pub(crate) fn apply_library_change(
        &mut self,
        revision: Option<i64>,
        count: Option<i64>,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut changed = false;
        // A snapshot can be collected just before the UI bumps the revision
        // (trash, compression). Revisions are monotonic, so ignore an older
        // observation instead of reacting to it as a new change.
        if is_stale_revision(revision, self.last_library_revision) {
            return false;
        }
        let now = Instant::now();
        // A terminal compression refresh was already scheduled for this
        // revision. Its light path invalidates pages and reloads the side
        // panels without re-querying the 100k index; let it consume the
        // revision instead of racing it with a full reload.
        let compression_refresh_pending = self.compression_library_refresh_pending;
        if let Some(revision) = revision
            && revision > self.last_library_revision
            && !compression_refresh_pending
        {
            let expensive = self.screen == Screen::Photos
                || (self.screen == Screen::Search && self.applied_search.is_some());
            if expensive {
                // A burst of revisions must not rebuild the 100k-entry index
                // on every poll tick. Wait until the revision has been stable
                // for one quiet period.
                if self
                    .revision_debounce
                    .observe(revision, now, LIBRARY_QUIET_PERIOD)
                {
                    if self.screen == Screen::Photos {
                        if self.apply_library_snapshot(Some(revision), cx) {
                            changed = true;
                        }
                    } else {
                        self.last_library_revision = revision;
                        self.revision_debounce.reset();
                        if let Some(filter) = self.applied_search.clone() {
                            self.start_search(filter, cx);
                            changed = true;
                        }
                    }
                } else {
                    self.mark_library_dirty();
                }
            } else {
                self.last_library_revision = revision;
                self.mark_library_dirty();
                if let Some(id) = self.selected {
                    self.load_selected_meta(id);
                    self.refresh_stale_preview(cx);
                }
            }
        }

        // Only compare counts when the query succeeds; treating a transient
        // error as zero used to blank the whole library.
        if let Some(count) = count {
            if count != self.library.borrow().index.total as i64 {
                // A burst of files arriving from the daemon must not rebuild
                // the 100k-entry index on every three-second tick. Wait until
                // the observed count has been stable for a quiet period.
                self.mark_library_dirty();
                if self.screen == Screen::Photos
                    && self
                        .count_debounce
                        .observe(count, now, LIBRARY_QUIET_PERIOD)
                    && self.apply_library_snapshot(revision, cx)
                {
                    changed = true;
                }
            } else {
                self.count_debounce.reset();
            }
        }
        changed
    }

    /// Rebuilds the visible library snapshot and records the revision that
    /// triggered it. `observed_revision` must be read before the snapshot
    /// query so a concurrent commit cannot be swallowed.
    pub(crate) fn apply_library_snapshot(
        &mut self,
        observed_revision: Option<i64>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.reload_library(true, cx) {
            self.mark_library_dirty();
            return false;
        }
        if let Some(revision) = observed_revision {
            self.last_library_revision = self.last_library_revision.max(revision);
        }
        self.count_debounce.reset();
        self.revision_debounce.reset();
        true
    }

    /// A finished scan is a natural end to a revision burst, so show its
    /// result once immediately. Watcher batches after this point still go
    /// through the quiet-period debounce.
    fn refresh_library_after_scan(&mut self, cx: &mut Context<Self>) {
        let revision = self.db.library_revision().ok();
        match self.screen {
            Screen::Photos => {
                self.apply_library_snapshot(revision, cx);
            }
            Screen::Search if self.applied_search.is_some() => {
                if let Some(revision) = revision {
                    self.last_library_revision = self.last_library_revision.max(revision);
                }
                self.revision_debounce.reset();
                self.count_debounce.reset();
                if let Some(filter) = self.applied_search.clone() {
                    self.start_search(filter, cx);
                }
            }
            _ => {
                if let Some(revision) = revision {
                    self.last_library_revision = self.last_library_revision.max(revision);
                }
                self.mark_library_dirty();
            }
        }
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
        let failed_ids: Vec<i64> = self.failed_jobs.iter().map(|job| job.photo_id).collect();
        self.failed_job_names = self
            .db
            .photos_by_ids(&failed_ids)
            .map(|photos| {
                photos
                    .into_iter()
                    .map(|photo| (photo.id, photo.file_name))
                    .collect()
            })
            .unwrap_or_default();
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
        match self.db.stats() {
            Ok(stats) => {
                self.stats = stats;
                self.stats_revision = self.db.library_revision().unwrap_or(-1);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot load library stats");
                self.stats = LibraryStats::default();
                self.stats_revision = -1;
            }
        }
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
        let palette = resolve(&self.settings.ui_theme_main, &self.settings.ui_theme_accent);
        if palette != self.palette {
            self.palette = palette;
            apply_component_theme(self.palette, cx);
        }
        self.refresh_monitor();
        if let Some(id) = self.selected {
            self.load_selected_meta(id);
            self.refresh_stale_preview(cx);
        }
    }

    /// Applies a resolved palette to the app, the component widgets, and the
    /// caches that bake colours into shaped text or morph state.
    pub(crate) fn apply_palette(&mut self, palette: Palette, cx: &mut Context<Self>) {
        self.palette = palette;
        apply_component_theme(palette, cx);
        self.library.borrow_mut().labels = None;
        self.search_view.borrow_mut().labels = None;
        self.viewer_morph_view
            .update(cx, |view, cx| view.set_palette(palette, cx));
        cx.notify();
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
            let browser = self.library.borrow();
            anchor_photo_id(
                &browser.index,
                list_scroll_top(&browser.scroll),
                browser.columns,
                browser.row_height,
            )
        });
        let anchor = anchor.flatten();

        let index_start = Instant::now();
        let result = self.db.photo_index(PhotoSort::Newest);
        log_timing("photo_index", index_start);
        let error = apply_library_index(&mut self.library.borrow_mut(), result);
        if let Some(error) = error {
            self.note_library_error(error, cx);
            // Leave the mounted view and side panels untouched; the next
            // successful reload repopulates them.
            return false;
        }

        self.library_error = None;
        self.library_dirty = false;
        let (columns, scroll) = {
            let browser = self.library.borrow();
            (browser.columns.max(1), browser.scroll.clone())
        };
        if let Some(row) = anchor.and_then(|id| self.library.borrow().index.row_of_id(id)) {
            scroll.scroll_to_item_strict(row / columns, ScrollStrategy::Top);
        } else {
            scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        }

        // An active result set stays mounted until the user clears it or runs
        // a new search; the inactive mirror only needs the fresh snapshot.
        if !self.search_active {
            let index = self.library.borrow().index.clone();
            self.search_view.borrow_mut().replace_index(index);
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
            let revision = self.db.library_revision().ok();
            self.apply_library_snapshot(revision, cx);
        } else if screen == Screen::Search && !self.search_active {
            // The no-query mirror only needs a refresh when the library
            // snapshot changed while another screen was mounted. A dirty
            // library is rebuilt first so the mirror also sees new photos.
            if self.library_dirty {
                let revision = self.db.library_revision().ok();
                self.apply_library_snapshot(revision, cx);
            } else {
                let mut search = self.search_view.borrow_mut();
                let library = self.library.borrow();
                sync_search_mirror(&mut search, &library);
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

#[cfg(test)]
mod tests {
    use super::is_stale_revision;

    #[test]
    fn only_older_live_revisions_are_stale() {
        assert!(is_stale_revision(Some(4), 5));
        assert!(!is_stale_revision(Some(5), 5));
        assert!(!is_stale_revision(Some(6), 5));
        assert!(!is_stale_revision(None, 5));
    }
}
