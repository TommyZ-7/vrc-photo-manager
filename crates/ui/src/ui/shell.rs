//! App shell: sidebar navigation, global alerts, and the screen dispatch
//! wrapper.
//!
//! `App::render` stays in `app.rs`; this module owns the chrome around the
//! active screen and the grid helpers shared by the screens.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::importer::ImportProgress;
use vrc_photo_ipc::ScanPhase as IpcScanPhase;

use crate::app::import::{
    ImportDialogPhase, ImportDialogState, ImportReportView, import_issue_counts,
};
use crate::app::lifecycle::ScanProgressView;
use crate::app::{App, Screen, SidebarStatusTab};
use crate::ui::components::status_progress_bar;
use crate::ui::import::import_progress_ratio;
use crate::ui::theme::*;

const NAV_ITEM_HEIGHT: f32 = 40.0;
const NAV_ITEM_GAP: f32 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusTone {
    Idle,
    Running,
    Waiting,
    Failed,
    Success,
}

impl StatusTone {
    fn color(self) -> u32 {
        match self {
            Self::Idle => MUTED_2,
            Self::Running => BLUE,
            Self::Waiting => AMBER,
            Self::Failed => RED,
            Self::Success => GREEN,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CardStatus {
    headline: String,
    detail: String,
    tone: StatusTone,
}

impl App {
    pub(crate) fn sidebar(&mut self, content_height: f32, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let update_badge = self.update.nav_badge();
        let epoch = self.sidebar_motion_epoch;
        let reveal_duration = if epoch == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(320)
        };
        let photo_count = format_count(self.stats.photo_count);
        let compression_count =
            (self.active_job_count > 0).then(|| format_count(self.active_job_count));
        let status_card = (!collapsed && sidebar_status_visible(content_height))
            .then(|| self.sidebar_status_card(cx));

        let brand_text = v_flex()
            .min_w_0()
            .gap(px(0.))
            .overflow_hidden()
            .whitespace_nowrap()
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child("VRC Photo"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child(format!("Manager · {photo_count}枚")),
            )
            .with_animation(
                SharedString::from(format!("sidebar-brand-{epoch}")),
                Animation::new(reveal_duration)
                    .with_easing(ease_motion)
                    .with_max_fps(60.0),
                move |el, t| {
                    el.w(px(sidebar_reveal_width(collapsed, t)))
                        .opacity(if collapsed { 1.0 - t } else { t })
                },
            );

        let body = v_flex()
            .relative()
            .flex_shrink_0()
            .h_full()
            .py(px(16.))
            .px(px(12.))
            .gap(px(3.))
            .bg(rgb(PANEL_SIDEBAR))
            .rounded(px(22.))
            .border_1()
            .border_color(rgba(0xffffff1b))
            .overflow_hidden()
            .shadow(vec![
                BoxShadow::new(px(0.), px(18.), rgba(0x00000047).into()).blur_radius(px(44.)),
                BoxShadow::new(px(0.), px(1.), rgba(0xffffff10).into())
                    .inset()
                    .blur_radius(px(0.)),
            ])
            .child(
                h_flex()
                    .h(px(34.))
                    .px(px(2.))
                    .gap(px(10.))
                    .items_center()
                    .when(collapsed, |this| this.justify_center().gap(px(0.)))
                    .child(
                        div()
                            .size(px(34.))
                            .rounded(px(10.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(linear_gradient(
                                135.,
                                linear_color_stop(rgb(BLUE), 0.),
                                linear_color_stop(rgb(PURPLE), 1.),
                            ))
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(rgb(0xffffff))
                            .child("VP"),
                    )
                    .child(brand_text),
            )
            .child(div().h(px(12.)))
            .child(
                v_flex()
                    .relative()
                    .w_full()
                    .gap(px(NAV_ITEM_GAP))
                    .child(self.nav_highlight())
                    .child(self.nav_item(
                        "写真",
                        Screen::Photos,
                        IconName::GalleryVerticalEnd,
                        Some(photo_count.clone()),
                        cx,
                    ))
                    .child(self.nav_item("検索", Screen::Search, IconName::Search, None, cx))
                    .child(self.nav_item(
                        "圧縮",
                        Screen::Compression,
                        IconName::Minimize,
                        compression_count,
                        cx,
                    ))
                    .child(self.nav_item(
                        "ダッシュボード",
                        Screen::Dashboard,
                        IconName::LayoutDashboard,
                        None,
                        cx,
                    ))
                    .child(self.nav_item(
                        "設定",
                        Screen::Settings,
                        IconName::Settings,
                        update_badge,
                        cx,
                    )),
            )
            .child(div().flex_1())
            .when_some(status_card, |this, card| this.child(card))
            .when(!collapsed, |this| {
                this.child(
                    v_flex()
                        .p(px(11.))
                        .gap(px(3.))
                        .rounded(px(13.))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgba(0xffffff0f))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED_2))
                                .child("ライブラリ"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_semibold()
                                .text_color(rgb(TEXT))
                                .child(format!("{photo_count} 枚")),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(format_bytes(self.stats.total_bytes)),
                        ),
                )
            })
            .child(
                h_flex()
                    .h(px(26.))
                    .gap(px(7.))
                    .items_center()
                    .when(collapsed, |this| this.justify_center())
                    .when(!collapsed, |this| this.px(px(9.)))
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(rgb(if !self.daemon_active {
                                MUTED_2
                            } else if self.daemon_paused {
                                AMBER
                            } else {
                                GREEN
                            })),
                    )
                    .when(!collapsed, |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(if self.daemon_active { MUTED } else { MUTED_2 }))
                                .child(if !self.daemon_active {
                                    "デーモン停止"
                                } else if self.daemon_paused {
                                    "一時停止中"
                                } else {
                                    "監視中"
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(if collapsed { 9. } else { 10. }))
                    .text_color(rgb(MUTED_2))
                    .child(if self.demo_assets.is_some() {
                        "DEMO DATA".to_string()
                    } else {
                        format!("v{}", env!("CARGO_PKG_VERSION"))
                    }),
            );

        body.with_animation(
            ("sidebar", epoch),
            Animation::new(if epoch == 0 {
                Duration::ZERO
            } else {
                Duration::from_millis(440)
            })
            .with_easing(ease_motion)
            .with_max_fps(60.0),
            move |el, t| {
                let from = sidebar_width(!collapsed);
                let to = sidebar_width(collapsed);
                el.w(px(from + (to - from) * t))
            },
        )
        .into_any_element()
    }

    fn sidebar_status_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let import_progress = import_running_progress(self.import_dialog.as_ref());
        let compression = compression_card_status(
            self.active_job_count,
            self.waiting_job_count,
            self.failed_job_count,
        );
        let import = import_card_status(import_progress, self.import_report.as_ref());
        let scan = scan_card_status(
            self.scan_progress.as_ref(),
            self.daemon_active,
            self.daemon_paused,
        );
        let compression_attention = compression_attention(
            self.active_job_count,
            self.waiting_job_count,
            self.failed_job_count,
        );
        let import_attention = import_attention(import_progress, self.import_report.as_ref());
        let scan_attention = scan_attention(
            self.scan_progress.as_ref(),
            self.daemon_active,
            self.daemon_paused,
        );
        let tab = self.sidebar_status_tab;
        let (status, ratio) = match tab {
            SidebarStatusTab::Compression => (&compression, None),
            SidebarStatusTab::Import => (&import, import_progress.map(import_progress_ratio)),
            SidebarStatusTab::Scan => (
                &scan,
                self.scan_progress
                    .as_ref()
                    .map(scan_progress_ratio)
                    .filter(|ratio| *ratio > 0.0),
            ),
        };
        let import_has_report = self.import_report.is_some();

        let tabs = [
            (
                SidebarStatusTab::Compression,
                "圧縮",
                "sidebar-status-tab-compression",
                compression_attention,
            ),
            (
                SidebarStatusTab::Import,
                "取り込み",
                "sidebar-status-tab-import",
                import_attention,
            ),
            (
                SidebarStatusTab::Scan,
                "スキャン",
                "sidebar-status-tab-scan",
                scan_attention,
            ),
        ];
        let tab_row =
            h_flex()
                .gap(px(4.))
                .children(tabs.into_iter().map(|(tab, label, id, attention)| {
                    let active = self.sidebar_status_tab == tab;
                    h_flex()
                        .id(id)
                        .relative()
                        .flex_1()
                        .h(px(20.))
                        .justify_center()
                        .items_center()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(if active {
                            rgba(0x66b3ff33)
                        } else {
                            rgba(0x00000000)
                        })
                        .text_size(px(10.))
                        .font_medium()
                        .cursor_pointer()
                        .when(active, |this| {
                            this.bg(rgb(ACCENT_SOFT)).text_color(rgb(BLUE))
                        })
                        .when(!active, |this| {
                            this.text_color(rgb(MUTED_2))
                                .hover(|this| this.bg(rgb(PANEL_2)).text_color(rgb(TEXT)))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.sidebar_status_tab = tab;
                            cx.notify();
                        }))
                        .when_some(attention, |this, tone| {
                            this.child(
                                div()
                                    .absolute()
                                    .top(px(3.))
                                    .right(px(4.))
                                    .size(px(4.))
                                    .rounded_full()
                                    .bg(rgb(tone.color())),
                            )
                        })
                        .child(label)
                }));

        let mut body = v_flex()
            .h(px(46.))
            .justify_center()
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(status.tone.color()))
                    .child(status.headline.clone()),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .truncate()
                    .child(status.detail.clone()),
            );
        if let Some(ratio) = ratio {
            body = body.child(status_progress_bar(ratio));
        }

        let footer =
            match tab {
                SidebarStatusTab::Compression => {
                    let mut row = h_flex().items_center().h(px(24.)).gap(px(6.));
                    if self.waiting_job_count > 0 {
                        row = row.child(
                            Button::new("sidebar-status-release-waiting")
                                .label("未実行を開始")
                                .small()
                                .primary()
                                .tooltip("未実行のジョブをすべて圧縮します")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.request_release_waiting_jobs(window, cx)
                                })),
                        );
                    }
                    if self.failed_job_count > 0 {
                        row = row.child(
                            Button::new("sidebar-status-retry-failed")
                                .label("再試行")
                                .small()
                                .ghost()
                                .tooltip("上限内の失敗ジョブを再試行します")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.retry_all_failed_jobs(cx)),
                                ),
                        );
                    }
                    if self.waiting_job_count == 0 && self.failed_job_count == 0 {
                        row = row.child(
                            Button::new("sidebar-status-open-queue")
                                .label("キューを開く")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.switch_screen(Screen::Dashboard, cx)
                                })),
                        );
                    }
                    row
                }
                SidebarStatusTab::Import => {
                    h_flex()
                        .items_center()
                        .h(px(24.))
                        .gap(px(6.))
                        .child(if import_has_report {
                            Button::new("sidebar-status-import-result")
                                .label("結果を見る")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.switch_screen(Screen::Photos, cx)
                                }))
                        } else {
                            Button::new("sidebar-status-photos")
                                .label("写真を開く")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.switch_screen(Screen::Photos, cx)
                                }))
                        })
                }
                SidebarStatusTab::Scan => {
                    let can_rescan =
                        self.scan_progress.is_none() && self.daemon_active && !self.daemon_paused;
                    let button = if can_rescan {
                        Button::new("sidebar-status-rescan")
                            .label("再スキャン")
                            .small()
                            .ghost()
                            .tooltip("デーモンに再スキャンを要求します")
                            .on_click(cx.listener(|this, _, _, cx| this.request_daemon_rescan(cx)))
                    } else {
                        Button::new("sidebar-status-dashboard")
                            .label("ダッシュボード")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.switch_screen(Screen::Dashboard, cx)
                            }))
                    };
                    h_flex().items_center().h(px(24.)).gap(px(6.)).child(button)
                }
            };

        v_flex()
            .flex_shrink_0()
            .p(px(9.))
            .gap(px(4.))
            .rounded(px(13.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgba(0xffffff0f))
            .child(
                h_flex()
                    .items_center()
                    .h(px(12.))
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .font_semibold()
                            .text_color(rgb(MUTED_2))
                            .child("進行状況"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(rgb(status.tone.color())),
                    ),
            )
            .child(tab_row)
            .child(body)
            .child(footer)
            .into_any_element()
    }

    fn nav_highlight(&self) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let nav_epoch = self.nav_epoch;
        let from = nav_item_top(nav_index(self.nav_previous));
        let to = nav_item_top(nav_index(self.screen));
        let vertical_duration = if nav_epoch == 0 {
            Duration::ZERO
        } else {
            MOTION_BASE
        };
        let vertical_anim = || {
            Animation::new(vertical_duration)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS)
        };

        let pill = div()
            .absolute()
            .left_0()
            .right_0()
            .h(px(NAV_ITEM_HEIGHT))
            .rounded(px(11.))
            .bg(rgb(ACCENT_SOFT))
            .with_animation(("nav-pill", nav_epoch), vertical_anim(), move |el, t| {
                el.top(px(from + (to - from) * t))
            });

        let sidebar_epoch = self.sidebar_motion_epoch;
        let from_left = nav_bar_left(!collapsed);
        let to_left = nav_bar_left(collapsed);
        let horizontal_anim = Animation::new(if sidebar_epoch == 0 {
            Duration::ZERO
        } else {
            MOTION_SLOW
        })
        .with_easing(ease_motion)
        .with_max_fps(MOTION_FPS);
        let bar = div()
            .absolute()
            .left_0()
            .top_0()
            .w(px(2.))
            .h_full()
            .with_animation(
                ("nav-bar-x", sidebar_epoch),
                horizontal_anim,
                move |el, t| el.left(px(from_left + (to_left - from_left) * t)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(from + 11.0))
                    .left_0()
                    .w(px(2.))
                    .h(px(18.))
                    .rounded_full()
                    .bg(rgb(BLUE))
                    .with_animation(("nav-bar-y", nav_epoch), vertical_anim(), move |el, t| {
                        el.top(px(from + 11.0 + (to - from) * t))
                    }),
            );

        div()
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h(px(5.0 * NAV_ITEM_HEIGHT + 4.0 * NAV_ITEM_GAP))
            .child(pill)
            .child(bar)
            .into_any_element()
    }

    fn nav_item(
        &mut self,
        label: &'static str,
        screen: Screen,
        icon: IconName,
        count: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.screen == screen;
        let was_active = self.nav_previous == screen;
        let hovered = self.hovered_nav == Some(screen);
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let nav_epoch = self.nav_epoch;
        let reveal_duration = if epoch == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(320)
        };
        let color_anim = || {
            Animation::new(MOTION_FAST)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS)
        };

        let icon_element = Icon::new(icon).with_size(px(17.)).with_animation(
            SharedString::from(format!("nav-icon-{label}-{nav_epoch}")),
            color_anim(),
            move |el, t| {
                if hovered && !active {
                    return el.text_color(rgb(TEXT));
                }
                let from = if was_active { BLUE } else { MUTED };
                let to = if active { BLUE } else { MUTED };
                el.text_color(rgb(lerp_color(from, to, t)))
            },
        );

        let label_from = if was_active { 0xeaf4ff } else { MUTED };
        let label_to = if active {
            0xeaf4ff
        } else if hovered {
            TEXT
        } else {
            MUTED
        };
        let label_block = h_flex()
            .min_w_0()
            .items_center()
            .justify_between()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(div().text_size(px(12.)).child(label).with_animation(
                SharedString::from(format!("nav-label-{label}-{nav_epoch}")),
                color_anim(),
                move |el, t| el.text_color(rgb(lerp_color(label_from, label_to, t))),
            ))
            .when_some(count, |this, count| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .font_family("monospace")
                        .child(count)
                        .with_animation(
                            SharedString::from(format!("nav-count-{label}-{nav_epoch}")),
                            color_anim(),
                            move |el, t| {
                                if hovered && !active {
                                    return el.text_color(rgb(TEXT));
                                }
                                let from = if was_active { BLUE } else { MUTED_2 };
                                let to = if active { BLUE } else { MUTED_2 };
                                el.text_color(rgb(lerp_color(from, to, t)))
                            },
                        ),
                )
            })
            .with_animation(
                SharedString::from(format!("sidebar-label-{label}-{epoch}")),
                Animation::new(reveal_duration)
                    .with_easing(ease_motion)
                    .with_max_fps(60.0),
                move |el, t| {
                    el.w(px(sidebar_reveal_width(collapsed, t)))
                        .opacity(if collapsed { 1.0 - t } else { t })
                },
            );

        let from_pad = if collapsed { 11.0 } else { 18.5 };
        let to_pad = if collapsed { 18.5 } else { 11.0 };
        let from_gap = if collapsed { 11.0 } else { 0.0 };
        let to_gap = if collapsed { 0.0 } else { 11.0 };
        let pad_anim = Animation::new(if epoch == 0 {
            Duration::ZERO
        } else {
            MOTION_SLOW
        })
        .with_easing(ease_motion)
        .with_max_fps(MOTION_FPS);
        let content = h_flex()
            .w_full()
            .items_center()
            .with_animation(
                SharedString::from(format!("nav-pad-{label}-{epoch}")),
                pad_anim,
                move |el, t| {
                    el.pl(px(from_pad + (to_pad - from_pad) * t))
                        .gap(px(from_gap + (to_gap - from_gap) * t))
                },
            )
            .child(icon_element)
            .child(label_block);

        h_flex()
            .id(label)
            .relative()
            .h(px(NAV_ITEM_HEIGHT))
            .w_full()
            .rounded(px(11.))
            .cursor_pointer()
            .items_center()
            .when(!active, |this| {
                this.hover(|this| this.bg(rgb(0x161d28)).text_color(rgb(TEXT)))
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                // Hover listeners fire in reverse paint order. Let the entering
                // row win instead of letting the leaving row clear it.
                if *hovered {
                    if this.hovered_nav != Some(screen) {
                        this.hovered_nav = Some(screen);
                        cx.notify();
                    }
                } else if this.hovered_nav == Some(screen) {
                    this.hovered_nav = None;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.switch_screen(screen, cx);
            }))
            .child(content)
            .into_any_element()
    }

    pub(crate) fn nav_tooltip(&self) -> AnyElement {
        let Some(screen) = self.hovered_nav.filter(|_| self.sidebar_collapsed) else {
            // Keep the empty slot out of the root flex flow; an in-flow
            // zero-width child would still consume a 14px gap and shrink the
            // main column on every layout.
            return div().absolute().into_any_element();
        };
        let index = nav_index(screen);

        div()
            .absolute()
            .left(px(100.))
            .top(px(84.0 + nav_item_top(index)))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(7.))
            .bg(rgb(0x161c26))
            .border_1()
            .border_color(rgb(BORDER_STRONG))
            .text_size(px(10.))
            .text_color(rgb(TEXT))
            .whitespace_nowrap()
            .child(screen.title())
            .with_animation(
                ("nav-tooltip", index),
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                move |el, t| el.opacity(t).relative().left(px(-4.0 * (1.0 - t))),
            )
            .into_any_element()
    }

    pub(crate) fn rail_toggle(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.sidebar_collapsed;
        let epoch = self.sidebar_motion_epoch;
        let icon = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronLeft
        };
        let tooltip = if collapsed {
            "サイドバーを展開 (Ctrl+B)"
        } else {
            "サイドバーを最小化 (Ctrl+B)"
        };

        div()
            .absolute()
            .top(px(32.))
            .left(px(14. + sidebar_width(collapsed) - 12.))
            .child(
                Button::new("rail-toggle")
                    .ghost()
                    .icon(icon)
                    .tooltip(tooltip)
                    .size(px(24.))
                    .rounded_full()
                    .bg(rgb(0x131923))
                    .border_1()
                    .border_color(rgb(BORDER_STRONG))
                    .text_color(rgb(MUTED))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
            )
            .with_animation(
                ("rail-toggle", epoch),
                Animation::new(if epoch == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(440)
                })
                .with_easing(ease_motion)
                .with_max_fps(60.0),
                move |el, t| {
                    let from = sidebar_width(!collapsed);
                    let to = sidebar_width(collapsed);
                    el.left(px(14. + from + (to - from) * t - 12.))
                },
            )
            .into_any_element()
    }

    /// One compact row per failure domain. Detail text stays on the target
    /// screen so the bar never truncates the actual reason.
    fn global_alerts(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let import_failed = self
            .import_report
            .as_ref()
            .map(|report| import_issue_counts(&report.issues).0)
            .unwrap_or(0);
        alert_summaries(
            self.failed_job_count,
            self.daemon_errors.len(),
            import_failed,
        )
        .into_iter()
        .enumerate()
        .map(|(index, (target, message))| {
            let (id, action) = match target {
                Screen::Photos => ("alert-photos", "写真を開く"),
                Screen::Compression => ("alert-compression", "圧縮を開く"),
                Screen::Search => ("alert-search", "検索を開く"),
                Screen::Dashboard => ("alert-dashboard", "ダッシュボードを開く"),
                Screen::Settings => ("alert-settings", "設定を開く"),
            };
            h_flex()
                .flex_shrink_0()
                .w_full()
                .items_center()
                .gap(px(10.))
                .px(px(26.))
                .py(px(8.))
                .bg(rgb(PANEL))
                .border_b_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(rgb(RED)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(rgb(RED))
                        .child(message),
                )
                .child(
                    Button::new(id)
                        .label(action)
                        .small()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.switch_screen(target, cx);
                        })),
                )
                .with_animation(
                    ("global-alert", index),
                    Animation::new(MOTION_BASE)
                        .with_easing(ease_motion)
                        .with_max_fps(MOTION_FPS),
                    |el, t| el.opacity(t).relative().top(px(-8.0 * (1.0 - t))),
                )
                .into_any_element()
        })
        .collect()
    }

    pub(crate) fn content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let screen = match self.screen {
            Screen::Photos => self.photos_screen(window, cx).into_any_element(),
            Screen::Compression => self.compression_screen(window, cx).into_any_element(),
            Screen::Search => self.search_screen(window, cx).into_any_element(),
            Screen::Dashboard => self.dashboard_screen(cx).into_any_element(),
            Screen::Settings => self.settings_screen(cx).into_any_element(),
        };
        let enter = Animation::new(if self.screen_epoch == 0 {
            Duration::ZERO
        } else {
            MOTION_BASE
        })
        .with_easing(ease_motion)
        .with_max_fps(MOTION_FPS);
        let direction = self.screen_direction;
        let screen = v_flex().flex_1().min_h_0().child(screen).with_animation(
            ("screen-enter", self.screen_epoch),
            enter,
            move |el, t| {
                el.relative()
                    .left(px(direction * SCREEN_SLIDE * (1.0 - t)))
                    .opacity(t)
            },
        );
        v_flex()
            .flex_1()
            .min_h_0()
            .children(self.global_alerts(cx))
            .child(screen)
            .into_any_element()
    }
}

/// Window-wide ambient glass surface.
///
/// The compositor may refuse the `Blurred` window request, so this layer paints
/// its own dark base: a diagonal navy gradient plus three broad colour washes.
/// GPUI has no radial gradient, so each wash is a linear gradient anchored at a
/// corner and fading to fully transparent; stacked washes stay smooth at any
/// window size and never rely on element-level blur. Cards drawn after this
/// layer are opaque, which keeps text legible either way.
pub(crate) fn ambient_background() -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .overflow_hidden()
        .bg(linear_gradient(
            160.,
            linear_color_stop(rgba(0x0a1526cc), 0.),
            linear_color_stop(rgba(0x04060be6), 1.),
        ))
        // Blue from the top-left, violet from the top-right, teal from the
        // bottom. The stops cover ~half the window so the centre stays dark.
        .child(ambient_glow(135., BG_AMBIENT_A, 0.58))
        .child(ambient_glow(225., BG_AMBIENT_B, 0.52))
        .child(ambient_glow(0., BG_AMBIENT_C, 0.55))
}

/// One ambient colour wash. `angle` places the full-strength corner, and the
/// colour is transparent past `falloff`, so overlapping washes never stack
/// into a flat tint.
fn ambient_glow(angle: f32, color: u32, falloff: f32) -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .bg(linear_gradient(
            angle,
            linear_color_stop(rgba(color), 0.),
            linear_color_stop(rgba(color & 0xffffff00), falloff),
        ))
}

/// Compact, ordered summaries of every failure that deserves a global alert.
/// The target screen owns the detail, so the bar only carries the count.
fn alert_summaries(
    failed_jobs: i64,
    daemon_errors: usize,
    import_failed: usize,
) -> Vec<(Screen, String)> {
    let mut rows = Vec::new();
    if failed_jobs > 0 {
        rows.push((
            Screen::Dashboard,
            format!("圧縮に {failed_jobs} 件の失敗があります"),
        ));
    }
    if daemon_errors > 0 {
        rows.push((
            Screen::Dashboard,
            format!("デーモンで {daemon_errors} 件の処理エラーがあります"),
        ));
    }
    if import_failed > 0 {
        rows.push((
            Screen::Photos,
            format!("取り込みで {import_failed} 件失敗しました"),
        ));
    }
    rows
}
pub(crate) fn scan_progress_label(progress: &ScanProgressView) -> String {
    match progress.phase {
        IpcScanPhase::Walking => format!(
            "フォルダ {}/{} · 検索 {} 件",
            progress.root, progress.roots, progress.processed
        ),
        IpcScanPhase::Indexing => format!(
            "フォルダ {}/{} · 索引 {}/{}",
            progress.root,
            progress.roots,
            progress.processed,
            progress
                .total
                .map(|total| total.to_string())
                .unwrap_or_else(|| "?".to_string())
        ),
    }
}

pub(crate) fn scan_progress_ratio(progress: &ScanProgressView) -> f32 {
    let ratio = match progress.phase {
        IpcScanPhase::Walking => 0.0,
        IpcScanPhase::Indexing => progress
            .total
            .filter(|total| *total > 0)
            .map(|total| progress.processed as f32 / total as f32)
            .unwrap_or(0.0),
    };
    ratio.clamp(0.0, 1.0)
}

fn running_job_count(active: i64, waiting: i64) -> i64 {
    (active - waiting).max(0)
}

fn compression_card_status(active: i64, waiting: i64, failed: i64) -> CardStatus {
    let running = running_job_count(active, waiting);
    if waiting > 0 {
        CardStatus {
            headline: format!("未実行 {waiting}"),
            detail: format!("実行中 {running} · 失敗 {failed}"),
            tone: StatusTone::Waiting,
        }
    } else if running > 0 {
        CardStatus {
            headline: format!("実行中 {running}"),
            detail: format!("未実行 0 · 失敗 {failed}"),
            tone: StatusTone::Running,
        }
    } else if failed > 0 {
        CardStatus {
            headline: format!("失敗 {failed}"),
            detail: "未実行 0 · 実行中 0".to_string(),
            tone: StatusTone::Failed,
        }
    } else {
        CardStatus {
            headline: "キューは空です".to_string(),
            detail: "未実行のジョブはありません".to_string(),
            tone: StatusTone::Idle,
        }
    }
}

fn import_card_status(
    progress: Option<&ImportProgress>,
    report: Option<&ImportReportView>,
) -> CardStatus {
    if let Some(progress) = progress {
        return CardStatus {
            headline: "取り込み中".to_string(),
            detail: format!(
                "{}/{} · 取込 {} · 失敗 {}",
                progress.processed, progress.total, progress.imported, progress.failed
            ),
            tone: StatusTone::Running,
        };
    }
    let Some(report) = report else {
        return CardStatus {
            headline: "履歴なし".to_string(),
            detail: "取り込み結果がここに残ります".to_string(),
            tone: StatusTone::Idle,
        };
    };
    let (failed, partial) = import_issue_counts(&report.issues);
    let scan_failed = import_scan_failure_count(report);
    let failed_total = failed + scan_failed;
    if !report.remaining.is_empty() {
        return CardStatus {
            headline: format!("未処理 {}", report.remaining.len()),
            detail: format!(
                "取込 {}/{} · 失敗 {failed_total}",
                report.imported, report.total
            ),
            tone: StatusTone::Waiting,
        };
    }
    if failed_total > 0 {
        return CardStatus {
            headline: format!("失敗 {failed_total}"),
            detail: if scan_failed > 0 {
                format!(
                    "取込 {}/{} · スキャン失敗 {scan_failed}",
                    report.imported, report.total
                )
            } else {
                format!("取込 {}/{}", report.imported, report.total)
            },
            tone: StatusTone::Failed,
        };
    }
    if partial > 0 {
        return CardStatus {
            headline: format!("部分移動 {partial}"),
            detail: format!("取込 {}/{} · 完了", report.imported, report.total),
            tone: StatusTone::Waiting,
        };
    }
    CardStatus {
        headline: format!("完了 {} 枚", report.imported),
        detail: if report.renamed.is_empty() {
            "前回の取り込み".to_string()
        } else {
            format!("前回の取り込み · 連番 {}", report.renamed.len())
        },
        tone: StatusTone::Success,
    }
}

fn import_scan_failure_count(report: &ImportReportView) -> usize {
    report.scan_failures.len() + usize::from(report.scan_error.is_some())
}

fn scan_card_status(
    progress: Option<&ScanProgressView>,
    daemon_active: bool,
    daemon_paused: bool,
) -> CardStatus {
    if let Some(progress) = progress {
        let detail = scan_progress_label(progress);
        let has_total = progress.total.is_some_and(|total| total > 0);
        let headline = if progress.phase == IpcScanPhase::Indexing && has_total {
            format!(
                "{}%",
                (scan_progress_ratio(progress) * 100.0).round() as i32
            )
        } else {
            "走査中…".to_string()
        };
        return CardStatus {
            headline,
            detail,
            tone: StatusTone::Running,
        };
    }
    if !daemon_active {
        return CardStatus {
            headline: "停止中".to_string(),
            detail: "デーモンは接続されていません".to_string(),
            tone: StatusTone::Idle,
        };
    }
    if daemon_paused {
        return CardStatus {
            headline: "一時停止中".to_string(),
            detail: "監視を再開すると走査します".to_string(),
            tone: StatusTone::Waiting,
        };
    }
    CardStatus {
        headline: "待機中".to_string(),
        detail: "変更を監視しています".to_string(),
        tone: StatusTone::Idle,
    }
}

fn compression_attention(active: i64, waiting: i64, failed: i64) -> Option<StatusTone> {
    if failed > 0 {
        Some(StatusTone::Failed)
    } else if running_job_count(active, waiting) > 0 {
        Some(StatusTone::Running)
    } else if waiting > 0 {
        Some(StatusTone::Waiting)
    } else {
        None
    }
}

fn import_attention(
    progress: Option<&ImportProgress>,
    report: Option<&ImportReportView>,
) -> Option<StatusTone> {
    if progress.is_some() {
        return Some(StatusTone::Running);
    }
    let report = report?;
    let (failed, partial) = import_issue_counts(&report.issues);
    if failed > 0 || import_scan_failure_count(report) > 0 {
        Some(StatusTone::Failed)
    } else if !report.remaining.is_empty() || partial > 0 {
        Some(StatusTone::Waiting)
    } else {
        None
    }
}

fn scan_attention(
    progress: Option<&ScanProgressView>,
    daemon_active: bool,
    daemon_paused: bool,
) -> Option<StatusTone> {
    if progress.is_some() {
        Some(StatusTone::Running)
    } else if daemon_active && daemon_paused {
        Some(StatusTone::Waiting)
    } else {
        None
    }
}

fn import_running_progress(dialog: Option<&ImportDialogState>) -> Option<&ImportProgress> {
    match dialog.map(|dialog| &dialog.phase) {
        Some(ImportDialogPhase::Running { progress, .. }) => Some(progress),
        _ => None,
    }
}
fn nav_index(screen: Screen) -> usize {
    match screen {
        Screen::Photos => 0,
        Screen::Search => 1,
        Screen::Compression => 2,
        Screen::Dashboard => 3,
        Screen::Settings => 4,
    }
}

fn nav_item_top(index: usize) -> f32 {
    index as f32 * (NAV_ITEM_HEIGHT + NAV_ITEM_GAP)
}

fn nav_bar_left(collapsed: bool) -> f32 {
    if collapsed { 5.0 } else { -1.0 }
}

pub(crate) fn screen_direction(from: Screen, to: Screen) -> f32 {
    if nav_index(to) >= nav_index(from) {
        1.0
    } else {
        -1.0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NAV_ITEM_GAP, NAV_ITEM_HEIGHT, StatusTone, alert_summaries, compression_attention,
        compression_card_status, import_attention, import_card_status, import_running_progress,
        nav_index, nav_item_top, running_job_count, scan_attention, scan_card_status,
        scan_progress_label, scan_progress_ratio, screen_direction,
    };
    use crate::app::Screen;
    use crate::app::import::{
        ImportDialogOrigin, ImportDialogPhase, ImportDialogState, ImportReportView,
    };
    use crate::app::lifecycle::ScanProgressView;
    use crate::ui::theme::{AMBER, BLUE, GREEN, MUTED_2, RED};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use vrc_photo_core::importer::{
        ImportIssue, ImportIssueKind, ImportMode, ImportProgress, ImportRename,
    };
    use vrc_photo_core::scanner::ScanFailure;
    use vrc_photo_ipc::ScanPhase as IpcScanPhase;

    #[test]
    fn alert_summaries_follow_priority_order() {
        let rows = alert_summaries(2, 1, 3);
        assert_eq!(rows[0].0, Screen::Dashboard);
        assert!(rows[0].1.contains("2"));
        assert_eq!(rows[1].0, Screen::Dashboard);
        assert_eq!(rows[2].0, Screen::Photos);
        assert!(alert_summaries(0, 0, 0).is_empty());
    }

    #[test]
    fn dashboard_replaces_the_monitor_screen() {
        assert_eq!(Screen::Dashboard.title(), "ダッシュボード");
        assert_eq!(alert_summaries(0, 1, 0)[0].0, Screen::Dashboard);
    }
    #[test]
    fn nav_index_matches_sidebar_order() {
        assert_eq!(nav_index(Screen::Photos), 0);
        assert_eq!(nav_index(Screen::Search), 1);
        assert_eq!(nav_index(Screen::Compression), 2);
        assert_eq!(nav_index(Screen::Dashboard), 3);
        assert_eq!(nav_index(Screen::Settings), 4);
    }

    #[test]
    fn nav_item_top_uses_item_height_and_gap() {
        assert_eq!(nav_item_top(0), 0.0);
        assert_eq!(nav_item_top(1), NAV_ITEM_HEIGHT + NAV_ITEM_GAP);
        assert_eq!(nav_item_top(4), 4.0 * (NAV_ITEM_HEIGHT + NAV_ITEM_GAP));
    }

    #[test]
    fn screen_direction_follows_sidebar_order() {
        assert_eq!(screen_direction(Screen::Photos, Screen::Settings), 1.0);
        assert_eq!(screen_direction(Screen::Settings, Screen::Photos), -1.0);
        assert_eq!(screen_direction(Screen::Search, Screen::Search), 1.0);
    }
    #[test]
    fn scan_progress_label_uses_root_and_counts() {
        let progress = ScanProgressView {
            phase: IpcScanPhase::Indexing,
            processed: 50,
            total: Some(100),
            indexed: 49,
            root: 2,
            roots: 3,
        };
        assert_eq!(scan_progress_label(&progress), "フォルダ 2/3 · 索引 50/100");
        assert_eq!(scan_progress_ratio(&progress), 0.5);
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
        }
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

    fn indexing_fixture() -> ScanProgressView {
        ScanProgressView {
            phase: IpcScanPhase::Indexing,
            processed: 12,
            total: Some(20),
            indexed: 12,
            root: 1,
            roots: 1,
        }
    }

    #[test]
    fn running_job_count_clamps_waiting_above_active() {
        assert_eq!(running_job_count(6, 2), 4);
        assert_eq!(running_job_count(2, 6), 0);
        assert_eq!(running_job_count(0, 0), 0);
    }
    #[test]
    fn compression_card_status_prioritizes_waiting_running_and_failed() {
        let waiting = compression_card_status(6, 5, 2);
        assert_eq!(waiting.headline, "未実行 5");
        assert_eq!(waiting.detail, "実行中 1 · 失敗 2");
        assert_eq!(waiting.tone, StatusTone::Waiting);

        let running = compression_card_status(3, 0, 0);
        assert_eq!(running.headline, "実行中 3");
        assert_eq!(running.detail, "未実行 0 · 失敗 0");
        assert_eq!(running.tone, StatusTone::Running);

        let failed = compression_card_status(0, 0, 2);
        assert_eq!(failed.headline, "失敗 2");
        assert_eq!(failed.detail, "未実行 0 · 実行中 0");
        assert_eq!(failed.tone, StatusTone::Failed);

        let idle = compression_card_status(0, 0, 0);
        assert_eq!(idle.headline, "キューは空です");
        assert_eq!(idle.detail, "未実行のジョブはありません");
        assert_eq!(idle.tone, StatusTone::Idle);
    }
    #[test]
    fn import_card_status_covers_running_remaining_failed_partial_and_success() {
        let mut progress = progress_fixture(42, 100);
        progress.imported = 40;
        progress.failed = 1;
        let running = import_card_status(Some(&progress), None);
        assert_eq!(running.headline, "取り込み中");
        assert_eq!(running.detail, "42/100 · 取込 40 · 失敗 1");
        assert_eq!(running.tone, StatusTone::Running);

        let pending = ImportReportView {
            total: 100,
            processed: 40,
            imported: 40,
            issues: vec![non_retryable_issue("/bad.png")],
            remaining: vec![PathBuf::from("/next.png")],
            ..import_report_fixture()
        };
        let pending = import_card_status(None, Some(&pending));
        assert_eq!(pending.headline, "未処理 1");
        assert_eq!(pending.detail, "取込 40/100 · 失敗 1");
        assert_eq!(pending.tone, StatusTone::Waiting);

        let failed = ImportReportView {
            total: 10,
            processed: 10,
            imported: 8,
            issues: vec![non_retryable_issue("/a.png"), non_retryable_issue("/b.png")],
            ..import_report_fixture()
        };
        let failed = import_card_status(None, Some(&failed));
        assert_eq!(failed.headline, "失敗 2");
        assert_eq!(failed.detail, "取込 8/10");
        assert_eq!(failed.tone, StatusTone::Failed);

        let partial = ImportReportView {
            total: 10,
            processed: 10,
            imported: 9,
            issues: vec![partial_issue("/c.png")],
            ..import_report_fixture()
        };
        let partial = import_card_status(None, Some(&partial));
        assert_eq!(partial.headline, "部分移動 1");
        assert_eq!(partial.detail, "取込 9/10 · 完了");
        assert_eq!(partial.tone, StatusTone::Waiting);

        let success = ImportReportView {
            total: 10,
            processed: 10,
            imported: 10,
            renamed: vec![rename_fixture("/a.png", "/dest/a-1.png")],
            ..import_report_fixture()
        };
        let success = import_card_status(None, Some(&success));
        assert_eq!(success.headline, "完了 10 枚");
        assert_eq!(success.detail, "前回の取り込み · 連番 1");
        assert_eq!(success.tone, StatusTone::Success);

        let none = import_card_status(None, None);
        assert_eq!(none.headline, "履歴なし");
        assert_eq!(none.detail, "取り込み結果がここに残ります");
        assert_eq!(none.tone, StatusTone::Idle);
    }

    #[test]
    fn import_card_status_surfaces_scan_failures_and_unprocessed_priority() {
        let scan_only = ImportReportView {
            total: 10,
            processed: 10,
            imported: 10,
            scan_failures: vec![ScanFailure {
                path: PathBuf::from("/dest"),
                message: "scan failed".to_string(),
            }],
            ..import_report_fixture()
        };
        let scan_only_status = import_card_status(None, Some(&scan_only));
        assert_eq!(scan_only_status.headline, "失敗 1");
        assert_eq!(scan_only_status.detail, "取込 10/10 · スキャン失敗 1");
        assert_eq!(scan_only_status.tone, StatusTone::Failed);
        assert_eq!(
            import_attention(None, Some(&scan_only)),
            Some(StatusTone::Failed)
        );

        let hard_error = ImportReportView {
            scan_error: Some("cannot index".to_string()),
            ..import_report_fixture()
        };
        let hard_error_status = import_card_status(None, Some(&hard_error));
        assert_eq!(hard_error_status.headline, "失敗 1");
        assert_eq!(hard_error_status.detail, "取込 0/0 · スキャン失敗 1");
        assert_eq!(hard_error_status.tone, StatusTone::Failed);

        let triple = ImportReportView {
            total: 10,
            processed: 8,
            imported: 7,
            issues: vec![non_retryable_issue("/a.png"), partial_issue("/b.png")],
            remaining: vec![PathBuf::from("/c.png")],
            scan_failures: vec![ScanFailure {
                path: PathBuf::from("/dest"),
                message: "scan failed".to_string(),
            }],
            ..import_report_fixture()
        };
        let triple_status = import_card_status(None, Some(&triple));
        assert_eq!(triple_status.headline, "未処理 1");
        assert_eq!(triple_status.detail, "取込 7/10 · 失敗 2");
        assert_eq!(triple_status.tone, StatusTone::Waiting);
        assert_eq!(
            import_attention(None, Some(&triple)),
            Some(StatusTone::Failed)
        );
    }
    #[test]
    fn scan_card_status_handles_percent_walking_paused_and_stopped() {
        let indexing = ScanProgressView {
            phase: IpcScanPhase::Indexing,
            processed: 50,
            total: Some(100),
            indexed: 49,
            root: 2,
            roots: 3,
        };
        let indexing = scan_card_status(Some(&indexing), true, false);
        assert_eq!(indexing.headline, "50%");
        assert_eq!(indexing.detail, "フォルダ 2/3 · 索引 50/100");
        assert_eq!(indexing.tone, StatusTone::Running);

        let walking = ScanProgressView {
            phase: IpcScanPhase::Walking,
            processed: 12,
            total: None,
            indexed: 0,
            root: 1,
            roots: 2,
        };
        let walking = scan_card_status(Some(&walking), true, false);
        assert_eq!(walking.headline, "走査中…");
        assert_eq!(walking.detail, "フォルダ 1/2 · 検索 12 件");
        assert_eq!(walking.tone, StatusTone::Running);

        let unknown_total = ScanProgressView {
            total: None,
            ..indexing_fixture()
        };
        assert_eq!(
            scan_card_status(Some(&unknown_total), true, false).headline,
            "走査中…"
        );

        let zero_total = ScanProgressView {
            total: Some(0),
            ..indexing_fixture()
        };
        assert_eq!(
            scan_card_status(Some(&zero_total), true, false).headline,
            "走査中…"
        );

        let paused = scan_card_status(None, true, true);
        assert_eq!(paused.headline, "一時停止中");
        assert_eq!(paused.detail, "監視を再開すると走査します");
        assert_eq!(paused.tone, StatusTone::Waiting);

        let stopped = scan_card_status(None, false, false);
        assert_eq!(stopped.headline, "停止中");
        assert_eq!(stopped.detail, "デーモンは接続されていません");
        assert_eq!(stopped.tone, StatusTone::Idle);

        let idle = scan_card_status(None, true, false);
        assert_eq!(idle.headline, "待機中");
        assert_eq!(idle.detail, "変更を監視しています");
        assert_eq!(idle.tone, StatusTone::Idle);
    }
    #[test]
    fn attention_dots_prioritize_failures_over_running_and_waiting() {
        assert_eq!(compression_attention(6, 5, 2), Some(StatusTone::Failed));
        assert_eq!(compression_attention(4, 1, 0), Some(StatusTone::Running));
        assert_eq!(compression_attention(2, 2, 0), Some(StatusTone::Waiting));
        assert_eq!(compression_attention(0, 0, 0), None);

        let progress = progress_fixture(1, 10);
        assert_eq!(
            import_attention(Some(&progress), None),
            Some(StatusTone::Running)
        );

        let failed = ImportReportView {
            total: 2,
            processed: 2,
            imported: 1,
            issues: vec![non_retryable_issue("/a.png")],
            remaining: vec![PathBuf::from("/b.png")],
            ..import_report_fixture()
        };
        assert_eq!(
            import_attention(None, Some(&failed)),
            Some(StatusTone::Failed)
        );

        let pending = ImportReportView {
            remaining: vec![PathBuf::from("/b.png")],
            ..import_report_fixture()
        };
        assert_eq!(
            import_attention(None, Some(&pending)),
            Some(StatusTone::Waiting)
        );

        assert_eq!(import_attention(None, None), None);

        let scanning = ScanProgressView {
            phase: IpcScanPhase::Indexing,
            processed: 1,
            total: Some(10),
            indexed: 1,
            root: 1,
            roots: 1,
        };
        assert_eq!(
            scan_attention(Some(&scanning), true, false),
            Some(StatusTone::Running)
        );
        assert_eq!(scan_attention(None, true, true), Some(StatusTone::Waiting));
        assert_eq!(scan_attention(None, true, false), None);
        assert_eq!(scan_attention(None, false, false), None);
    }

    #[test]
    fn import_running_progress_only_reads_the_running_phase() {
        assert!(import_running_progress(None).is_none());

        let ready = import_dialog_fixture();
        assert!(import_running_progress(Some(&ready)).is_none());

        let mut running = import_dialog_fixture();
        running.phase = ImportDialogPhase::Running {
            progress: progress_fixture(1, 2),
            cancel: Arc::new(AtomicBool::new(false)),
            cancel_requested: false,
        };
        assert_eq!(
            import_running_progress(Some(&running)).map(|progress| progress.processed),
            Some(1)
        );
    }
    #[test]
    fn tone_colors_use_the_sidebar_palette() {
        assert_eq!(StatusTone::Idle.color(), MUTED_2);
        assert_eq!(StatusTone::Running.color(), BLUE);
        assert_eq!(StatusTone::Waiting.color(), AMBER);
        assert_eq!(StatusTone::Failed.color(), RED);
        assert_eq!(StatusTone::Success.color(), GREEN);
    }
}
