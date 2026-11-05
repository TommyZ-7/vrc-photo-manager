//! Dashboard screen: library/queue stats, daemon status, errors, and logs.
//!
//! Daemon commands live in `app::daemon`; screen dispatch lives in `ui::shell`.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photon_core::logging::{DAEMON_LOG_FILE, UI_LOG_FILE, logs_dir};
use vrc_photon_core::model::DeletionStatus;
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::ai::ai_dashboard_view;
use crate::app::daemon::{
    daemon_operation_label, daemon_state_label, daemon_sync_label, format_timestamp,
    ipc_status_label, open_path,
};
use crate::app::deletion::{
    deletion_actions, deletion_blocked_label, deletion_reason_label, deletion_status_label,
};
use crate::ui::components::glass_panel;
use crate::ui::daemon_error::daemon_error_message;
use crate::ui::shell::{scan_progress_label, scan_progress_ratio};
use crate::ui::theme::*;

impl App {
    pub(crate) fn dashboard_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let show_ai_card = ai_dashboard_view(&self.ai, self.settings.ai_tagging_enabled).visible;
        let stats = h_flex()
            .flex_shrink_0()
            .gap(px(CARD_GAP))
            .child(self.stat_card(
                tr!("dashboard.stat.photos"),
                tr_args!(
                    "dashboard.stat.photos_value",
                    count = self.stats.photo_count
                ),
            ))
            .child(self.stat_card(
                tr!("dashboard.stat.active_jobs"),
                tr_args!("dashboard.stat.jobs_value", count = self.active_job_count),
            ))
            .child(self.stat_card(
                tr!("dashboard.stat.library_size"),
                format_bytes(self.stats.total_bytes),
            ))
            .child(self.stat_card(
                tr!("dashboard.daemon.title"),
                if self.daemon_active {
                    daemon_state_label(true, self.daemon_paused).to_string()
                } else if self.settings.log_dir.is_some() {
                    tr!("dashboard.daemon.stopped")
                } else {
                    tr!("dashboard.daemon.unset")
                },
            ));
        v_flex()
            .flex_1()
            .min_h_0()
            .gap(px(CARD_GAP))
            .child(stats)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(CARD_GAP))
                    .items_stretch()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .gap(px(CARD_GAP))
                            .child(self.queue_panel(cx))
                            .child(
                                v_flex()
                                    .flex_shrink_0()
                                    .p(px(12.))
                                    .rounded(px(12.))
                                    .bg(rgb(p.amber_soft))
                                    .border_1()
                                    .border_color(rgba(with_alpha(p.amber, 0x40)))
                                    .text_size(px(11.))
                                    .text_color(rgb(p.amber))
                                    .child(tr!("dashboard.log_retention_hint")),
                            ),
                    )
                    .child(
                        v_flex()
                            .w(px(DASHBOARD_SIDE_WIDTH))
                            .flex_shrink_0()
                            .min_h_0()
                            .gap(px(CARD_GAP))
                            .overflow_y_scrollbar()
                            .child(self.daemon_card(cx))
                            .when(show_ai_card, |this| this.child(self.ai_dashboard_card(cx)))
                            .when(!self.deletion_requests.is_empty(), |this| {
                                this.child(self.deletion_card(cx))
                            })
                            .when(!self.daemon_errors.is_empty(), |this| {
                                this.child(self.daemon_error_card(cx))
                            })
                            .child(self.session_card())
                            .child(self.monitor_paths_card())
                            .child(self.log_card(cx)),
                    ),
            )
    }

    fn session_card(&self) -> impl IntoElement {
        let p = self.palette;
        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_shrink_0()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(17.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child(tr!("dashboard.session.title")),
            )
            .child(
                self.meta_row(
                    tr!("dashboard.meta.world"),
                    self.monitor_world
                        .clone()
                        .unwrap_or_else(|| "—".to_string()),
                ),
            )
            .child(
                self.meta_row(
                    tr!("dashboard.meta.instance"),
                    self.monitor_instance
                        .clone()
                        .unwrap_or_else(|| "—".to_string()),
                ),
            )
            .child(self.meta_row(
                tr!("dashboard.meta.players"),
                tr_args!(
                    "dashboard.players.count",
                    count = self.monitor_players.len()
                ),
            ))
            .child(self.section_label(if self.monitor_players.is_empty() {
                tr!("dashboard.players.empty")
            } else {
                tr!("dashboard.players.title")
            }))
            .children(self.monitor_players.iter().map(|player| {
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(player.clone())
            }))
    }

    fn monitor_paths_card(&self) -> impl IntoElement {
        let p = self.palette;
        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_shrink_0()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(17.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child(tr!("dashboard.monitor.path")),
            )
            .children(self.settings.photo_roots.iter().map(|path| {
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(10.))
                    .font_family("monospace")
                    .text_color(rgb(p.muted))
                    .child(path.clone())
            }))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(10.))
                    .font_family("monospace")
                    .text_color(rgb(p.muted))
                    .child(
                        self.settings
                            .log_dir
                            .clone()
                            .unwrap_or_else(|| tr!("dashboard.log_dir.unset")),
                    ),
            )
    }

    pub(crate) fn deletion_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let requests = self.deletion_requests.clone();
        let counts = self.deletion_counts;
        let mut card = glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_shrink_0()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child(tr!("dashboard.deletion.title")),
                    )
                    .when(counts.pending > 0, |this| {
                        this.child(self.badge(
                            tr_args!("dashboard.deletion.pending", count = counts.pending),
                            p.amber,
                            p.amber_soft,
                        ))
                    })
                    .when(counts.blocked > 0, |this| {
                        this.child(self.badge(
                            tr_args!("dashboard.deletion.blocked", count = counts.blocked),
                            p.red,
                            p.red_soft,
                        ))
                    })
                    .when(counts.running > 0, |this| {
                        this.child(self.badge(
                            tr_args!("dashboard.deletion.running", count = counts.running),
                            p.accent,
                            p.accent_soft,
                        ))
                    }),
            );

        for request in requests.iter().take(20) {
            let id = request.id;
            let status = deletion_status_label(request.status);
            let (status_color, status_bg) = match request.status {
                DeletionStatus::Blocked => (p.red, p.red_soft),
                DeletionStatus::Running => (p.accent, p.accent_soft),
                _ => (p.amber, p.amber_soft),
            };
            let actions = deletion_actions(request);
            let blocked = deletion_blocked_label(request);
            let row = v_flex()
                .gap(px(6.))
                .p(px(10.))
                .rounded(px(10.))
                .bg(rgb(p.panel_2))
                .border_1()
                .border_color(rgb(p.border))
                .child(
                    h_flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(p.text))
                                .child(request.photo_name.clone()),
                        )
                        .child(self.badge(status, status_color, status_bg)),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted))
                                .child(deletion_reason_label(request.reason)),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(format_timestamp(&request.requested_at)),
                        ),
                )
                .when_some(blocked, |this, detail| {
                    this.child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.red))
                            .child(detail),
                    )
                })
                .child(
                    h_flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(6.))
                        .when(actions.can_cancel, |this| {
                            this.child(
                                Button::new(SharedString::from(format!("cancel-deletion-{id}")))
                                    .label(tr!("dashboard.deletion.cancel"))
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.cancel_deletion(id, cx);
                                    })),
                            )
                        })
                        .when(actions.retryable, |this| {
                            this.child(
                                Button::new(SharedString::from(format!("retry-deletion-{id}")))
                                    .label(tr!("common.retry"))
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_deletion(id, cx);
                                    })),
                            )
                        })
                        .when(actions.forceable, |this| {
                            this.child(
                                Button::new(SharedString::from(format!("force-deletion-{id}")))
                                    .label(tr!("dashboard.deletion.force"))
                                    .small()
                                    .danger()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.request_force_deletion(id, window, cx);
                                    })),
                            )
                        }),
                );
            card = card.child(row);
        }

        card.child(
            div()
                .text_size(px(10.))
                .text_color(rgb(p.muted_2))
                .child(tr!("dashboard.deletion.hint")),
        )
    }

    pub(crate) fn daemon_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let status = if !self.daemon_active {
            (tr!("dashboard.daemon.state.stopped"), p.muted, p.panel_2)
        } else if self.daemon_paused {
            (tr!("dashboard.daemon.state.paused"), p.amber, p.amber_soft)
        } else {
            (tr!("dashboard.daemon.state.running"), p.green, p.green_soft)
        };
        let sync = daemon_sync_label(
            self.daemon_age_secs,
            self.daemon_supervisor.as_deref(),
            self.ipc_connected,
        );
        let ipc = if self.ipc_connected {
            tr!("dashboard.ipc.connected")
        } else {
            tr!("dashboard.ipc.disconnected")
        };
        let ipc_detail = ipc_status_label(self.ipc_connected, self.ipc_error.as_deref());
        let ipc_color = if self.ipc_connected { p.green } else { p.red };
        let ipc_bg = if self.ipc_connected {
            p.green_soft
        } else {
            p.red_soft
        };
        let scan = self.scan_progress;
        let armed = self.daemon_shutdown_armed;
        let pause_label = if self.daemon_paused {
            tr!("dashboard.daemon.resume")
        } else {
            tr!("dashboard.daemon.pause")
        };

        let mut card = glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_shrink_0()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child(tr!("dashboard.daemon.title")),
                    )
                    .child(self.badge(status.0, status.1, status.2))
                    .child(self.badge(ipc, ipc_color, ipc_bg)),
            )
            .child(self.meta_row(tr!("dashboard.daemon.last_sync"), sync))
            .child(self.meta_row("IPC", ipc_detail))
            .child(self.meta_row(
                tr!("dashboard.daemon.watching"),
                daemon_state_label(self.daemon_active, self.daemon_paused).to_string(),
            ));

        if let Some(progress) = scan {
            card = card.child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(scan_progress_label(&progress)),
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
                                    .w(relative(scan_progress_ratio(&progress)))
                                    .rounded_full()
                                    .bg(rgb(p.accent)),
                            ),
                    ),
            );
        }

        card.child(
            h_flex()
                .flex_wrap()
                .gap(px(8.))
                .child(
                    Button::new("daemon-rescan")
                        .label(tr!("dashboard.daemon.rescan"))
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.request_daemon_rescan(cx))),
                )
                .child(
                    Button::new("daemon-pause")
                        .label(pause_label)
                        .small()
                        .disabled(!self.ipc_connected)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_daemon_pause(cx))),
                )
                .child(
                    Button::new("daemon-shutdown")
                        .label(if armed {
                            tr!("dashboard.daemon.quit_armed")
                        } else {
                            tr!("dashboard.daemon.quit")
                        })
                        .small()
                        .disabled(!self.ipc_connected)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_daemon_shutdown(cx))),
                ),
        )
    }

    pub(crate) fn daemon_error_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let mut card = glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_shrink_0()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child(tr!("dashboard.errors.title")),
                    )
                    .child(self.badge(
                        tr_args!("dashboard.errors.count", count = self.daemon_errors.len()),
                        p.red,
                        p.red_soft,
                    )),
            );
        for error in &self.daemon_errors {
            let command = error.operation.retry_command();
            card =
                card.child(
                    v_flex()
                        .gap(px(8.))
                        .p(px(12.))
                        .rounded(px(10.))
                        .bg(rgb(p.panel_2))
                        .border_1()
                        .border_color(rgb(p.border))
                        .child(
                            h_flex()
                                .items_center()
                                .gap(px(8.))
                                .child(self.badge(
                                    daemon_operation_label(error.operation),
                                    p.red,
                                    p.red_soft,
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(12.))
                                        .text_color(rgb(p.red))
                                        .child(daemon_error_message(error)),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "retry-daemon-{}",
                                        error.operation.as_str()
                                    )))
                                    .label(tr!("common.retry"))
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.request_daemon_command(command.clone(), cx)
                                    })),
                                ),
                        )
                        .when_some(error.detail.clone(), |this, detail| {
                            this.child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .p(px(8.))
                                    .rounded(px(8.))
                                    .bg(rgb(p.bg))
                                    .font_family("monospace")
                                    .whitespace_normal()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted))
                                    .child(detail),
                            )
                        })
                        .child(
                            h_flex()
                                .items_center()
                                .gap(px(12.))
                                .child(div().text_size(px(10.)).text_color(rgb(p.muted_2)).child(
                                    tr_args!("dashboard.errors.attempts", count = error.attempts),
                                ))
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(p.muted_2))
                                        .child(format_timestamp(&error.occurred_at)),
                                ),
                        ),
                );
        }
        card.when(!self.daemon_active, |this| {
            this.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("dashboard.errors.daemon_stopped")),
            )
        })
    }

    pub(crate) fn log_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let dir = logs_dir();
        let mut card = glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_shrink_0()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(17.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child(tr!("dashboard.log.title")),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(self.section_label(tr!("dashboard.log.location")))
                    .child(match &dir {
                        Some(dir) => div()
                            .min_w_0()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(dir.display().to_string()),
                        None => div()
                            .text_size(px(10.))
                            .text_color(rgb(p.red))
                            .child(tr!("dashboard.log.location_missing")),
                    }),
            )
            .child(
                v_flex()
                    .gap(px(4.))
                    .child(self.section_label(tr!("dashboard.log.file")))
                    .child(
                        div()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(format!("UI: {UI_LOG_FILE}")),
                    )
                    .child(
                        div()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr_args!(
                                "dashboard.log.daemon_file",
                                file = DAEMON_LOG_FILE
                            )),
                    ),
            );
        if dir.is_some() {
            card = card.child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        Button::new("open-logs-folder")
                            .label(tr!("dashboard.log.open_folder"))
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.open_logs_folder(cx))),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("dashboard.log.open_hint")),
                    ),
            );
        } else {
            card = card.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("dashboard.log.unavailable")),
            );
        }
        card
    }

    pub(crate) fn open_logs_folder(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = logs_dir() else {
            self.toast_error(tr!("dashboard.log.location_missing"), cx);
            cx.notify();
            return;
        };
        let result = std::fs::create_dir_all(&dir)
            .map_err(|error| error.to_string())
            .and_then(|()| open_path(&dir));
        match result {
            Ok(()) => {
                self.toast_success(tr_args!("dashboard.log.opened", path = dir.display()), cx)
            }
            Err(error) => self.toast_error(tr_args!("dashboard.log.open_error", error = error), cx),
        }
        cx.notify();
    }
}
