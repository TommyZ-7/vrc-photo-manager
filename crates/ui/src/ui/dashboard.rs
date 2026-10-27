//! Dashboard screen: library/queue stats, daemon status, errors, and logs.
//!
//! Daemon commands live in `app::daemon`; screen dispatch lives in `ui::shell`.

use gpui_kit::component::button::Button;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::logging::{DAEMON_LOG_FILE, UI_LOG_FILE, logs_dir};

use crate::app::App;
use crate::app::daemon::{
    daemon_operation_label, daemon_state_label, daemon_sync_label, format_timestamp,
    ipc_status_label, open_path,
};
use crate::ui::components::glass_panel;
use crate::ui::shell::{scan_progress_label, scan_progress_ratio};
use crate::ui::theme::*;

impl App {
    pub(crate) fn dashboard_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .px(px(26.))
                    .pt(px(22.))
                    .pb(px(26.))
                    .gap(px(24.))
                    .child(
                        h_flex()
                            .gap(px(16.))
                            .child(self.stat_card(
                                "写真",
                                format!("{} 枚", self.stats.photo_count),
                            ))
                            .child(self.stat_card(
                                "未完了ジョブ",
                                format!("{} 件", self.active_job_count),
                            ))
                            .child(self.stat_card(
                                "未実行",
                                format!("{} 件", self.waiting_job_count),
                            ))
                            .child(self.stat_card(
                                "デーモン",
                                if self.daemon_active {
                                    daemon_state_label(true, self.daemon_paused).to_string()
                                } else if self.settings.log_dir.is_some() {
                                    "停止中".to_string()
                                } else {
                                    "未設定".to_string()
                                },
                            )),
                    )
                    .child(self.queue_panel(cx))
                    .child(self.daemon_card(cx))
                    .when(!self.daemon_errors.is_empty(), |this| {
                        this.child(self.daemon_error_card(cx))
                    })
                    .child(
                        h_flex()
                            .gap(px(24.))
                            .items_start()
                            .child(
                                glass_panel()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .p(px(16.))
                                    .gap(px(10.))
                                    .bg(rgb(PANEL))
                                    .child(
                                        div()
                                            .text_size(px(17.))
                                            .font_semibold()
                                            .text_color(rgb(TEXT))
                                            .child("現在のセッション"),
                                    )
                                    .child(self.meta_row(
                                        "ワールド",
                                        self.monitor_world
                                            .clone()
                                            .unwrap_or_else(|| "—".to_string()),
                                    ))
                                    .child(self.meta_row(
                                        "インスタンス",
                                        self.monitor_instance
                                            .clone()
                                            .unwrap_or_else(|| "—".to_string()),
                                    ))
                                    .child(self.meta_row(
                                        "同接",
                                        format!("{} 人", self.monitor_players.len()),
                                    ))
                                    .child(self.section_label(if self.monitor_players.is_empty() {
                                        "プレイヤー情報はまだありません"
                                    } else {
                                        "入室中のプレイヤー"
                                    }))
                                    .children(self.monitor_players.iter().map(|player| {
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(MUTED))
                                            .child(player.clone())
                                    })),
                            )
                            .child(
                                glass_panel()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .p(px(16.))
                                    .gap(px(10.))
                                    .bg(rgb(PANEL))
                                    .child(
                                        div()
                                            .text_size(px(17.))
                                            .font_semibold()
                                            .text_color(rgb(TEXT))
                                            .child("監視パス"),
                                    )
                                    .children(self.settings.photo_roots.iter().map(|path| {
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(10.))
                                            .font_family("monospace")
                                            .text_color(rgb(MUTED))
                                            .child(path.clone())
                                    }))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(10.))
                                            .font_family("monospace")
                                            .text_color(rgb(MUTED))
                                            .child(
                                                self.settings
                                                    .log_dir
                                                    .clone()
                                                    .unwrap_or_else(|| "ログフォルダ未設定".to_string()),
                                            ),
                                    ),
                            ),
                    )
                    .child(self.log_card(cx))
                    .child(
                        div()
                            .p(px(14.))
                            .rounded(px(14.))
                            .bg(rgb(0x2a2412))
                            .border_1()
                            .border_color(rgba(0xfbbf2440))
                            .text_size(px(12.))
                            .text_color(rgb(AMBER))
                            .child("VRChat本体は出力ログを24時間で削除します。アプリ側には解析済みイベントだけを保存します。"),
                    ),
            )
    }

    pub(crate) fn daemon_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if !self.daemon_active {
            ("停止中", MUTED, PANEL_2)
        } else if self.daemon_paused {
            ("一時停止中", AMBER, 0x2a2412)
        } else {
            ("稼働中", GREEN, 0x112a22)
        };
        let sync = daemon_sync_label(
            self.daemon_age_secs,
            self.daemon_supervisor.as_deref(),
            self.ipc_connected,
        );
        let ipc = if self.ipc_connected {
            "IPC 接続中"
        } else {
            "IPC 未接続"
        };
        let ipc_detail = ipc_status_label(self.ipc_connected, self.ipc_error.as_deref());
        let ipc_color = if self.ipc_connected { GREEN } else { RED };
        let ipc_bg = if self.ipc_connected {
            0x112a22
        } else {
            0x2a1414
        };
        let scan = self.scan_progress;
        let armed = self.daemon_shutdown_armed;
        let pause_label = if self.daemon_paused {
            "監視を再開"
        } else {
            "監視を一時停止"
        };

        let mut card = glass_panel()
            .flex()
            .flex_col()
            .p(px(16.))
            .gap(px(12.))
            .bg(rgb(PANEL))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("デーモン"),
                    )
                    .child(self.badge(status.0, status.1, status.2))
                    .child(self.badge(ipc, ipc_color, ipc_bg)),
            )
            .child(self.meta_row("最終同期", sync))
            .child(self.meta_row("IPC", ipc_detail))
            .child(self.meta_row(
                "監視",
                daemon_state_label(self.daemon_active, self.daemon_paused).to_string(),
            ));

        if let Some(progress) = scan {
            card = card.child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child(scan_progress_label(&progress)),
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
                                    .w(relative(scan_progress_ratio(&progress)))
                                    .rounded_full()
                                    .bg(rgb(BLUE)),
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
                        .label("再スキャン")
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
                            "もう一度押して終了"
                        } else {
                            "デーモンを終了"
                        })
                        .small()
                        .disabled(!self.ipc_connected)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_daemon_shutdown(cx))),
                ),
        )
    }

    pub(crate) fn daemon_error_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut card = glass_panel()
            .flex()
            .flex_col()
            .p(px(16.))
            .gap(px(10.))
            .bg(rgb(PANEL))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("処理エラー"),
                    )
                    .child(self.badge(&format!("{} 件", self.daemon_errors.len()), RED, 0x2a1414)),
            );
        for error in &self.daemon_errors {
            let command = error.operation.retry_command();
            card = card.child(
                v_flex()
                    .gap(px(8.))
                    .p(px(12.))
                    .rounded(px(10.))
                    .bg(rgb(PANEL_2))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(self.badge(
                                daemon_operation_label(error.operation),
                                RED,
                                0x2a1414,
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(RED))
                                    .child(error.message.clone()),
                            )
                            .child(
                                Button::new(SharedString::from(format!(
                                    "retry-daemon-{}",
                                    error.operation.as_str()
                                )))
                                .label("再試行")
                                .small()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.request_daemon_command(command, cx),
                                )),
                            ),
                    )
                    .when_some(error.detail.clone(), |this, detail| {
                        this.child(
                            div()
                                .w_full()
                                .min_w_0()
                                .p(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(BG))
                                .font_family("monospace")
                                .whitespace_normal()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(detail),
                        )
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format!("検出回数 {}", error.attempts)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format_timestamp(&error.occurred_at)),
                            ),
                    ),
            );
        }
        card.when(!self.daemon_active, |this| {
            this.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("デーモン停止中です。起動後に実行されます。"),
            )
        })
    }

    pub(crate) fn log_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dir = logs_dir();
        let mut card = glass_panel()
            .flex()
            .flex_col()
            .p(px(24.))
            .gap(px(16.))
            .bg(rgb(PANEL))
            .child(
                div()
                    .text_size(px(17.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child("ログ"),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(self.section_label("保存先"))
                    .child(match &dir {
                        Some(dir) => div()
                            .min_w_0()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child(dir.display().to_string()),
                        None => div()
                            .text_size(px(10.))
                            .text_color(rgb(RED))
                            .child("ログフォルダを特定できません"),
                    }),
            )
            .child(
                v_flex()
                    .gap(px(4.))
                    .child(self.section_label("ファイル"))
                    .child(
                        div()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(format!("UI: {UI_LOG_FILE}")),
                    )
                    .child(
                        div()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(format!("デーモン: {DAEMON_LOG_FILE}")),
                    ),
            );
        if dir.is_some() {
            card = card.child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        Button::new("open-logs-folder")
                            .label("ログフォルダを開く")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.open_logs_folder(cx))),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("OSのファイラーで開きます"),
                    ),
            );
        } else {
            card = card.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("ファイルへのログ出力は利用できません。"),
            );
        }
        card
    }

    pub(crate) fn open_logs_folder(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = logs_dir() else {
            self.message = Some("ログフォルダを特定できません".to_string());
            cx.notify();
            return;
        };
        let result = std::fs::create_dir_all(&dir)
            .map_err(|error| error.to_string())
            .and_then(|()| open_path(&dir));
        self.message = Some(match result {
            Ok(()) => format!("ログフォルダを開きました: {}", dir.display()),
            Err(error) => format!("ログフォルダを開けません: {error}"),
        });
        cx.notify();
    }
}
