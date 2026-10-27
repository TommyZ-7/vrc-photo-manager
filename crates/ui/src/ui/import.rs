//! Import dialog overlay and the post-run import report panel.

use std::path::PathBuf;

use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::importer::{ImportIssue, ImportIssueKind, ImportMode, ImportProgress};

use crate::app::App;
use crate::app::import::{
    ImportDialogOrigin, ImportDialogPhase, ImportReportView, import_issue_counts,
};
use crate::ui::components::dialog_panel;
use crate::ui::theme::*;

pub(crate) fn import_progress_ratio(progress: &ImportProgress) -> f32 {
    if progress.total == 0 {
        0.0
    } else {
        (progress.processed as f32 / progress.total as f32).clamp(0.0, 1.0)
    }
}

impl App {
    pub(crate) fn import_modal_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
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

        let mut panel = dialog_panel()
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
            .p(px(DIALOG_VIEWPORT_MARGIN))
            .bg(rgba(0x05070acc))
            .on_click(cx.listener(|this, _, _, cx| this.cancel_or_close_import(cx)))
            .child(panel)
            .with_animation(
                "import-dialog",
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(self.motion_fps()),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }

    pub(crate) fn import_report_panel(
        &self,
        report: &ImportReportView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            .mx(px(GALLERY_PAD_X))
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
                    .with_max_fps(self.motion_fps()),
                |el, t| el.opacity(t).relative().top(px(-8.0 * (1.0 - t))),
            )
            .into_any_element()
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

pub(crate) fn import_mode_label(mode: ImportMode) -> &'static str {
    match mode {
        ImportMode::Copy => "コピー",
        ImportMode::Move => "移動",
    }
}

pub(crate) fn import_progress_label(progress: &ImportProgress) -> String {
    format!(
        "取り込み {}/{} · 取込 {} · スキップ {} · 失敗 {}",
        progress.processed, progress.total, progress.imported, progress.skipped, progress.failed
    )
}

#[cfg(test)]
mod tests {
    use super::{
        import_issue_counts, import_progress_label, import_progress_ratio, retryable_sources,
    };
    use std::path::PathBuf;
    use vrc_photo_core::importer::{ImportIssue, ImportIssueKind, ImportProgress};

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
