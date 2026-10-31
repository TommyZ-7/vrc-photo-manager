//! App-side import state: source picking, background run, cancellation,
//! retry/resume, and destination indexing.
//!
//! Rendering stays in `ui::import`; these methods only touch App fields and the
//! importer/scanner services.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui_kit::component::notification::NotificationType;
use gpui_kit::*;
use vrc_photo_core::importer::{
    self, ImportIssue, ImportIssueKind, ImportMode, ImportProgress, ImportRename, ImportResult,
};
use vrc_photo_core::linker;
use vrc_photo_core::scanner;
use vrc_photo_core::scanner::ScanFailure;
use vrc_photo_core::settings::AppSettings;

use crate::app::App;
use crate::ui::import::retryable_sources;

#[derive(Clone)]
pub(crate) struct ImportReportView {
    pub(crate) mode: ImportMode,
    pub(crate) dest: PathBuf,
    pub(crate) total: usize,
    pub(crate) processed: usize,
    pub(crate) imported: usize,
    pub(crate) skipped: usize,
    pub(crate) issues: Vec<ImportIssue>,
    pub(crate) renamed: Vec<ImportRename>,
    pub(crate) remaining: Vec<PathBuf>,
    pub(crate) cancelled: bool,
    pub(crate) scan_failures: Vec<ScanFailure>,
    pub(crate) scan_error: Option<String>,
    /// Cached `(failed, partial)` counts. Reports can carry thousands of
    /// issues and the sidebar status card is rebuilt on every render; recount
    /// only after a merge mutates `issues`.
    pub(crate) issue_counts: Cell<Option<(usize, usize)>>,
}

impl ImportReportView {
    pub(crate) fn issue_counts(&self) -> (usize, usize) {
        if let Some(counts) = self.issue_counts.get() {
            return counts;
        }
        let counts = import_issue_counts(&self.issues);
        self.issue_counts.set(Some(counts));
        counts
    }

    /// Discards the cached counts after `issues` was modified.
    fn invalidate_issue_counts(&mut self) {
        self.issue_counts.set(None);
    }
}

#[derive(Clone)]
pub(crate) enum ImportSource {
    Files(Vec<PathBuf>),
    Folder {
        folder: PathBuf,
        files: Vec<PathBuf>,
        issues: Vec<ImportIssue>,
    },
}

impl ImportSource {
    pub(crate) fn files(&self) -> &[PathBuf] {
        match self {
            Self::Files(files) => files,
            Self::Folder { files, .. } => files,
        }
    }

    pub(crate) fn issues(&self) -> &[ImportIssue] {
        match self {
            Self::Files(_) => &[],
            Self::Folder { issues, .. } => issues,
        }
    }

    pub(crate) fn summary(&self) -> String {
        match self {
            Self::Files(files) => format!("{} 件のファイル", files.len()),
            Self::Folder { folder, files, .. } => {
                format!("{} · {} 件", folder.display(), files.len())
            }
        }
    }
}

#[derive(Clone)]
pub(crate) enum ImportDialogOrigin {
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
    pub(crate) fn run_summary(&self) -> Option<String> {
        match self {
            Self::New => None,
            Self::Retry { sources, .. } => Some(format!("失敗分 {} 件", sources.len())),
            Self::Resume { sources, .. } => Some(format!("残り {} 件", sources.len())),
        }
    }
}

#[derive(Clone)]
pub(crate) enum ImportDialogPhase {
    Ready,
    Running {
        progress: ImportProgress,
        cancel: Arc<AtomicBool>,
        cancel_requested: bool,
    },
}

#[derive(Clone)]
pub(crate) struct ImportDialogState {
    pub(crate) origin: ImportDialogOrigin,
    pub(crate) mode: ImportMode,
    pub(crate) dest: PathBuf,
    pub(crate) source: Option<ImportSource>,
    pub(crate) error: Option<String>,
    pub(crate) phase: ImportDialogPhase,
}

enum ImportRunMessage {
    Progress(ImportProgress),
    Finished(Result<ImportResult, String>),
}

/// The importer reports once per file. A large folder import would otherwise
/// rebuild the whole window for every copied photo.
const IMPORT_PROGRESS_NOTIFY_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn should_notify_import_progress(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= IMPORT_PROGRESS_NOTIFY_INTERVAL)
}

impl App {
    pub(crate) fn open_import_dialog(
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

    pub(crate) fn pick_import_files(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn pick_import_folder(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn start_import_from_source(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn start_import_run(&mut self, files: Vec<PathBuf>, cx: &mut Context<Self>) {
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
        self.last_import_progress_notify = None;
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

    pub(crate) fn update_import_progress(
        &mut self,
        progress: ImportProgress,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.import_dialog
            && let ImportDialogPhase::Running {
                progress: current, ..
            } = &mut dialog.phase
        {
            *current = progress;
        }
        let now = Instant::now();
        if should_notify_import_progress(self.last_import_progress_notify, now) {
            self.last_import_progress_notify = Some(now);
            cx.notify();
        }
    }

    pub(crate) fn finish_import_run(
        &mut self,
        result: Result<ImportResult, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.import_dialog.take() else {
            return;
        };
        match result {
            Err(error) => match dialog.origin {
                ImportDialogOrigin::New => {
                    self.toast_error(format!("取り込みに失敗: {error}"), cx);
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
                    self.toast_error(format!("再試行に失敗: {error}"), cx);
                }
                ImportDialogOrigin::Resume { .. } => {
                    self.toast_error(format!("再開に失敗: {error}"), cx);
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
                            issue_counts: Cell::new(None),
                        }
                    }
                    ImportDialogOrigin::Retry { report, sources } => {
                        merge_retry_report(&report, &sources, result)
                    }
                    ImportDialogOrigin::Resume { report, .. } => {
                        merge_resume_report(&report, result)
                    }
                };
                self.import_auto_compress_enqueued = 0;
                if import_should_index(imported) {
                    report.scan_failures.clear();
                    report.scan_error = None;
                    self.index_import_destination(&mut report, cx);
                }
                let summary = import_summary(&report);
                let (failed, partial) = report.issue_counts();
                let scan_failed =
                    report.scan_failures.len() + usize::from(report.scan_error.is_some());
                let tone = if failed == 0
                    && partial == 0
                    && scan_failed == 0
                    && report.remaining.is_empty()
                {
                    NotificationType::Success
                } else {
                    NotificationType::Warning
                };
                self.toast_actionable(tone, summary, cx);
                self.import_report = Some(report);
                self.import_report_expanded = false;
                self.import_dialog = None;
                cx.notify();
            }
        }
    }

    pub(crate) fn cancel_or_close_import(&mut self, cx: &mut Context<Self>) {
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

    /// Registers the import destination and asks the daemon to rescan it.
    ///
    /// Indexing, linking, and thumbnail generation stay in the daemon: doing
    /// that work here blocked the UI thread for seconds on a large import and
    /// duplicated the daemon's own post-scan work.
    pub(crate) fn index_import_destination(
        &mut self,
        report: &mut ImportReportView,
        cx: &mut Context<Self>,
    ) {
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
        if self.demo {
            // Demo mode never starts the daemon; keep the small synchronous
            // index so importing into the sample library still works.
            self.index_demo_import_destination(root_id, report, cx);
            return;
        }
        // The daemon may not have watched the new import destination yet.
        // A targeted scan makes the daemon reload settings and rebuild its
        // recursive watchers, then index only this root and generate
        // thumbnails with its own workers. The scan-finished event drives the
        // UI reload.
        match request_import_scan(&self.db, &report.dest) {
            Ok(_) => {
                self.import_scan_pending = true;
                self.send_wake_commands(cx);
                if !self.ipc_connected {
                    self.toast_warning(
                        "デーモンに接続できないため、取り込み先のスキャンは起動後に行われます",
                        cx,
                    );
                }
            }
            Err(error) => {
                tracing::warn!(%error, "cannot request scan for import destination");
                append_scan_error(
                    report,
                    format!("取り込み先のスキャンを要求できません: {error}"),
                );
            }
        }
    }

    fn index_demo_import_destination(
        &mut self,
        root_id: i64,
        report: &mut ImportReportView,
        cx: &mut Context<Self>,
    ) {
        match scanner::scan_and_index(&self.db, root_id, &report.dest) {
            Ok(scan) => report.scan_failures.extend(scan.failures),
            Err(error) => {
                tracing::warn!(%error, "cannot scan import destination");
                append_scan_error(report, format!("取り込み先をスキャンできません: {error}"));
                return;
            }
        }
        if let Err(error) = linker::relink_unlinked(&self.db, 1000) {
            tracing::warn!(%error, "cannot link imported photos");
            append_scan_error(report, format!("写真の紐付けに失敗しました: {error}"));
        }
        let revision = self.db.library_revision().ok();
        if !self.apply_library_snapshot(revision, cx)
            && let Some(error) = self.library_error.clone()
        {
            append_scan_error(report, error);
        }
    }

    /// Opens the import modal in retry mode for the retryable failed sources.
    pub(crate) fn open_retry_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(crate) fn open_resume_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
}
/// Import failures and partial moves. Partial moves reached the destination,
/// so they are warnings and never count as failures.
pub(crate) fn import_issue_counts(issues: &[ImportIssue]) -> (usize, usize) {
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

/// Queues a scan of only the directory the import wrote to.
pub(crate) fn request_import_scan(
    db: &vrc_photo_core::Db,
    dest: &Path,
) -> vrc_photo_core::Result<bool> {
    db.request_daemon_scan_root(&dest.to_string_lossy())
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
    merged.invalidate_issue_counts();
    merged
}

fn merge_resume_report(previous: &ImportReportView, result: ImportResult) -> ImportReportView {
    let mut merged = previous.clone();
    merged.imported += result.imported.len();
    merged.skipped += result.skipped;
    merged.processed += result.processed;
    merged.issues.extend(result.issues);
    merged.renamed.extend(result.renamed);
    merged.invalidate_issue_counts();
    let consumed = result.processed.min(previous.remaining.len());
    merged.remaining = previous.remaining.iter().skip(consumed).cloned().collect();
    merged.cancelled = !merged.remaining.is_empty();
    merged
}

fn import_summary(report: &ImportReportView) -> String {
    let (failed, _) = report.issue_counts();
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

#[cfg(test)]
mod tests {
    use super::{
        IMPORT_PROGRESS_NOTIFY_INTERVAL, ImportDialogOrigin, ImportDialogPhase, ImportDialogState,
        ImportReportView, add_photo_root_and_save, import_remaining, import_should_index,
        import_summary, merge_resume_report, merge_retry_report, request_import_scan,
        reserve_import_dialog, should_notify_import_progress,
    };
    use crate::ui::import::retryable_sources;
    use std::cell::Cell;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use vrc_photo_core::importer::{
        ImportIssue, ImportIssueKind, ImportMode, ImportRename, ImportResult,
    };
    use vrc_photo_core::scanner::ScanFailure;
    use vrc_photo_core::settings::AppSettings;

    #[test]
    fn import_scan_request_targets_the_destination_root() {
        let db = vrc_photo_core::Db::open_in_memory().unwrap();
        let dest = Path::new("/photos/imported");
        assert!(request_import_scan(&db, dest).unwrap());
        assert_eq!(
            db.take_daemon_commands().unwrap(),
            vec![vrc_photo_core::model::DaemonCommand::ScanRoot {
                path: "/photos/imported".to_string(),
            }]
        );
    }

    #[test]
    fn import_progress_notifications_are_throttled() {
        let start = Instant::now();
        assert!(should_notify_import_progress(None, start));
        assert!(!should_notify_import_progress(
            Some(start),
            start + Duration::from_millis(49)
        ));
        assert!(should_notify_import_progress(
            Some(start),
            start + IMPORT_PROGRESS_NOTIFY_INTERVAL
        ));
    }

    #[test]
    fn report_issue_counts_are_cached_and_invalidated_on_change() {
        let mut report = ImportReportView {
            issues: vec![non_retryable_issue("/a.png"), partial_issue("/b.png")],
            ..import_report_fixture()
        };
        assert_eq!(report.issue_counts(), (1, 1));

        report.issues.push(non_retryable_issue("/c.png"));
        report.invalidate_issue_counts();
        assert_eq!(report.issue_counts(), (2, 1));
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
            issue_counts: Cell::new(None),
        }
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
}
