//! Titlebar line and compact command ribbon.
//!
//! The shell's header card owns two rows: the [`TitleBar`] line with app
//! identity, screen name, ribbon tabs and status, and the ribbon body with the
//! three command pages from design §10.2. Both stay opaque on top of the
//! window's glass backdrop.

use gpui_kit::assets::IconName;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::importer::ImportMode;

use crate::app::daemon::open_path;
use crate::app::{App, Screen};
use crate::state::viewer::ViewerOpenSource;
use crate::ui::theme::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RibbonTab {
    Home,
    Photo,
    Compression,
}

impl RibbonTab {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Home => "ホーム",
            Self::Photo => "写真",
            Self::Compression => "圧縮",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Home => "ribbon-tab-home",
            Self::Photo => "ribbon-tab-photo",
            Self::Compression => "ribbon-tab-compression",
        }
    }
}

/// Default tab for a screen switch. Manual tab choices survive until the next
/// switch; Compression owns its screen, everything else starts at Home.
pub(crate) fn default_ribbon_tab(screen: Screen) -> RibbonTab {
    match screen {
        Screen::Compression => RibbonTab::Compression,
        _ => RibbonTab::Home,
    }
}

/// Compact ribbon height. Short windows trade 6px for the gallery.
pub(crate) fn ribbon_height(window_height: f32) -> f32 {
    if window_height <= 760.0 { 62.0 } else { 68.0 }
}

/// Ribbon metrics for the current window height. The regular 68px body has
/// room for 44px buttons; the 62px body uses the mock's short mode (38/19px
/// buttons, smaller group label and padding) so no label or button clips.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RibbonMetrics {
    pub(crate) large_height: f32,
    pub(crate) small_height: f32,
    pub(crate) group_label: f32,
    pub(crate) padding: f32,
    pub(crate) small_gap: f32,
}

pub(crate) fn ribbon_metrics(window_height: f32) -> RibbonMetrics {
    if ribbon_height(window_height) <= 62.0 {
        RibbonMetrics {
            large_height: 38.0,
            small_height: 19.0,
            group_label: 9.0,
            padding: 3.0,
            small_gap: 1.0,
        }
    } else {
        RibbonMetrics {
            large_height: 44.0,
            small_height: 21.0,
            group_label: 10.0,
            padding: 4.0,
            small_gap: 2.0,
        }
    }
}

/// §8.2: the ribbon's right-hand status note disappears at 1180 px and below.
pub(crate) const NARROW_CHROME_WIDTH: f32 = 1180.0;

pub(crate) fn ribbon_status_visible(window_width: f32) -> bool {
    window_width > NARROW_CHROME_WIDTH
}

/// The titlebar's sparkline of sync state hides with the ribbon status note so
/// the screen crumb, tabs, notification bell, and window controls keep their
/// room at narrow widths.
pub(crate) fn titlebar_sync_visible(window_width: f32) -> bool {
    window_width > NARROW_CHROME_WIDTH
}

/// Compact sync text for the 46px titlebar. The full
/// `daemon_sync_label` (including the supervisor) stays in the Photos header
/// and on the Dashboard; the titlebar only needs the age or transport state.
pub(crate) fn titlebar_sync_label(
    age_secs: Option<i64>,
    _supervisor: Option<&str>,
    ipc_connected: bool,
    demo: bool,
) -> String {
    if demo {
        return "デモ".to_string();
    }
    match age_secs.filter(|age| *age < 20) {
        Some(age) => format!("{age}秒前"),
        None if ipc_connected => "IPC".to_string(),
        None => "停止".to_string(),
    }
}

/// Number of photos reflected in the ribbon's status note.
pub(crate) fn ribbon_selection_label(selected: usize, total: i64, bytes: u64) -> String {
    if selected == 0 {
        format!("{} 枚 / {}", format_count(total), format_bytes(bytes))
    } else {
        format!(
            "選択中 {selected} 枚 · {} 枚 / {}",
            format_count(total),
            format_bytes(bytes)
        )
    }
}

/// Every icon chrome renders. `gpui_kit::assets::Assets` embeds only the icons
/// in `gpui-kit-assets-0.8.0/default-icons.txt`; the full `IconName` catalog is
/// much larger and unbundled variants paint nothing without an error. Keep this
/// list in sync with every icon passed to [`chrome_icon`].
const CHROME_ICONS: &[IconName] = &[
    IconName::Asterisk,
    IconName::Calendar,
    IconName::Bell,
    IconName::ChevronDown,
    IconName::ChevronUp,
    IconName::Copy,
    IconName::Replace,
    IconName::RefreshCw,
    IconName::FolderOpen,
    IconName::RotateCw,
    IconName::PanelRight,
    IconName::GalleryVerticalEnd,
    IconName::CircleAlert,
    IconName::Check,
    IconName::Close,
    IconName::Star,
    IconName::Maximize,
    IconName::BookOpen,
    IconName::FileText,
    IconName::Delete,
    IconName::Minimize,
    IconName::Play,
    IconName::LayoutDashboard,
    IconName::ChartPie,
    IconName::CircleCheck,
];

fn is_bundled(name: IconName) -> bool {
    gpui_kit::assets::Assets::get(name.path().as_ref()).is_some()
}

fn chrome_icon(name: IconName) -> Icon {
    debug_assert!(
        CHROME_ICONS.contains(&name) && is_bundled(name),
        "chrome icon {name:?} is not listed or not in the bundled gpui-kit asset set"
    );
    Icon::new(name)
}

impl App {
    /// Header card: titlebar line plus the ribbon body. Sits above the
    /// transparent content area with a 14px gap.
    pub(crate) fn chrome(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let window_height = f32::from(window.bounds().size.height);
        let window_width = f32::from(window.bounds().size.width);
        let titlebar = self.titlebar(titlebar_sync_visible(window_width), cx);
        let body =
            (!self.ribbon_collapsed).then(|| self.ribbon_body(window_height, window_width, cx));

        v_flex()
            .flex_shrink_0()
            .w_full()
            .rounded(px(22.))
            .bg(linear_gradient(
                180.,
                linear_color_stop(rgb(PANEL_HEADER), 0.),
                linear_color_stop(rgb(PANEL_CARD), 1.),
            ))
            .border_1()
            .border_color(rgba(0xffffff1b))
            .overflow_hidden()
            .shadow(vec![
                BoxShadow::new(px(0.), px(18.), rgba(0x00000047).into()).blur_radius(px(44.)),
                BoxShadow::new(px(0.), px(1.), rgba(0xffffff10).into())
                    .inset()
                    .blur_radius(px(0.)),
            ])
            .child(titlebar)
            .children(body)
            .into_any_element()
    }

    fn titlebar(&mut self, show_sync: bool, cx: &mut Context<Self>) -> AnyElement {
        let screen = self.screen;
        let active_tab = self.active_ribbon_tab;

        let app_glyph = div()
            .size(px(22.))
            .flex_shrink_0()
            .rounded(px(7.))
            .flex()
            .items_center()
            .justify_center()
            .border_1()
            .border_color(rgba(0xffffff30))
            .bg(linear_gradient(
                145.,
                linear_color_stop(rgb(0x74c8ff), 0.),
                linear_color_stop(rgb(0xa67eff), 1.),
            ))
            .shadow(vec![
                BoxShadow::new(px(0.), px(3.), rgba(0x608cff47).into()).blur_radius(px(9.)),
                BoxShadow::new(px(0.), px(1.), rgba(0xffffff66).into())
                    .inset()
                    .blur_radius(px(0.)),
            ])
            .child(
                chrome_icon(IconName::Asterisk)
                    .with_size(px(13.))
                    .text_color(rgb(0xffffff)),
            );

        let tabs = h_flex()
            .flex_shrink_0()
            .ml(px(7.))
            .pl(px(9.))
            .gap(px(2.))
            .border_l_1()
            .border_color(rgba(0xffffff14))
            .child(tab_button(RibbonTab::Home, active_tab, cx))
            .child(tab_button(RibbonTab::Photo, active_tab, cx))
            .child(tab_button(RibbonTab::Compression, active_tab, cx));

        let (daemon_tone, daemon_label, daemon_border, daemon_bg) = if self.demo_assets.is_some() {
            (BLUE, "デモモード", rgba(0x8bcbff29), rgba(0x8bcbff0e))
        } else if !self.daemon_active {
            (MUTED_2, "デーモン停止", rgba(0xffffff14), rgba(0xffffff08))
        } else if self.daemon_paused {
            (AMBER, "一時停止中", rgba(0xf4c87829), rgba(0xf4c87812))
        } else {
            (GREEN, "デーモン稼働中", rgba(0x7ee2b829), rgba(0x7ee2b80e))
        };
        let sync = titlebar_sync_label(
            self.daemon_age_secs,
            self.daemon_supervisor.as_deref(),
            self.ipc_connected,
            self.demo_assets.is_some(),
        );
        let has_message = self.message.is_some();
        let collapsed = self.ribbon_collapsed;

        let status_pill = h_flex()
            .flex_shrink_0()
            .h(px(22.))
            .px(px(8.))
            .gap(px(6.))
            .items_center()
            .rounded_full()
            .border_1()
            .border_color(daemon_border)
            .bg(daemon_bg)
            .child(div().size(px(4.)).rounded_full().bg(rgb(daemon_tone)))
            .child(
                div()
                    .text_size(px(8.5))
                    .text_color(rgb(daemon_tone))
                    .whitespace_nowrap()
                    .child(daemon_label),
            );

        let sync_label = h_flex()
            .min_w_0()
            .max_w(px(96.))
            .gap(px(4.))
            .items_center()
            .child(
                chrome_icon(IconName::Calendar)
                    .with_size(px(11.))
                    .text_color(rgb(MUTED_2)),
            )
            .child(
                div()
                    .min_w_0()
                    .max_w(px(96.))
                    .truncate()
                    .font_family("monospace")
                    .text_size(px(7.5))
                    .text_color(rgb(MUTED_2))
                    .child(sync),
            );

        let line = h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .gap(px(9.))
            .child(app_glyph)
            .child(
                div()
                    .max_w(px(140.))
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("VRC Photo Manager"),
            )
            .child(div().w(px(1.)).h(px(14.)).bg(rgba(0xffffff14)))
            .child(
                div()
                    .max_w(px(140.))
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.5))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child(screen.title()),
            )
            .child(tabs)
            .child(div().flex_1().min_w_0())
            .child(status_pill)
            .when(show_sync, |this| this.child(sync_label))
            .child(collapse_button(collapsed, cx))
            .child(
                div()
                    .id("titlebar-notifications")
                    .relative()
                    .flex_shrink_0()
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .text_color(rgb(MUTED_2))
                    .child(chrome_icon(IconName::Bell).with_size(px(14.)))
                    .when(has_message, |this| {
                        this.child(
                            div()
                                .absolute()
                                .top(px(5.))
                                .right(px(5.))
                                .size(px(4.))
                                .rounded_full()
                                .bg(rgb(AMBER))
                                .shadow(vec![
                                    BoxShadow::new(px(0.), px(0.), rgba(0xf4c878cc).into())
                                        .blur_radius(px(6.)),
                                ]),
                        )
                    }),
            );

        TitleBar::new()
            .h(px(46.))
            .flex_shrink_0()
            .pl(px(14.))
            .pr(px(8.))
            .border_color(rgba(0xffffff0e))
            .bg(linear_gradient(
                180.,
                linear_color_stop(rgba(0xffffff0a), 0.),
                linear_color_stop(rgba(0x00000000), 1.),
            ))
            .child(line)
            .into_any_element()
    }

    fn ribbon_body(
        &mut self,
        window_height: f32,
        window_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let show_status = ribbon_status_visible(window_width);
        let metrics = ribbon_metrics(window_height);
        let page = match self.active_ribbon_tab {
            RibbonTab::Home => self.ribbon_home(show_status, metrics, cx),
            RibbonTab::Photo => self.ribbon_photo(metrics, cx),
            RibbonTab::Compression => self.ribbon_compression(show_status, metrics, cx),
        };
        h_flex()
            .w_full()
            .h(px(ribbon_height(window_height)))
            .flex_shrink_0()
            .items_stretch()
            .overflow_hidden()
            .child(page)
            .into_any_element()
    }

    fn ribbon_home(
        &mut self,
        show_status: bool,
        metrics: RibbonMetrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let import_group = ribbon_group(
            vec![
                large_button(
                    "ribbon-import-copy",
                    "コピー取込",
                    IconName::Copy,
                    RibbonLook::Normal,
                    metrics,
                    cx,
                    |this, window, cx| this.open_import_dialog(ImportMode::Copy, window, cx),
                ),
                large_button(
                    "ribbon-import-move",
                    "移動取込",
                    IconName::Replace,
                    RibbonLook::Normal,
                    metrics,
                    cx,
                    |this, window, cx| this.open_import_dialog(ImportMode::Move, window, cx),
                ),
            ],
            "取り込み",
            false,
            metrics,
        );

        let reload = large_button(
            "ribbon-reload",
            "再読込",
            IconName::RefreshCw,
            RibbonLook::Normal,
            metrics,
            cx,
            |this, _, cx| {
                if matches!(this.screen, Screen::Photos | Screen::Search) {
                    this.reload_library(true, cx);
                } else {
                    this.reload_aux(cx);
                }
            },
        );
        let open_folder = small_button(
            "ribbon-open-folder",
            "フォルダを開く",
            IconName::FolderOpen,
            if self.settings.photo_roots.is_empty() {
                RibbonLook::Disabled
            } else {
                RibbonLook::Normal
            },
            metrics,
            cx,
            |this, _, cx| match this.settings.photo_roots.first() {
                Some(root) => {
                    if let Err(error) = open_path(std::path::Path::new(root)) {
                        this.message = Some(format!("フォルダを開けません: {error}"));
                        cx.notify();
                    }
                }
                None => {
                    this.message = Some("写真フォルダが設定されていません".to_string());
                    cx.notify();
                }
            },
        );
        let rebuild = small_button(
            "ribbon-rebuild-index",
            "索引を再構築",
            IconName::RotateCw,
            RibbonLook::Normal,
            metrics,
            cx,
            |this, _, cx| this.request_daemon_rescan(cx),
        );
        let library_group = ribbon_group(
            vec![
                reload,
                v_flex()
                    .flex_shrink_0()
                    .justify_center()
                    .gap(px(metrics.small_gap))
                    .child(open_folder)
                    .child(rebuild)
                    .into_any_element(),
            ],
            "ライブラリ",
            false,
            metrics,
        );

        let inspector_group = (self.screen == Screen::Photos).then(|| {
            ribbon_group(
                vec![large_button(
                    "ribbon-toggle-inspector",
                    "詳細パネル",
                    IconName::PanelRight,
                    if self.inspector_collapsed {
                        RibbonLook::Normal
                    } else {
                        RibbonLook::Active
                    },
                    metrics,
                    cx,
                    |this, _, cx| this.toggle_inspector(cx),
                )],
                "表示",
                false,
                metrics,
            )
        });

        let status = self.message.clone().unwrap_or_else(|| {
            ribbon_selection_label(
                self.selected_ids.len(),
                self.stats.photo_count,
                self.stats.total_bytes,
            )
        });
        let status_group = ribbon_group(
            vec![status_note(
                if self.message.is_some() {
                    IconName::CircleAlert
                } else {
                    IconName::GalleryVerticalEnd
                },
                status,
                self.message.is_some(),
            )],
            "現在の状態",
            true,
            metrics,
        );

        h_flex()
            .w_full()
            .items_stretch()
            .child(import_group)
            .child(library_group)
            .when_some(inspector_group, |this, group| this.child(group))
            .when(show_status, |this| this.child(status_group))
            .into_any_element()
    }

    fn ribbon_photo(&mut self, metrics: RibbonMetrics, cx: &mut Context<Self>) -> AnyElement {
        let has_selection = self.selected_photo.is_some();
        let select_group = ribbon_group(
            vec![
                large_button(
                    "ribbon-select-all",
                    "すべて選択",
                    IconName::Check,
                    if self.photo_browser_empty() {
                        RibbonLook::Disabled
                    } else {
                        RibbonLook::Normal
                    },
                    metrics,
                    cx,
                    |this, _, cx| this.select_all_photos(cx),
                ),
                large_button(
                    "ribbon-clear-selection",
                    "選択解除",
                    IconName::Close,
                    if self.selected_ids.is_empty() {
                        RibbonLook::Disabled
                    } else {
                        RibbonLook::Normal
                    },
                    metrics,
                    cx,
                    |this, _, cx| this.clear_selection(cx),
                ),
                large_button(
                    "ribbon-favorite",
                    "お気に入り",
                    IconName::Star,
                    if !has_selection {
                        RibbonLook::Disabled
                    } else if self
                        .selected_photo
                        .as_ref()
                        .is_some_and(|photo| photo.is_favorite)
                    {
                        RibbonLook::Active
                    } else {
                        RibbonLook::Normal
                    },
                    metrics,
                    cx,
                    |this, _, cx| this.toggle_selected_favorite(cx),
                ),
            ],
            "選択",
            false,
            metrics,
        );

        let detail_group = ribbon_group(
            vec![
                large_button(
                    "ribbon-fullscreen",
                    "全画面",
                    IconName::Maximize,
                    selection_look(has_selection),
                    metrics,
                    cx,
                    |this, window, cx| {
                        this.open_quick_look(window, cx, ViewerOpenSource::Inspector)
                    },
                ),
                large_button(
                    "ribbon-tags",
                    "タグ",
                    IconName::BookOpen,
                    selection_look(has_selection),
                    metrics,
                    cx,
                    |this, window, cx| this.edit_selected_tags(window, cx),
                ),
                large_button(
                    "ribbon-note",
                    "メモ",
                    IconName::FileText,
                    selection_look(has_selection),
                    metrics,
                    cx,
                    |this, window, cx| this.edit_selected_note(window, cx),
                ),
            ],
            "詳細",
            false,
            metrics,
        );

        let armed = self
            .selected
            .is_some_and(|id| self.delete_armed == Some(id));
        let organize_group = ribbon_group(
            vec![
                large_button(
                    "ribbon-trash",
                    "ゴミ箱へ",
                    IconName::Delete,
                    if !has_selection {
                        RibbonLook::Disabled
                    } else if armed {
                        RibbonLook::Active
                    } else {
                        RibbonLook::Normal
                    },
                    metrics,
                    cx,
                    |this, _, cx| this.trash_selected(cx),
                ),
                large_button(
                    "ribbon-manual-compress",
                    "圧縮",
                    IconName::Minimize,
                    selection_look(!self.selected_action_ids().is_empty()),
                    metrics,
                    cx,
                    |this, _, cx| {
                        let ids = this.selected_action_ids();
                        this.open_manual_compression(ids, cx);
                    },
                ),
            ],
            "整理",
            false,
            metrics,
        );

        h_flex()
            .w_full()
            .items_stretch()
            .child(select_group)
            .child(detail_group)
            .child(organize_group)
            .into_any_element()
    }

    fn ribbon_compression(
        &mut self,
        show_status: bool,
        metrics: RibbonMetrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_selection = !self.selected_action_ids().is_empty();
        let waiting = self.waiting_job_count;
        let failed = self.failed_job_count;
        let run_group = ribbon_group(
            vec![
                large_button(
                    "ribbon-compress-selection",
                    "手動圧縮",
                    IconName::Minimize,
                    if has_selection {
                        RibbonLook::Primary
                    } else {
                        RibbonLook::Disabled
                    },
                    metrics,
                    cx,
                    |this, _, cx| {
                        let ids = this.selected_action_ids();
                        this.open_manual_compression(ids, cx);
                    },
                ),
                large_button(
                    "ribbon-release-waiting",
                    "待機を圧縮",
                    IconName::Play,
                    if waiting > 0 {
                        RibbonLook::Normal
                    } else {
                        RibbonLook::Disabled
                    },
                    metrics,
                    cx,
                    |this, window, cx| this.request_release_waiting_jobs(window, cx),
                ),
                large_button(
                    "ribbon-retry-failed",
                    "失敗を再試行",
                    IconName::RotateCw,
                    if failed > 0 {
                        RibbonLook::Normal
                    } else {
                        RibbonLook::Disabled
                    },
                    metrics,
                    cx,
                    |this, _, cx| this.retry_all_failed_jobs(cx),
                ),
            ],
            "実行",
            false,
            metrics,
        );

        let queue_group = ribbon_group(
            vec![
                large_button(
                    "ribbon-open-queue",
                    "キュー",
                    IconName::LayoutDashboard,
                    RibbonLook::Normal,
                    metrics,
                    cx,
                    |this, _, cx| this.switch_screen(Screen::Dashboard, cx),
                ),
                large_button(
                    "ribbon-open-status",
                    "状態",
                    IconName::ChartPie,
                    RibbonLook::Normal,
                    metrics,
                    cx,
                    |this, _, cx| this.switch_screen(Screen::Dashboard, cx),
                ),
            ],
            "キュー",
            false,
            metrics,
        );

        let auto = if self.settings.auto_compress {
            "自動適用: ON"
        } else {
            "自動適用: OFF"
        };
        let profile = self
            .profiles
            .get(self.profile_index)
            .map(profile_summary)
            .unwrap_or_else(|| "プロファイルなし".to_string());
        let policy = if self.settings.auto_compress_keep_original {
            "元画像を保持"
        } else {
            "元画像を置換"
        };
        let status_group = ribbon_group(
            vec![status_note(
                IconName::CircleCheck,
                format!("{auto} · {profile} · {policy}"),
                false,
            )],
            "現在の設定",
            true,
            metrics,
        );

        h_flex()
            .w_full()
            .items_stretch()
            .child(run_group)
            .child(queue_group)
            .when(show_status, |this| this.child(status_group))
            .into_any_element()
    }

    fn photo_browser_empty(&self) -> bool {
        if self.screen == Screen::Search {
            self.search_view.index.entries.is_empty()
        } else {
            self.library.index.entries.is_empty()
        }
    }
}

fn tab_button(tab: RibbonTab, active: RibbonTab, cx: &mut Context<App>) -> AnyElement {
    let is_active = tab == active;
    let mut button = div()
        .id(tab.id())
        .h(px(27.))
        .px(px(10.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .border_1()
        .text_size(px(10.))
        .whitespace_nowrap()
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, _, cx| this.select_ribbon_tab(tab, cx)));
    if is_active {
        button = button
            .border_color(rgba(0x8bcbff29))
            .bg(rgba(0x8bcbff1a))
            .text_color(rgb(TEXT))
            .shadow(vec![
                BoxShadow::new(px(0.), px(1.), rgba(0xffffff12).into())
                    .inset()
                    .blur_radius(px(0.)),
            ]);
    } else {
        button = button
            .border_color(rgba(0x00000000))
            .text_color(rgb(MUTED_2))
            .hover(|this| this.bg(rgba(0xffffff0a)).text_color(rgb(TEXT)));
    }
    button.child(tab.label()).into_any_element()
}

fn collapse_button(collapsed: bool, cx: &mut Context<App>) -> AnyElement {
    let icon = if collapsed {
        IconName::ChevronDown
    } else {
        IconName::ChevronUp
    };
    div()
        .id("ribbon-collapse")
        .flex_shrink_0()
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .text_color(rgb(MUTED_2))
        .cursor_pointer()
        .hover(|this| this.bg(rgba(0xffffff0a)).text_color(rgb(TEXT)))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(|this, _, _, cx| this.toggle_ribbon_collapsed(cx)))
        .child(chrome_icon(icon).with_size(px(14.)))
        .into_any_element()
}

fn ribbon_group(
    items: Vec<AnyElement>,
    label: &'static str,
    grow: bool,
    metrics: RibbonMetrics,
) -> AnyElement {
    h_flex()
        .flex_shrink_0()
        .when(grow, |this| this.flex_1().min_w_0())
        .items_stretch()
        .py(px(metrics.padding))
        .px(px(10.))
        .border_r_1()
        .border_color(rgba(0xffffff0d))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .justify_center()
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap(px(3.))
                        .children(items),
                )
                .child(
                    div()
                        .pl(px(2.))
                        .text_size(px(metrics.group_label))
                        .text_color(rgb(MUTED_2))
                        .whitespace_nowrap()
                        .child(label),
                ),
        )
        .into_any_element()
}

#[derive(Clone, Copy)]
enum RibbonLook {
    Normal,
    Active,
    Primary,
    Disabled,
}

impl RibbonLook {
    fn enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    fn active(self) -> bool {
        matches!(self, Self::Active)
    }

    fn primary(self) -> bool {
        matches!(self, Self::Primary)
    }
}

fn selection_look(enabled: bool) -> RibbonLook {
    if enabled {
        RibbonLook::Normal
    } else {
        RibbonLook::Disabled
    }
}

fn large_button(
    id: &'static str,
    label: &'static str,
    icon: IconName,
    look: RibbonLook,
    metrics: RibbonMetrics,
    cx: &mut Context<App>,
    on_click: impl Fn(&mut App, &mut Window, &mut Context<App>) + 'static,
) -> AnyElement {
    let enabled = look.enabled();
    let active = look.active();
    let primary = look.primary();
    let (bg, border, fg) = if !enabled {
        (rgba(0x00000000), rgba(0x00000000), rgba(0x76819680))
    } else if primary {
        (rgba(0x66b3ff33), rgba(0x8bcbff38), rgb(0xf8fcff))
    } else if active {
        (rgba(0x8bcbff1a), rgba(0x8bcbff2e), rgb(TEXT))
    } else {
        (rgba(0x00000000), rgba(0x00000000), rgb(MUTED))
    };

    let mut button = div()
        .id(id)
        .h(px(metrics.large_height))
        .min_w(px(50.))
        .flex_shrink_0()
        .px(px(6.))
        .py(px(if metrics.large_height <= 38.0 { 1. } else { 2. }))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(2.))
        .rounded(px(7.))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_size(px(8.5))
        .text_color(fg)
        .whitespace_nowrap()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(chrome_icon(icon).with_size(px(18.)).text_color(if enabled {
            if active || primary {
                rgb(BLUE)
            } else {
                rgba(0x8bcbffe0)
            }
        } else {
            rgb(MUTED_2)
        }))
        .child(label);

    if enabled {
        button = button
            .cursor_pointer()
            .hover(|this| {
                this.border_color(rgba(0xffffff14))
                    .bg(rgba(0xffffff0e))
                    .text_color(rgb(TEXT))
            })
            .on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| on_click(this, window, cx)),
            );
    } else {
        button = button.opacity(0.42);
    }
    button.into_any_element()
}

fn small_button(
    id: &'static str,
    label: &'static str,
    icon: IconName,
    look: RibbonLook,
    metrics: RibbonMetrics,
    cx: &mut Context<App>,
    on_click: impl Fn(&mut App, &mut Window, &mut Context<App>) + 'static,
) -> AnyElement {
    let enabled = look.enabled();
    let active = look.active();
    let mut button = h_flex()
        .id(id)
        .h(px(metrics.small_height))
        .min_w(px(110.))
        .flex_shrink_0()
        .px(px(7.))
        .gap(px(5.))
        .items_center()
        .rounded(px(6.))
        .border_1()
        .border_color(if active {
            rgba(0x8bcbff29)
        } else {
            rgba(0x00000000)
        })
        .when(active, |this| this.bg(rgba(0x8bcbff17)))
        .text_size(px(8.5))
        .text_color(rgb(if enabled {
            if active { TEXT } else { MUTED }
        } else {
            MUTED_2
        }))
        .whitespace_nowrap()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            chrome_icon(icon)
                .with_size(px(12.))
                .text_color(rgb(if !enabled {
                    MUTED_2
                } else if active {
                    BLUE
                } else {
                    MUTED
                })),
        )
        .child(label);

    if enabled {
        button = button
            .cursor_pointer()
            .hover(|this| this.bg(rgba(0xffffff0e)).text_color(rgb(TEXT)))
            .on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| on_click(this, window, cx)),
            );
    } else {
        button = button.opacity(0.42);
    }
    button.into_any_element()
}

fn status_note(icon: IconName, text: String, warning: bool) -> AnyElement {
    h_flex()
        .flex_1()
        .min_w_0()
        .justify_end()
        .gap(px(6.))
        .items_center()
        .child(
            chrome_icon(icon)
                .with_size(px(13.))
                .text_color(rgb(if warning { AMBER } else { MUTED_2 })),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(10.))
                .text_color(rgb(if warning { AMBER } else { MUTED }))
                .child(text),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{
        CHROME_ICONS, RibbonTab, default_ribbon_tab, is_bundled, ribbon_height, ribbon_metrics,
        ribbon_selection_label, ribbon_status_visible, titlebar_sync_label, titlebar_sync_visible,
    };
    use crate::app::Screen;

    #[test]
    fn goal_screen_owns_the_compression_tab() {
        assert_eq!(
            default_ribbon_tab(Screen::Compression),
            RibbonTab::Compression
        );
        for screen in [
            Screen::Photos,
            Screen::Search,
            Screen::Dashboard,
            Screen::Settings,
        ] {
            assert_eq!(default_ribbon_tab(screen), RibbonTab::Home, "{screen:?}");
        }
    }

    #[test]
    fn ribbon_height_switches_at_760() {
        assert_eq!(ribbon_height(700.0), 62.0);
        assert_eq!(ribbon_height(760.0), 62.0);
        assert_eq!(ribbon_height(761.0), 68.0);
    }

    #[test]
    fn short_ribbon_metrics_fit_the_62px_body() {
        let short = ribbon_metrics(700.0);
        assert_eq!(short.large_height, 38.0);
        assert_eq!(short.small_height, 19.0);
        assert_eq!(short.group_label, 9.0);
        assert_eq!(short.padding, 3.0);
        // Two stacked small buttons stay close to one large button row.
        assert!(short.small_height * 2.0 + short.small_gap <= short.large_height + 3.0);
        // Group row + one label line + vertical padding fits the 62px body
        // without clipping the label.
        assert!(short.large_height + short.group_label * 1.4 + short.padding * 2.0 <= 62.0);

        let regular = ribbon_metrics(820.0);
        assert_eq!(regular.large_height, 44.0);
        assert_eq!(regular.small_height, 21.0);
        assert_eq!(regular.group_label, 10.0);
        assert_eq!(regular.padding, 4.0);
    }

    #[test]
    fn every_chrome_icon_is_bundled() {
        for &name in CHROME_ICONS {
            assert!(
                is_bundled(name),
                "chrome icon {name:?} is missing from gpui_kit::assets::Assets"
            );
        }
    }

    #[test]
    fn titlebar_sync_label_stays_compact() {
        assert_eq!(
            titlebar_sync_label(Some(5), Some("systemd"), false, false),
            "5秒前"
        );
        assert_eq!(
            titlebar_sync_label(
                Some(5),
                Some("a-very-long-supervisor-name-that-would-overflow-the-titlebar"),
                false,
                false
            ),
            "5秒前"
        );
        assert_eq!(titlebar_sync_label(None, None, true, true), "デモ");
        assert_eq!(titlebar_sync_label(None, None, true, false), "IPC");
        assert_eq!(titlebar_sync_label(None, None, false, false), "停止");
        assert_eq!(
            titlebar_sync_label(Some(30), Some("systemd"), true, false),
            "IPC"
        );
    }

    #[test]
    fn titlebar_sync_hides_at_1180() {
        assert!(!titlebar_sync_visible(1180.0));
        assert!(titlebar_sync_visible(1181.0));
    }

    #[test]
    fn status_note_hides_at_1180() {
        assert!(!ribbon_status_visible(1180.0));
        assert!(ribbon_status_visible(1181.0));
    }

    #[test]
    fn selection_status_handles_empty_and_multiple() {
        assert_eq!(ribbon_selection_label(0, 2418, 0), "2,418 枚 / 0 B");
        assert_eq!(
            ribbon_selection_label(3, 2418, 1_048_576),
            "選択中 3 枚 · 2,418 枚 / 1.0 MB"
        );
    }
}
