//! Titlebar line and compact command ribbon.
//!
//! The shell's header card owns two rows: the [`TitleBar`] line with app
//! identity, screen name, ribbon tabs and status, and the ribbon body with the
//! three command pages from design §10.2. Both stay opaque on top of the
//! window's glass backdrop.

use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::time::Duration;
use vrc_photo_core::importer::ImportMode;
use vrc_photo_core::settings::CardSize;

use crate::app::{App, Screen};
use crate::ui::theme::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RibbonTab {
    Home,
    Photo,
    Compression,
}

/// Default tab used by the message-strip visibility rule on a screen switch.
/// The ribbon itself shows every group, so this no longer selects a page.
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
    /// Narrow windows keep every command but show icons with tooltips so the
    /// three groups never clip or scroll horizontally.
    pub(crate) compact: bool,
}

pub(crate) fn ribbon_metrics(window_height: f32) -> RibbonMetrics {
    if ribbon_height(window_height) <= 62.0 {
        RibbonMetrics {
            large_height: 38.0,
            small_height: 19.0,
            group_label: 9.0,
            padding: 3.0,
            small_gap: 1.0,
            compact: false,
        }
    } else {
        RibbonMetrics {
            large_height: 44.0,
            small_height: 21.0,
            group_label: 10.0,
            padding: 4.0,
            small_gap: 2.0,
            compact: false,
        }
    }
}

/// §8.2: the ribbon's right-hand status note disappears at 1180 px and below.
pub(crate) const NARROW_CHROME_WIDTH: f32 = 1180.0;

/// Below this header width the labelled ribbon no longer fits beside the
/// status note. The compact ribbon keeps the same commands as icons with
/// tooltips so nothing clips and there is no horizontal scrollbar.
pub(crate) const RIBBON_LABELS_MIN_WIDTH: f32 = 1000.0;

pub(crate) fn ribbon_status_visible(window_width: f32) -> bool {
    window_width > NARROW_CHROME_WIDTH
}

/// The titlebar's compact sync readout hides with the ribbon status note so
/// the screen crumb, notification indicator, and window controls keep their
/// room at narrow widths.
pub(crate) fn titlebar_sync_visible(window_width: f32) -> bool {
    window_width > NARROW_CHROME_WIDTH
}

/// Compact sync text for the 46px titlebar. The full `daemon_sync_label`
/// (including the supervisor) stays on the Dashboard; the titlebar only needs
/// the age or transport state.
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

/// True when `App.message` needs the persistent strip above the screen because
/// the ribbon cannot show it: the body is collapsed, the Photo tab has no
/// status group, or the right-hand slot is hidden at narrow widths. Home and
/// Compression keep using the ribbon slot when it is visible, so a message is
/// never rendered twice.
pub(crate) fn message_strip_visible(collapsed: bool, tab: RibbonTab, content_width: f32) -> bool {
    collapsed || tab == RibbonTab::Photo || !ribbon_status_visible(content_width)
}

/// Chrome values derived from the window's content extent.
///
/// X11 client-side decorations report the outer window through
/// `viewport_size`, so the responsive thresholds must see the same inner size
/// the gallery and inspector use. `client_inset` is 0 on Wayland and on X11
/// without CSD.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChromeLayout {
    pub(crate) content_width: f32,
    pub(crate) content_height: f32,
    pub(crate) ribbon_height: f32,
    pub(crate) ribbon_metrics: RibbonMetrics,
    pub(crate) status_visible: bool,
    pub(crate) sync_visible: bool,
}

pub(crate) fn chrome_layout(
    viewport_width: f32,
    viewport_height: f32,
    client_inset: f32,
    sidebar_collapsed: bool,
) -> ChromeLayout {
    let content_width = window_content_extent(viewport_width, client_inset);
    let content_height = window_content_extent(viewport_height, client_inset);
    let header_width =
        (content_width - WORKSPACE_PAD * 2.0 - sidebar_width(sidebar_collapsed) - WORKSPACE_GAP)
            .max(0.0);
    let mut ribbon_metrics = ribbon_metrics(content_height);
    ribbon_metrics.compact = header_width < RIBBON_LABELS_MIN_WIDTH;
    ChromeLayout {
        content_width,
        content_height,
        ribbon_height: ribbon_height(content_height),
        ribbon_metrics,
        status_visible: ribbon_status_visible(content_width),
        sync_visible: titlebar_sync_visible(content_width),
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
    pub(crate) fn chrome(&mut self, layout: ChromeLayout, cx: &mut Context<Self>) -> AnyElement {
        let titlebar = self.titlebar(layout.sync_visible, cx);
        let collapsed = self.ribbon_collapsed;
        let epoch = self.ribbon_epoch;
        let body_height = layout.ribbon_height;
        let body = self.ribbon_body(layout, cx);
        let body = div()
            .w_full()
            .overflow_hidden()
            .with_animation(
                ("ribbon-body", epoch),
                Animation::new(if epoch == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(220)
                })
                .with_easing(ease_motion)
                .with_max_fps(self.motion_fps()),
                move |el, t| {
                    let height = if collapsed {
                        body_height * (1.0 - t)
                    } else {
                        body_height * t
                    };
                    el.h(px(height))
                        .opacity(if collapsed { 1.0 - t } else { t })
                },
            )
            .child(body);

        v_flex()
            .flex_shrink_0()
            .w_full()
            .rounded(px(22.))
            .bg(rgb(PANEL_HEADER))
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
            .child(body)
            .into_any_element()
    }

    fn titlebar(&mut self, show_sync: bool, cx: &mut Context<Self>) -> AnyElement {
        let screen = self.screen;

        let app_glyph = div()
            .size(px(22.))
            .flex_shrink_0()
            .rounded(px(7.))
            .flex()
            .items_center()
            .justify_center()
            .border_1()
            .border_color(rgba(0xffffff30))
            .bg(rgb(BLUE))
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

        let (daemon_tone, daemon_label, daemon_border, daemon_bg) = if self.demo_assets.is_some() {
            (BLUE, "デモモード", rgba(0x8bcbff29), rgba(0x8bcbff0e))
        } else if !self.daemon_active {
            (MUTED_2, "デーモン停止", rgba(0xffffff14), rgba(0xffffff08))
        } else if self.daemon_paused {
            (AMBER, "一時停止中", rgba(0xf4c87829), rgba(0xf4c87812))
        } else {
            (GREEN, "デーモン稼働中", rgba(0x7ee2b829), rgba(0x7ee2b80e))
        };
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

        let sync = titlebar_sync_label(
            self.daemon_age_secs,
            self.daemon_supervisor.as_deref(),
            self.ipc_connected,
            self.demo_assets.is_some(),
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
            .child(div().flex_1().min_w_0())
            .child(status_pill)
            .when(show_sync, |this| this.child(sync_label))
            .child(collapse_button(collapsed, cx))
            .child(self.notification_button(cx));

        TitleBar::new()
            .h(px(46.))
            .flex_shrink_0()
            .pl(px(14.))
            .pr(px(8.))
            .border_color(rgba(0xffffff0e))
            .bg(rgba(0x00000000))
            .child(line)
            .into_any_element()
    }

    fn ribbon_body(&mut self, layout: ChromeLayout, cx: &mut Context<Self>) -> AnyElement {
        let show_status = layout.status_visible;
        let metrics = layout.ribbon_metrics;
        h_flex()
            .w_full()
            .h(px(layout.ribbon_height))
            .flex_shrink_0()
            .items_stretch()
            // The three groups must fit at 1080 px; the shell design forbids a
            // horizontal scrollbar anywhere in the ribbon.
            .overflow_hidden()
            .child(self.ribbon_home(show_status, metrics, cx))
            .child(self.ribbon_photo(metrics, cx))
            .child(self.ribbon_compression(metrics, cx))
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
                cx.notify();
            },
        );
        let library_group = ribbon_group(vec![reload], "ライブラリ", false, metrics);

        let mut display_items = vec![self.card_size_selector(metrics, cx)];
        if self.screen == Screen::Photos {
            display_items.push(large_button(
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
            ));
        }
        let display_group = ribbon_group(display_items, "表示", false, metrics);

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
            .flex_shrink_0()
            .items_stretch()
            .child(import_group)
            .child(library_group)
            .child(display_group)
            .when(show_status, |this| this.child(status_group))
            .into_any_element()
    }

    fn card_size_selector(&mut self, metrics: RibbonMetrics, cx: &mut Context<Self>) -> AnyElement {
        let current = self.settings.ui_card_size;
        let mut row = h_flex().gap(px(2.)).items_center();
        for size in [
            CardSize::Small,
            CardSize::Medium,
            CardSize::Large,
            CardSize::ExtraLarge,
        ] {
            let active = size == current;
            row = row.child(
                div()
                    .id(SharedString::from(format!("ribbon-card-size-{size:?}")))
                    .h(px(metrics.small_height.max(20.)))
                    .px(px(6.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(if active {
                        rgba(0x8bcbff29)
                    } else {
                        rgba(0x00000000)
                    })
                    .when(active, |this| this.bg(rgba(0x8bcbff17)))
                    .text_size(px(9.))
                    .text_color(rgb(if active { TEXT } else { MUTED }))
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| this.set_card_size(size, cx)))
                    .child(card_size_label(size)),
            );
        }
        row.into_any_element()
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
            ],
            "選択",
            false,
            metrics,
        );

        let organize_group = ribbon_group(
            vec![
                large_button(
                    "ribbon-trash",
                    "ゴミ箱へ",
                    IconName::Delete,
                    if !has_selection {
                        RibbonLook::Disabled
                    } else {
                        RibbonLook::Normal
                    },
                    metrics,
                    cx,
                    |this, window, cx| this.trash_selected(window, cx),
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
            .flex_shrink_0()
            .items_stretch()
            .child(select_group)
            .child(organize_group)
            .into_any_element()
    }

    fn ribbon_compression(&mut self, metrics: RibbonMetrics, cx: &mut Context<Self>) -> AnyElement {
        let has_selection = !self.selected_action_ids().is_empty();
        let waiting = self.waiting_job_count;
        let run_group = ribbon_group(
            vec![
                large_button(
                    "ribbon-compress-folder",
                    "指定フォルダ",
                    IconName::FolderOpen,
                    selection_look(has_selection),
                    metrics,
                    cx,
                    |this, _, cx| this.compress_selection_to_folder(cx),
                ),
                large_button(
                    "ribbon-compress-profile",
                    "プロファイル先",
                    IconName::FolderOpen,
                    selection_look(has_selection),
                    metrics,
                    cx,
                    |this, _, cx| this.compress_selection_to_profile_folder(cx),
                ),
                large_button(
                    "ribbon-compress-clipboard",
                    "クリップボード",
                    IconName::Copy,
                    selection_look(has_selection),
                    metrics,
                    cx,
                    |this, _, cx| this.compress_selection_to_clipboard(cx),
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
            ],
            "圧縮",
            false,
            metrics,
        );

        run_group.into_any_element()
    }

    fn photo_browser_empty(&self) -> bool {
        if self.screen == Screen::Search {
            self.search_view.index.entries.is_empty()
        } else {
            self.library.index.entries.is_empty()
        }
    }
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
        .on_double_click(|_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(|this, _, _, cx| {
            this.toggle_ribbon_collapsed(cx);
            cx.stop_propagation();
        }))
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
        // Keep the three ribbon groups inside the default 1280 px window and
        // the 1080 px minimum without a horizontal scroll surface.
        .px(px(6.))
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
                        .gap(px(2.))
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
    Disabled,
}

impl RibbonLook {
    fn enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    fn active(self) -> bool {
        matches!(self, Self::Active)
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
    let (bg, border, fg) = if !enabled {
        (rgba(0x00000000), rgba(0x00000000), rgba(0x76819680))
    } else if active {
        (rgba(0x8bcbff1a), rgba(0x8bcbff2e), rgb(TEXT))
    } else {
        (rgba(0x00000000), rgba(0x00000000), rgb(MUTED))
    };

    let mut button = div()
        .id(id)
        .h(px(metrics.large_height))
        .min_w(px(if metrics.compact { 30. } else { 42. }))
        .flex_shrink_0()
        .px(px(if metrics.compact { 2. } else { 3. }))
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
            if active { rgb(BLUE) } else { rgba(0x8bcbffe0) }
        } else {
            rgb(MUTED_2)
        }));

    if metrics.compact {
        button = button.tooltip(move |window, cx| Tooltip::new(label).build(window, cx));
    } else {
        button = button.child(label);
    }

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
        CHROME_ICONS, RibbonTab, chrome_layout, default_ribbon_tab, is_bundled,
        message_strip_visible, ribbon_height, ribbon_metrics, ribbon_selection_label,
        ribbon_status_visible, titlebar_sync_label, titlebar_sync_visible,
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
    fn ribbon_uses_icons_below_1280() {
        assert!(
            chrome_layout(1279.0, 820.0, 0.0, false)
                .ribbon_metrics
                .compact
        );
        assert!(
            !chrome_layout(1280.0, 820.0, 0.0, false)
                .ribbon_metrics
                .compact
        );
        // Collapsed sidebar: 1080 - 28 - 78 - 14 = 960 (compact), while
        // 1280 - 28 - 78 - 14 = 1160 leaves room for labels.
        assert!(
            chrome_layout(1080.0, 700.0, 0.0, true)
                .ribbon_metrics
                .compact
        );
        assert!(
            !chrome_layout(1280.0, 820.0, 0.0, true)
                .ribbon_metrics
                .compact
        );
    }

    #[test]
    fn status_note_hides_at_1180() {
        assert!(!ribbon_status_visible(1180.0));
        assert!(ribbon_status_visible(1181.0));
        assert!(!titlebar_sync_visible(1180.0));
        assert!(titlebar_sync_visible(1181.0));
    }

    #[test]
    fn titlebar_sync_label_stays_compact() {
        assert_eq!(titlebar_sync_label(Some(5), None, true, false), "5秒前");
        assert_eq!(titlebar_sync_label(None, None, true, false), "IPC");
        assert_eq!(titlebar_sync_label(None, None, false, false), "停止");
        assert_eq!(
            titlebar_sync_label(None, Some("systemd"), true, true),
            "デモ"
        );
    }

    #[test]
    fn message_strip_covers_slots_the_ribbon_cannot_show() {
        // Wide Home/Compression keep the message in the ribbon status slot.
        assert!(!message_strip_visible(false, RibbonTab::Home, 1440.0));
        assert!(!message_strip_visible(
            false,
            RibbonTab::Compression,
            1280.0
        ));
        // The Photo page has no status group at any width.
        assert!(message_strip_visible(false, RibbonTab::Photo, 1440.0));
        // A collapsed ribbon hides the whole body, including the slot.
        assert!(message_strip_visible(true, RibbonTab::Home, 1440.0));
        // The status slot disappears at 1180 and below.
        assert!(message_strip_visible(false, RibbonTab::Home, 1180.0));
        assert!(message_strip_visible(false, RibbonTab::Compression, 1080.0));
        assert!(!message_strip_visible(false, RibbonTab::Home, 1181.0));
    }

    #[test]
    fn chrome_layout_uses_the_content_extent() {
        // The requested 1080x700 is the content size; X11 reports the outer
        // 1120x740 window, and the 20 px CSD inset on each edge is removed.
        let minimum = chrome_layout(1120.0, 740.0, 20.0, false);
        assert_eq!(minimum.content_width, 1080.0);
        assert_eq!(minimum.content_height, 700.0);
        assert_eq!(minimum.ribbon_height, 62.0);
        let mut expected_metrics = ribbon_metrics(700.0);
        expected_metrics.compact = true;
        assert_eq!(minimum.ribbon_metrics, expected_metrics);
        assert!(!minimum.status_visible);
        assert!(!minimum.sync_visible);

        // 1280x820 content keeps the regular ribbon and status readout.
        let regular = chrome_layout(1320.0, 860.0, 20.0, false);
        assert_eq!(regular.content_width, 1280.0);
        assert_eq!(regular.content_height, 820.0);
        assert_eq!(regular.ribbon_height, 68.0);
        assert!(regular.status_visible);
        assert!(regular.sync_visible);

        let large = chrome_layout(1480.0, 940.0, 20.0, false);
        assert_eq!(large.content_width, 1440.0);
        assert_eq!(large.ribbon_height, 68.0);
        assert!(large.status_visible);
    }

    #[test]
    fn chrome_layout_thresholds_sit_on_the_content_edge() {
        assert_eq!(chrome_layout(1200.0, 760.0, 0.0, false).ribbon_height, 62.0);
        assert_eq!(chrome_layout(1200.0, 761.0, 0.0, false).ribbon_height, 68.0);
        assert!(!chrome_layout(1180.0, 820.0, 0.0, false).status_visible);
        assert!(chrome_layout(1181.0, 820.0, 0.0, false).status_visible);
        assert!(!chrome_layout(1180.0, 820.0, 0.0, false).sync_visible);
        assert!(chrome_layout(1181.0, 820.0, 0.0, false).sync_visible);
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
