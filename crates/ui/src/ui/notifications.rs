//! Transient toasts and the action-required notification modal.
//!
//! Toasts are delivered through gpui-component's `NotificationList` so
//! stacking, auto-hide, and enter/exit animations stay consistent across the
//! app. The modal is app-owned because it hosts the import report summary and
//! the failure actions that are specific to this application.

use gpui_kit::assets::IconName;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::App;
use crate::app::Screen;
use crate::app::import::{ImportReportView, import_issue_counts};
use crate::ui::components::{ButtonState, ButtonTone};
use crate::ui::import::{import_mode_label, retryable_sources};
use crate::ui::theme::*;

#[derive(Clone, Debug)]
pub(crate) struct ToastSpec {
    pub(crate) tone: NotificationType,
    pub(crate) message: SharedString,
    pub(crate) opens_notifications: bool,
}

/// One failure domain that crossed its last-seen count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureNotice {
    Compression { count: i64 },
    Daemon { count: usize },
}

/// Returns the failures that appeared since `previous`. `None` means the
/// baseline has not been seeded yet, so existing failures stay silent.
pub(crate) fn failure_transition(
    previous: Option<(i64, usize)>,
    next: (i64, usize),
) -> Vec<FailureNotice> {
    let Some((previous_compression, previous_daemon)) = previous else {
        return Vec::new();
    };
    let mut notices = Vec::new();
    if next.0 > previous_compression {
        notices.push(FailureNotice::Compression { count: next.0 });
    }
    if next.1 > previous_daemon {
        notices.push(FailureNotice::Daemon { count: next.1 });
    }
    notices
}

/// The action-required blocks shown by the notification modal, in display
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotificationSection {
    Import,
    Library,
    Compression,
    Daemon,
}

pub(crate) fn notification_sections(
    import: bool,
    library_error: bool,
    failed_jobs: i64,
    daemon_errors: usize,
) -> Vec<NotificationSection> {
    let mut sections = Vec::new();
    if import {
        sections.push(NotificationSection::Import);
    }
    if library_error {
        sections.push(NotificationSection::Library);
    }
    if failed_jobs > 0 {
        sections.push(NotificationSection::Compression);
    }
    if daemon_errors > 0 {
        sections.push(NotificationSection::Daemon);
    }
    sections
}

/// Whether a render should schedule the deferred toast flush.
pub(crate) fn toast_flush_needed(pending: usize, scheduled: bool) -> bool {
    pending > 0 && !scheduled
}

/// The notification modal must not stack over another modal.
pub(crate) fn notification_modal_allowed(import_open: bool, manual_open: bool) -> bool {
    !import_open && !manual_open
}

fn notification_for(spec: &ToastSpec, entity: WeakEntity<App>) -> Notification {
    let mut note = Notification::new()
        .message(spec.message.clone())
        .with_type(spec.tone);
    if spec.opens_notifications {
        note = note.on_click(move |_, window, cx| {
            entity
                .update(cx, |this, cx| this.open_notification_modal(window, cx))
                .ok();
        });
    }
    note
}

impl App {
    pub(crate) fn toast(
        &mut self,
        tone: NotificationType,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.enqueue_toast(
            ToastSpec {
                tone,
                message: message.into(),
                opens_notifications: false,
            },
            cx,
        );
    }

    pub(crate) fn toast_info(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast(NotificationType::Info, message, cx);
    }

    pub(crate) fn toast_success(
        &mut self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.toast(NotificationType::Success, message, cx);
    }

    pub(crate) fn toast_warning(
        &mut self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.toast(NotificationType::Warning, message, cx);
    }

    pub(crate) fn toast_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast(NotificationType::Error, message, cx);
    }

    /// A toast that opens the notification modal when clicked.
    pub(crate) fn toast_actionable(
        &mut self,
        tone: NotificationType,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.enqueue_toast(
            ToastSpec {
                tone,
                message: message.into(),
                opens_notifications: true,
            },
            cx,
        );
    }

    fn enqueue_toast(&mut self, spec: ToastSpec, cx: &mut Context<Self>) {
        // Always queue. Pushing directly from an event handler would re-enter
        // the window update that is currently dispatching the event.
        self.pending_toasts.push(spec);
        cx.notify();
    }

    /// Drains queued toasts after the current window update has finished.
    pub(crate) fn flush_pending_toasts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = std::mem::take(&mut self.pending_toasts);
        for spec in pending {
            let entity = cx.entity().downgrade();
            let note = notification_for(&spec, entity);
            window.push_notification(note, cx);
        }
    }

    pub(crate) fn notification_attention(&self) -> bool {
        self.import_report.is_some()
            || self.library_error.is_some()
            || self.failed_job_count > 0
            || !self.daemon_errors.is_empty()
    }

    pub(crate) fn open_notification_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.notification_modal_open {
            return;
        }
        if !notification_modal_allowed(self.import_dialog.is_some(), self.manual_compress.is_some())
        {
            // Actionable toasts are painted above the import dialog. Do not
            // stack a second modal over it; keep focus in the import trap.
            if self.import_dialog.is_some() {
                window.focus(&self.import_dialog_focus, cx);
            }
            return;
        }
        self.notification_return_focus = window.focused(cx);
        self.notification_modal_open = true;
        self.notification_modal_epoch = self.notification_modal_epoch.wrapping_add(1);
        self.import_report_expanded = false;
        let focus = self.notification_modal_focus.clone();
        cx.defer_in(window, move |this, window, cx| {
            if this.notification_modal_open {
                window.focus(&focus, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn close_notification_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.notification_modal_open {
            return;
        }
        self.notification_modal_open = false;
        self.import_report_expanded = false;
        let focus = self
            .notification_return_focus
            .take()
            .unwrap_or_else(|| self.photo_grid_focus.clone());
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(crate) fn notification_button(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let attention = self.notification_attention();
        div()
            .relative()
            .flex_shrink_0()
            .child(
                Button::new("titlebar-notifications")
                    .ghost()
                    .icon(IconName::Bell)
                    .tooltip("通知")
                    .size(px(26.))
                    .rounded(px(6.))
                    .text_color(rgb(p.muted_2))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_notification_modal(window, cx)),
                    ),
            )
            .when(attention, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(5.))
                        .right(px(5.))
                        .size(px(4.))
                        .rounded_full()
                        .bg(rgb(p.amber))
                        .shadow(vec![
                            BoxShadow::new(px(0.), px(0.), rgba(with_alpha(p.amber, 0xcc)).into())
                                .blur_radius(px(6.)),
                        ]),
                )
            })
            .into_any_element()
    }

    pub(crate) fn notification_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        if !self.notification_modal_open {
            return div().into_any_element();
        }
        let sections = notification_sections(
            self.import_report.is_some(),
            self.library_error.is_some(),
            self.failed_job_count,
            self.daemon_errors.len(),
        );
        let import_report = self.import_report.clone();
        let library_error = self.library_error.clone();
        let failed_jobs = self.failed_job_count;
        let daemon_errors = self.daemon_errors.len();

        let mut items = Vec::new();
        for section in sections {
            let item = match section {
                NotificationSection::Import => import_report
                    .as_ref()
                    .map(|report| self.import_notification(report, cx)),
                NotificationSection::Library => library_error
                    .as_ref()
                    .map(|error| self.library_notification(error, cx)),
                NotificationSection::Compression => {
                    Some(self.compression_notification(failed_jobs, cx))
                }
                NotificationSection::Daemon => Some(self.daemon_notification(daemon_errors, cx)),
            };
            if let Some(item) = item {
                items.push(item);
            }
        }
        if items.is_empty() {
            items.push(
                div()
                    .p(px(16.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child("対応が必要な通知はありません")
                    .into_any_element(),
            );
        }

        let max_height = (f32::from(window.viewport_size().height) * 0.8).max(240.0);
        let close = self.focusable_text_button(
            "notification-close",
            "閉じる Esc",
            self.notification_close_focus.clone(),
            ButtonState {
                tone: ButtonTone::Neutral,
                enabled: true,
            },
            cx,
            |this, window, cx| this.close_notification_modal(window, cx),
        );

        div()
            .id("notification-modal")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.))
            .bg(rgba(p.scrim))
            .key_context("notification_modal")
            .occlude()
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_notification_modal(window, cx);
            }))
            .child(
                v_flex()
                    .id("notification-modal-card")
                    .track_focus(&self.notification_modal_focus)
                    .focus_trap("notification-modal", &self.notification_modal_focus)
                    .w(px(720.))
                    .max_w(relative(1.0))
                    .max_h(px(max_height))
                    .overflow_hidden()
                    .rounded(px(20.))
                    .border_1()
                    .border_color(rgba(p.line_soft))
                    .bg(rgb(p.panel_deep))
                    .shadow(vec![
                        BoxShadow::new(px(0.), px(24.), rgba(p.shadow).into()).blur_radius(px(56.)),
                        BoxShadow::new(px(0.), px(1.), rgba(with_alpha(p.text, 0x10)).into())
                            .inset()
                            .blur_radius(px(0.)),
                    ])
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(
                        h_flex()
                            .h(px(52.))
                            .flex_shrink_0()
                            .px(px(18.))
                            .gap(px(9.))
                            .items_center()
                            .border_b_1()
                            .border_color(rgba(p.highlight))
                            .child(
                                Icon::new(IconName::Bell)
                                    .with_size(px(16.))
                                    .text_color(rgb(p.accent)),
                            )
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child("通知"),
                            )
                            .child(div().flex_1())
                            .child(close),
                    )
                    .child(
                        v_flex()
                            .id("notification-modal-body")
                            .flex_1()
                            .min_h_0()
                            .p(px(16.))
                            .gap(px(12.))
                            .overflow_y_scrollbar()
                            .children(items),
                    ),
            )
            .with_animation(
                ("notification-modal", self.notification_modal_epoch),
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(self.motion_fps()),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }

    fn import_notification(
        &mut self,
        report: &ImportReportView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let (failed, partial) = import_issue_counts(&report.issues);
        let retryable = retryable_sources(&report.issues);
        let scan_failed = report.scan_failures.len() + usize::from(report.scan_error.is_some());
        let mode = import_mode_label(report.mode);
        let dest = report.dest.to_string_lossy().to_string();
        let expanded = self.import_report_expanded;

        let mut badges = h_flex().flex_wrap().gap(px(6.));
        badges = badges
            .child(self.badge(&format!("取込 {}", report.imported), p.green, p.green_soft))
            .child(self.badge(&format!("スキップ {}", report.skipped), p.muted, p.panel_2));
        if failed > 0 {
            badges = badges.child(self.badge(&format!("失敗 {failed}"), p.red, p.red_soft));
        }
        if partial > 0 {
            badges =
                badges.child(self.badge(&format!("部分移動 {partial}"), p.amber, p.amber_soft));
        }
        if !report.renamed.is_empty() {
            badges = badges.child(self.badge(
                &format!("連番 {}", report.renamed.len()),
                p.accent,
                p.accent_soft,
            ));
        }
        if !report.remaining.is_empty() {
            badges = badges.child(self.badge(
                &format!("未処理 {}", report.remaining.len()),
                p.amber,
                p.amber_soft,
            ));
        }
        if scan_failed > 0 {
            badges =
                badges.child(self.badge(&format!("スキャン失敗 {scan_failed}"), p.red, p.red_soft));
        }

        let mut actions = h_flex().flex_wrap().gap(px(6.));
        if !retryable.is_empty() {
            actions = actions.child(
                Button::new("notification-import-retry")
                    .label("失敗分を再試行")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_notification_modal(window, cx);
                        this.open_retry_dialog(window, cx);
                    })),
            );
        }
        if !report.remaining.is_empty() {
            actions = actions.child(
                Button::new("notification-import-resume")
                    .label(format!("残り {} 件を再開", report.remaining.len()))
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_notification_modal(window, cx);
                        this.open_resume_dialog(window, cx);
                    })),
            );
        }
        actions = actions
            .child(
                Button::new("notification-import-details")
                    .label(if expanded {
                        "詳細を閉じる"
                    } else {
                        "詳細"
                    })
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_report_expanded = !this.import_report_expanded;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("notification-import-dismiss")
                    .label("結果を閉じる")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_report = None;
                        this.import_report_expanded = false;
                        cx.notify();
                    })),
            );

        v_flex()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(14.))
            .bg(rgb(p.panel))
            .border_1()
            .border_color(rgb(p.border))
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child("取り込み結果"),
                    )
                    .child(self.badge(mode, p.accent, p.accent_soft))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(dest),
                    ),
            )
            .child(badges)
            .child(actions)
            .when(expanded, |this| {
                this.child(self.import_report_details(report))
            })
            .into_any_element()
    }

    fn library_notification(&self, error: &str, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        v_flex()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(14.))
            .bg(rgb(p.panel))
            .border_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .text_size(px(12.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child("ライブラリ"),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.red))
                    .child(error.to_string()),
            )
            .child(
                Button::new("notification-library-reload")
                    .label("再読込")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload_library(true, cx);
                    })),
            )
            .into_any_element()
    }

    fn compression_notification(&self, count: i64, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        v_flex()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(14.))
            .bg(rgb(p.panel))
            .border_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .text_size(px(12.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child("圧縮"),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.amber))
                    .child(format!("{count} 件の失敗があります")),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        Button::new("notification-compression-dashboard")
                            .label("ダッシュボードを開く")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_notification_modal(window, cx);
                                this.switch_screen(Screen::Dashboard, cx);
                            })),
                    )
                    .child(
                        Button::new("notification-compression-retry")
                            .label("失敗を再試行")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.retry_all_failed_jobs(cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn daemon_notification(&self, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        v_flex()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(14.))
            .bg(rgb(p.panel))
            .border_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .text_size(px(12.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child("デーモン"),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.amber))
                    .child(format!("{count} 件の処理エラーがあります")),
            )
            .child(
                Button::new("notification-daemon-dashboard")
                    .label("ダッシュボードを開く")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_notification_modal(window, cx);
                        this.switch_screen(Screen::Dashboard, cx);
                    })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NotificationSection, notification_modal_allowed, notification_sections, toast_flush_needed,
    };

    #[test]
    fn notification_sections_follow_the_action_order() {
        assert_eq!(
            notification_sections(true, true, 2, 3),
            vec![
                NotificationSection::Import,
                NotificationSection::Library,
                NotificationSection::Compression,
                NotificationSection::Daemon,
            ]
        );
        assert!(notification_sections(false, false, 0, 0).is_empty());
    }

    #[test]
    fn toast_flush_is_scheduled_once_for_a_non_empty_queue() {
        assert!(!toast_flush_needed(0, false));
        assert!(toast_flush_needed(1, false));
        assert!(!toast_flush_needed(1, true));
    }

    #[test]
    fn notification_modal_never_stacks_over_another_modal() {
        assert!(notification_modal_allowed(false, false));
        assert!(!notification_modal_allowed(true, false));
        assert!(!notification_modal_allowed(false, true));
    }
}
