//! Import dialog overlay and the post-run import report panel.

use std::path::PathBuf;

use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photon_core::importer::{ImportIssue, ImportIssueKind, ImportMode, ImportProgress};
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::import::{ImportDialogOrigin, ImportDialogPhase, ImportReportView};
use crate::ui::components::dialog_panel;
use crate::ui::theme::*;

pub(crate) fn import_progress_ratio(progress: &ImportProgress) -> f32 {
    if progress.total == 0 {
        0.0
    } else {
        (progress.processed as f32 / progress.total as f32).clamp(0.0, 1.0)
    }
}

/// Reports can carry thousands of issues. The modal is a summary, so each
/// section renders at most this many rows and folds the remainder into one
/// "N more" line. This keeps a large report from building thousands of GPUI
/// children on every frame it is visible.
pub(crate) const IMPORT_REPORT_ROW_LIMIT: usize = 100;

/// Rows to render and the number folded into the trailing "N more" line.
pub(crate) fn import_report_row_window(total: usize) -> (usize, usize) {
    let shown = total.min(IMPORT_REPORT_ROW_LIMIT);
    (shown, total - shown)
}

fn import_more_row(hidden: usize, p: &Palette) -> AnyElement {
    div()
        .pt(px(2.))
        .text_size(px(10.))
        .text_color(rgb(p.muted_2))
        .child(tr_args!("import.section.more", count = hidden))
        .into_any_element()
}

impl App {
    pub(crate) fn import_modal_overlay(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let Some(dialog) = self.import_dialog.as_ref() else {
            return div().into_any_element();
        };
        let title = match &dialog.origin {
            ImportDialogOrigin::New => tr!("import.dialog.title"),
            ImportDialogOrigin::Retry { .. } => tr!("import.dialog.retry"),
            ImportDialogOrigin::Resume { .. } => tr!("import.dialog.resume"),
        };
        let mode = import_mode_label(dialog.mode);
        let dest = dialog.dest.to_string_lossy().to_string();

        let mut panel = dialog_panel(&self.palette)
            .id("import-dialog-panel")
            .w(px(560.))
            .gap(px(16.))
            .p(px(22.))
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
                            .text_color(rgb(p.text))
                            .child(title),
                    )
                    .child(self.badge(mode, p.accent, p.accent_soft))
                    .when_some(dialog.origin.run_summary(), |this, summary| {
                        this.child(self.badge(&summary, p.muted, p.panel_2))
                    })
                    .child(div().flex_1()),
            );

        match &dialog.phase {
            ImportDialogPhase::Ready => {
                let source = dialog.source.as_ref();
                let has_files = source.is_some_and(|source| !source.files().is_empty());
                let mut body = v_flex()
                    .gap(px(10.))
                    .child(self.section_label(tr!("import.section.source")))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                Button::new("import-pick-files")
                                    .label(tr!("import.choose_files"))
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_import_files(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("import-pick-folder")
                                    .label(tr!("import.choose_folder"))
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_import_folder(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("import.folder_hint")),
                    )
                    .when_some(source.map(|source| source.summary()), |this, summary| {
                        this.child(
                            div()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(rgb(p.muted))
                                .child(summary),
                        )
                    })
                    .when_some(
                        source.and_then(|source| {
                            let count = source.issues().len();
                            (count > 0).then(|| tr_args!("import.folder_read_error", count = count))
                        }),
                        |this, warning| {
                            this.child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.amber))
                                    .child(warning),
                            )
                        },
                    )
                    .when_some(dialog.error.as_ref(), |this, error| {
                        this.child(
                            div()
                                .p(px(10.))
                                .rounded(px(8.))
                                .bg(rgb(p.red_soft))
                                .border_1()
                                .border_color(rgb(p.red_border))
                                .text_size(px(10.))
                                .text_color(rgb(p.red))
                                .child(error.message()),
                        )
                    });
                body = body
                    .child(self.section_label(tr!("import.section.destination")))
                    .child(
                        v_flex()
                            .gap(px(4.))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(rgb(p.muted))
                                    .child(dest.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("import.destination_hint")),
                            ),
                    );
                panel = panel.child(body).child(
                    h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("import-cancel")
                                .label(tr!("common.cancel"))
                                .small()
                                .ghost()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.cancel_or_close_import(cx)),
                                ),
                        )
                        .child(
                            Button::new("import-start")
                                .label(tr!("import.start"))
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
                                    .text_color(rgb(p.muted))
                                    .child(import_progress_label(progress)),
                            )
                            .child(
                                div()
                                    .h(px(6.))
                                    .w_full()
                                    .rounded_full()
                                    .bg(rgb(p.panel_2))
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(import_progress_ratio(progress)))
                                            .rounded_full()
                                            .bg(rgb(p.accent)),
                                    ),
                            )
                            .when_some(last_source, |this, file| {
                                this.child(
                                    div()
                                        .truncate()
                                        .text_size(px(10.))
                                        .text_color(rgb(p.muted_2))
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
                                    .text_color(rgb(p.muted_2))
                                    .child(summary),
                            )
                        },
                    )
                    .child(self.section_label(tr!("import.section.destination")))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(p.muted))
                            .child(dest.clone()),
                    )
                    .child(
                        h_flex().justify_end().child(
                            Button::new("import-cancel")
                                .label(if *cancel_requested {
                                    tr!("import.cancelling")
                                } else {
                                    tr!("common.cancel")
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
            .p(px(DIALOG_VIEWPORT_MARGIN))
            .bg(rgba(p.scrim))
            .on_click(cx.listener(|this, _, _, cx| this.cancel_or_close_import(cx)))
            .child(panel)
            .with_animation(
                "import-dialog",
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(self.motion_fps(window, cx)),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }

    pub(crate) fn import_report_details(&self, report: &ImportReportView) -> AnyElement {
        let p = self.palette;
        let (failed, partial) = report.issue_counts();

        let mut body = v_flex().w_full();
        if failed > 0 {
            let (shown_limit, hidden) = import_report_row_window(failed);
            let shown: Vec<&ImportIssue> = report
                .issues
                .iter()
                .filter(|issue| issue.kind == ImportIssueKind::Failed)
                .take(shown_limit)
                .collect();
            body = body.child(
                v_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(rgb(p.border))
                    .child(self.section_label(tr!("import.section.failed_files")))
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .max_h(px(150.))
                            .overflow_y_scrollbar()
                            .children(shown.iter().map(|issue| {
                                let source = issue.source.to_string_lossy().to_string();
                                v_flex()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.red))
                                            .child(source),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted))
                                            .child(issue.message.clone()),
                                    )
                            }))
                            .when(hidden > 0, |this| this.child(import_more_row(hidden, &p))),
                    ),
            );
        }

        if partial > 0 {
            let (shown_limit, hidden) = import_report_row_window(partial);
            let shown: Vec<&ImportIssue> = report
                .issues
                .iter()
                .filter(|issue| issue.kind == ImportIssueKind::PartialMove)
                .take(shown_limit)
                .collect();
            body = body.child(
                v_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(rgb(p.border))
                    .child(self.section_label(tr!("import.section.partial_files")))
                    .children(shown.iter().map(|issue| {
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
                                            .text_color(rgb(p.amber))
                                            .child(source),
                                    )
                                    .when_some(target, |this, target| {
                                        this.child(
                                            div()
                                                .truncate()
                                                .text_size(px(9.))
                                                .text_color(rgb(p.muted_2))
                                                .child(format!("→ {target}")),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted))
                                    .child(issue.message.clone()),
                            )
                    }))
                    .when(hidden > 0, |this| this.child(import_more_row(hidden, &p))),
            );
        }

        if !report.renamed.is_empty() {
            let (shown_limit, hidden) = import_report_row_window(report.renamed.len());
            body = body.child(
                v_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(rgb(p.border))
                    .child(self.section_label(tr!("import.section.renamed_files")))
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .max_h(px(150.))
                            .overflow_y_scrollbar()
                            .children(report.renamed.iter().take(shown_limit).map(|rename| {
                                v_flex()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted))
                                            .child(rename.source.to_string_lossy().to_string()),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(9.))
                                            .text_color(rgb(p.accent))
                                            .child(format!(
                                                "→ {}",
                                                rename.target.to_string_lossy()
                                            )),
                                    )
                            }))
                            .when(hidden > 0, |this| this.child(import_more_row(hidden, &p))),
                    ),
            );
        }

        if !report.scan_failures.is_empty() {
            let (shown_limit, hidden) = import_report_row_window(report.scan_failures.len());
            body = body.child(
                v_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(rgb(p.border))
                    .child(self.section_label(tr!("import.section.scan_failed_files")))
                    .children(
                        report
                            .scan_failures
                            .iter()
                            .take(shown_limit)
                            .map(|failure| {
                                v_flex()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.red))
                                            .child(failure.path.to_string_lossy().to_string()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted))
                                            .child(failure.message.clone()),
                                    )
                            }),
                    )
                    .when(hidden > 0, |this| this.child(import_more_row(hidden, &p))),
            );
        }

        if !report.scan_errors.is_empty() {
            let (shown_limit, hidden) = import_report_row_window(report.scan_errors.len());
            body = body.child(
                v_flex()
                    .mx(px(16.))
                    .my(px(8.))
                    .p(px(8.))
                    .gap(px(2.))
                    .rounded(px(8.))
                    .bg(rgb(p.red_soft))
                    .border_1()
                    .border_color(rgb(p.red_border))
                    .text_size(px(10.))
                    .text_color(rgb(p.red))
                    .children(
                        report
                            .scan_errors
                            .iter()
                            .take(shown_limit)
                            .map(|error| div().child(error.clone())),
                    )
                    .when(hidden > 0, |this| this.child(import_more_row(hidden, &p))),
            );
        }
        body.into_any_element()
    }
}

/// Sources a retry may try again: only `Failed` issues marked retryable.
pub(crate) fn retryable_sources(issues: &[ImportIssue]) -> Vec<PathBuf> {
    issues
        .iter()
        .filter(|issue| issue.kind == ImportIssueKind::Failed && issue.retryable)
        .map(|issue| issue.source.clone())
        .collect()
}

/// Whether the report has any source a retry can try. This avoids cloning
/// every retryable path just to decide whether the button is shown.
pub(crate) fn has_retryable_sources(issues: &[ImportIssue]) -> bool {
    issues
        .iter()
        .any(|issue| issue.kind == ImportIssueKind::Failed && issue.retryable)
}

pub(crate) fn import_mode_label(mode: ImportMode) -> String {
    match mode {
        ImportMode::Copy => tr!("import.mode.copy"),
        ImportMode::Move => tr!("import.mode.move"),
    }
}

pub(crate) fn import_progress_label(progress: &ImportProgress) -> String {
    tr_args!(
        "import.progress",
        processed = progress.processed,
        total = progress.total,
        imported = progress.imported,
        skipped = progress.skipped,
        failed = progress.failed
    )
}

#[cfg(test)]
mod tests {
    use super::{
        IMPORT_REPORT_ROW_LIMIT, has_retryable_sources, import_progress_label,
        import_progress_ratio, import_report_row_window, retryable_sources,
    };
    use crate::app::import::import_issue_counts;
    use std::path::PathBuf;
    use vrc_photon_core::importer::{ImportIssue, ImportIssueKind, ImportProgress};

    #[test]
    fn large_report_sections_render_only_the_documented_row_cap() {
        assert_eq!(import_report_row_window(0), (0, 0));
        assert_eq!(import_report_row_window(3), (3, 0));
        assert_eq!(
            import_report_row_window(IMPORT_REPORT_ROW_LIMIT),
            (IMPORT_REPORT_ROW_LIMIT, 0)
        );

        let (shown, hidden) = import_report_row_window(1_000);
        assert_eq!(shown, IMPORT_REPORT_ROW_LIMIT);
        assert_eq!(hidden, 1_000 - IMPORT_REPORT_ROW_LIMIT);
        assert_eq!(shown, 100);
        assert_eq!(hidden, 900);
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
        assert!(has_retryable_sources(&issues));
        assert!(!has_retryable_sources(&issues[1..]));
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
}
