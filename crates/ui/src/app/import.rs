//! App-side import state: source picking, background run, cancellation,
//! retry/resume, and destination indexing.
//!
//! Rendering stays in `ui::import`; these methods only touch App fields and the
//! importer/scanner services.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use gpui_kit::*;
use vrc_photo_core::importer::{self, ImportMode, ImportProgress, ImportResult};
use vrc_photo_core::linker;
use vrc_photo_core::model::DaemonCommand;
use vrc_photo_core::{preview, scanner, thumbnail};

use crate::app::App;
use crate::app::cli::log_timing;
use crate::state::preview::{PreviewKey, preview_cache_dir, preview_key};
use crate::ui::import::retryable_sources;
use crate::{
    ImportDialogOrigin, ImportDialogPhase, ImportDialogState, ImportReportView, ImportSource,
    add_photo_root_and_save, append_scan_error, import_remaining, import_should_index,
    import_summary, merge_resume_report, merge_retry_report, reserve_import_dialog,
};

enum ImportRunMessage {
    Progress(ImportProgress),
    Finished(Result<ImportResult, String>),
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
        cx.notify();
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

    /// Registers, scans, and links the import destination, then rebuilds the
    /// library. Indexing failures stay on the report instead of aborting the
    /// UI, so the imported files and per-file issues remain visible.
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
    pub(crate) fn warm_previews(
        &mut self,
        jobs: Vec<(PreviewKey, PathBuf)>,
        cx: &mut Context<Self>,
    ) {
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
