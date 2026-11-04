//! Settings screen: library paths, display options, startup, AI plugin
//! management, and compress policies.
//!
//! The switch, card-size, and field widgets come from `ui::components`; screen
//! dispatch lives in `ui::shell`.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::slider::Slider;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::prelude::StatefulInteractiveElement as _;
use gpui_kit::*;
use std::path::Path;
use vrc_photon_ai_protocol::ModelState;
use vrc_photon_core::ai::PluginInstallation;
use vrc_photon_core::model::DaemonCommand;
use vrc_photon_core::settings::CardSize;
use vrc_photon_core::thumbnail;
use vrc_photon_i18n::{tr, tr_args};
use vrc_photon_ipc::Response;

use crate::app::App;
use crate::app::ai::{
    AiManifestView, AiPluginAction, AiPluginCardState, AiPluginCardView, ModelDownloadView,
    ai_error_text_in, ai_manifest_view, installation_row_id, is_active_installation,
    model_progress_ratio, variant_backend_label,
};
use crate::app::daemon::{autostart_description, open_path};
use crate::ipc_bridge;
use crate::state::photo_browser::{anchor_photo_id, list_scroll_top};
use crate::ui::components::{dialog_panel, glass_panel, status_progress_bar};
use crate::ui::palette::{
    accent_colors, find_accent, find_main, main_colors, preview_accent, preview_main, readable_on,
    resolve,
};
use crate::ui::theme::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeAxis {
    Main,
    Accent,
}

impl ThemeAxis {
    fn key(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Accent => "accent",
        }
    }

    fn title(self) -> String {
        match self {
            Self::Main => tr!("theme.color.main.title"),
            Self::Accent => tr!("theme.color.accent.title"),
        }
    }
}

impl App {
    pub(crate) fn settings_screen(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let autostart_hint =
            autostart_description(self.daemon_active, self.daemon_supervisor.as_deref());
        let cache_label = if self.settings.thumbnail_cache_mb == 0 {
            tr!("settings.cache.unlimited")
        } else {
            format!("{} MB", self.settings.thumbnail_cache_mb)
        };
        let active_language = vrc_photon_i18n::normalize(&self.settings.language);
        let language_name = vrc_photon_i18n::locales()
            .into_iter()
            .find(|locale| locale.code == active_language)
            .map(|locale| locale.name)
            .unwrap_or(active_language);
        h_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(
                v_flex()
                    .id("settings-sections")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.settings_scroll)
                    .gap(px(CARD_GAP))
                    .child(self.language_card(cx))
                    .child(self.theme_card(cx))
                    .child(
                        glass_panel(&self.palette)
                            .flex()
                            .flex_col()
                            .p(px(14.))
                            .gap(px(12.))
                            .rounded(px(17.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("settings.library.title")),
                            )
                            .child(self.filter_field(
                                tr!("settings.library.photo_roots"),
                                Input::new(&self.photo_root_input),
                            ))
                            .child(self.filter_field(
                                tr!("settings.library.log_dir"),
                                Input::new(&self.log_dir_input),
                            ))
                            .child(self.filter_field(
                                tr!("settings.library.import_dir"),
                                Input::new(&self.import_dir_input),
                            ))
                            .child(
                                h_flex()
                                    .gap(px(8.))
                                    .items_center()
                                    .child(
                                        Button::new("save-paths")
                                            .label(tr!("settings.library.save_paths"))
                                            .small()
                                            .primary()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                let root = this
                                                    .photo_root_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let log =
                                                    this.log_dir_input.read(cx).value().to_string();
                                                let imported = this
                                                    .import_dir_input
                                                    .read(cx)
                                                    .value()
                                                    .to_string();
                                                let previous = this.settings.clone();
                                                this.settings.photo_roots =
                                                    if root.trim().is_empty() {
                                                        Vec::new()
                                                    } else {
                                                        vec![root.trim().to_string()]
                                                    };
                                                this.settings.log_dir = if log.trim().is_empty() {
                                                    None
                                                } else {
                                                    Some(log.trim().to_string())
                                                };
                                                this.settings.managed_dir =
                                                    if imported.trim().is_empty() {
                                                        None
                                                    } else {
                                                        Some(imported.trim().to_string())
                                                    };
                                                let root_changed = this.settings.photo_roots
                                                    != previous.photo_roots;
                                                let paths_changed = root_changed
                                                    || this.settings.log_dir != previous.log_dir
                                                    || this.settings.managed_dir
                                                        != previous.managed_dir;
                                                if let Err(error) = this.settings.save(&this.db) {
                                                    this.settings = previous;
                                                    this.toast_error(
                                                        tr_args!(
                                                            "settings.language.save_failed",
                                                            error = error
                                                        ),
                                                        cx,
                                                    );
                                                    tracing::warn!(
                                                        %error,
                                                        "cannot save path settings"
                                                    );
                                                    cx.notify();
                                                    return;
                                                }
                                                if root_changed {
                                                    let revision = this.db.library_revision().ok();
                                                    this.apply_library_snapshot_with_anchor(
                                                        revision, false, cx,
                                                    );
                                                }
                                                if paths_changed {
                                                    let requested = this.db.request_daemon_command(
                                                        DaemonCommand::Rescan,
                                                    );
                                                    if requested.is_ok() {
                                                        this.send_wake_commands(cx);
                                                    }
                                                    this.toast_success(
                                                        match requested {
                                                            Ok(_) => {
                                                                tr!("settings.library.saved_rescan")
                                                            }
                                                            Err(error) => {
                                                                tracing::warn!(
                                                                    %error,
                                                                    "cannot request daemon rescan"
                                                                );
                                                                tr!(
                                                                    "settings.library.saved_restart"
                                                                )
                                                            }
                                                        },
                                                        cx,
                                                    );
                                                } else {
                                                    this.toast_success(
                                                        tr!("settings.library.saved"),
                                                        cx,
                                                    );
                                                }
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted_2))
                                            .child(tr!("settings.library.save_hint")),
                                    ),
                            ),
                    )
                    .child(self.ai_plugin_card(cx))
                    .child(
                        glass_panel(&self.palette)
                            .flex()
                            .flex_col()
                            .p(px(14.))
                            .gap(px(12.))
                            .rounded(px(17.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("settings.world_name.title")),
                            )
                            .child(self.setting_switch(
                                tr!("settings.world_name.enabled"),
                                tr!("settings.world_name.hint"),
                                self.settings.world_name_resolution_enabled,
                                false,
                                cx,
                                |settings, value| settings.world_name_resolution_enabled = value,
                            ))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("settings.world_name.privacy")),
                            ),
                    )
                    .child(
                        glass_panel(&self.palette)
                            .flex()
                            .flex_col()
                            .p(px(14.))
                            .gap(px(12.))
                            .rounded(px(17.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("settings.display.title")),
                            )
                            .child(self.setting_switch(
                                tr!("settings.display.status.label"),
                                tr!("settings.display.status.hint"),
                                self.settings.ui_card_show_status,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_status = value,
                            ))
                            .child(self.setting_switch(
                                tr!("settings.display.world.label"),
                                tr!("settings.display.world.hint"),
                                self.settings.ui_card_show_world,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_world = value,
                            ))
                            .child(self.setting_switch(
                                tr!("settings.display.date.label"),
                                tr!("settings.display.date.hint"),
                                self.settings.ui_card_show_date,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_date = value,
                            ))
                            .child(self.setting_switch(
                                tr!("settings.display.size.label"),
                                tr!("settings.display.size.hint"),
                                self.settings.ui_card_show_size,
                                true,
                                cx,
                                |settings, value| settings.ui_card_show_size = value,
                            )),
                    )
                    .child(
                        glass_panel(&self.palette)
                            .flex()
                            .flex_col()
                            .p(px(14.))
                            .gap(px(12.))
                            .rounded(px(17.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("settings.performance.title")),
                            )
                            .child(
                                v_flex()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(p.text))
                                            .child(tr!("settings.performance.max_fps.label")),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted_2))
                                            .child(tr!("settings.performance.max_fps.hint")),
                                    )
                                    .child(
                                        h_flex().flex_wrap().gap(px(4.)).children(
                                            [
                                                (30u32, "30".to_string()),
                                                (60, "60".to_string()),
                                                (120, "120".to_string()),
                                                (144, "144".to_string()),
                                                (240, "240".to_string()),
                                                (0, tr!("settings.performance.max_fps.unlimited")),
                                            ]
                                            .into_iter()
                                            .map(
                                                |(fps, label)| {
                                                    let active = self.settings.ui_max_fps == fps;
                                                    div()
                                                        .id(SharedString::from(format!(
                                                            "max-fps-{fps}"
                                                        )))
                                                        .px(px(10.))
                                                        .py(px(5.))
                                                        .rounded(px(8.))
                                                        .bg(if active {
                                                            rgb(p.accent_soft)
                                                        } else {
                                                            rgba(0x00000000)
                                                        })
                                                        .text_size(px(11.))
                                                        .font_medium()
                                                        .text_color(if active {
                                                            rgb(p.accent)
                                                        } else {
                                                            rgb(p.muted)
                                                        })
                                                        .cursor_pointer()
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.set_max_fps(fps, cx)
                                                            },
                                                        ))
                                                        .child(label)
                                                },
                                            ),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        glass_panel(&self.palette)
                            .flex()
                            .flex_col()
                            .p(px(14.))
                            .gap(px(12.))
                            .rounded(px(17.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("settings.thumbnail.title")),
                            )
                            .child(
                                v_flex()
                                    .gap(px(6.))
                                    .child(
                                        h_flex()
                                            .items_baseline()
                                            .gap(px(6.))
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(rgb(p.text))
                                                    .child(tr!("settings.thumbnail.quality")),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(p.muted_2))
                                                    .child(tr!("settings.thumbnail.quality_hint")),
                                            )
                                            .child(div().flex_1())
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .font_semibold()
                                                    .text_color(rgb(p.text))
                                                    .child(
                                                        self.settings.thumbnail_quality.to_string(),
                                                    ),
                                            ),
                                    )
                                    .child(Slider::new(&self.thumbnail_quality_slider).w_full()),
                            )
                            .child(
                                v_flex()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(p.text))
                                            .child(tr!("settings.thumbnail.size")),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted_2))
                                            .child(tr!("settings.thumbnail.size_hint")),
                                    )
                                    .child(h_flex().gap(px(4.)).children(
                                        thumbnail::THUMBNAIL_SIZES.into_iter().map(|size| {
                                            let active = self.settings.thumbnail_size == size;
                                            div()
                                                .id(SharedString::from(format!(
                                                    "thumbnail-size-{size}"
                                                )))
                                                .px(px(10.))
                                                .py(px(5.))
                                                .rounded(px(8.))
                                                .bg(if active {
                                                    rgb(p.accent_soft)
                                                } else {
                                                    rgba(0x00000000)
                                                })
                                                .text_size(px(11.))
                                                .font_medium()
                                                .text_color(if active {
                                                    rgb(p.accent)
                                                } else {
                                                    rgb(p.muted)
                                                })
                                                .cursor_pointer()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.set_thumbnail_size(size, cx)
                                                }))
                                                .child(format!("{size} px"))
                                        }),
                                    )),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .pt(px(2.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted))
                                            .child(match self.thumbnail_cache_usage {
                                                Some(usage) => tr_args!(
                                                    "settings.thumbnail.usage",
                                                    files = usage.files,
                                                    size = format_bytes(usage.bytes)
                                                ),
                                                None => tr!("settings.thumbnail.usage_loading"),
                                            }),
                                    )
                                    .child(
                                        Button::new("clear-thumbnail-cache")
                                            .label(tr!("settings.thumbnail.clear"))
                                            .small()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.clear_thumbnail_cache(cx)
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("settings.thumbnail.clear_hint")),
                            ),
                    )
                    .child(
                        glass_panel(&self.palette)
                            .flex()
                            .flex_col()
                            .p(px(14.))
                            .gap(px(12.))
                            .rounded(px(17.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("settings.startup.title")),
                            )
                            .child(self.setting_switch(
                                tr!("settings.startup.autostart"),
                                autostart_hint,
                                self.settings.autostart,
                                false,
                                cx,
                                |settings, value| settings.autostart = value,
                            ))
                            .child(self.setting_switch(
                                tr!("settings.startup.tray"),
                                tr!("settings.startup.tray_hint"),
                                self.settings.tray,
                                false,
                                cx,
                                |settings, value| settings.tray = value,
                            ))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(px(12.))
                                    .py(px(2.))
                                    .child(
                                        v_flex()
                                            .gap(px(2.))
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(rgb(p.text))
                                                    .child(tr!("settings.startup.reduce_motion")),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(p.muted_2))
                                                    .child(tr!(
                                                        "settings.startup.reduce_motion_hint"
                                                    )),
                                            ),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Switch::new("reduce-motion")
                                            .checked(self.settings.ui_reduce_motion)
                                            .on_click(cx.listener(|this, value, _, cx| {
                                                this.set_reduce_motion(*value, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(self.update_card(cx))
                    .child(self.log_card(cx))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                Button::new("settings-reload")
                                    .label(tr!("settings.reload"))
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_aux(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(div().text_size(px(10.)).text_color(rgb(p.muted_2)).child(
                                tr_args!(
                                    "settings.footer.summary",
                                    language = language_name,
                                    cache = cache_label
                                ),
                            )),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(8.))
                    .child(Scrollbar::vertical(&self.settings_scroll)),
            )
    }

    fn language_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let active = vrc_photon_i18n::normalize(&self.settings.language);
        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .p(px(14.))
            .gap(px(12.))
            .rounded(px(17.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child(tr!("settings.language.title")),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("settings.language.hint")),
            )
            .child(h_flex().flex_wrap().gap(px(4.)).children(
                vrc_photon_i18n::locales().into_iter().map(|locale| {
                    let selected = locale.code == active;
                    let code = locale.code.clone();
                    div()
                        .id(SharedString::from(format!("language-{}", locale.code)))
                        .px(px(10.))
                        .py(px(5.))
                        .rounded(px(8.))
                        .bg(if selected {
                            rgb(p.accent_soft)
                        } else {
                            rgba(0x00000000)
                        })
                        .text_size(px(11.))
                        .font_medium()
                        .text_color(if selected {
                            rgb(p.accent)
                        } else {
                            rgb(p.muted)
                        })
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_language(&code, window, cx)
                        }))
                        .child(locale.name)
                }),
            ))
    }

    fn theme_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .p(px(14.))
            .gap(px(12.))
            .rounded(px(17.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child(tr!("settings.theme.title")),
            )
            .child(self.theme_axis(ThemeAxis::Main, cx))
            .child(self.theme_axis(ThemeAxis::Accent, cx))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("settings.theme.hint")),
            )
    }

    fn theme_axis(&self, axis: ThemeAxis, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let selected_main = find_main(&self.settings.ui_theme_main);
        let selected_accent = find_accent(&self.settings.ui_theme_accent);
        let selected_label = match axis {
            ThemeAxis::Main => selected_main.label(),
            ThemeAxis::Accent => selected_accent.label(),
        };
        let items: Vec<(&'static str, String, u32, bool)> = match axis {
            ThemeAxis::Main => main_colors()
                .iter()
                .map(|color| {
                    (
                        color.id,
                        color.label(),
                        preview_main(color),
                        selected_main.id == color.id,
                    )
                })
                .collect(),
            ThemeAxis::Accent => accent_colors()
                .iter()
                .map(|color| {
                    (
                        color.id,
                        color.label(),
                        preview_accent(color),
                        selected_accent.id == color.id,
                    )
                })
                .collect(),
        };
        // GPUI tooltips only appear on hover, so while a swatch in this
        // axis holds keyboard focus its color name is repeated in the
        // heading, in the accent color, next to the selected color name.
        let focused_label = self
            .theme_focused
            .filter(|(focused_axis, _)| *focused_axis == axis)
            .and_then(|(_, index)| items.get(index))
            .map(|item| item.1.clone())
            .filter(|label| *label != selected_label);
        let mut rows = v_flex().gap(px(8.));
        for (chunk_index, chunk) in items.chunks(6).enumerate() {
            let mut row = h_flex().items_center().gap(px(8.));
            for (offset, item) in chunk.iter().enumerate() {
                let (id, label, swatch, selected) = item;
                let index = chunk_index * 6 + offset;
                row = row.child(self.theme_swatch(
                    axis,
                    index,
                    id,
                    label.clone(),
                    *swatch,
                    *selected,
                    cx,
                ));
            }
            rows = rows.child(row);
        }
        v_flex()
            .gap(px(8.))
            .child(
                h_flex()
                    .items_baseline()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_medium()
                            .text_color(rgb(p.text))
                            .child(axis.title()),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(format!("· {selected_label}")),
                    )
                    .when_some(focused_label, |this, label| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.accent))
                                .child(format!("· {label}")),
                        )
                    }),
            )
            .child(rows)
    }

    #[allow(clippy::too_many_arguments)]
    fn theme_swatch(
        &self,
        axis: ThemeAxis,
        index: usize,
        id: &'static str,
        label: String,
        swatch: u32,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.palette;
        let focus = match axis {
            ThemeAxis::Main => self.theme_main_focus[index].clone(),
            ThemeAxis::Accent => self.theme_accent_focus[index].clone(),
        };
        let click_focus = focus.clone();
        let key_focus = focus;
        div()
            .id(SharedString::from(format!("theme-{}-{id}", axis.key())))
            .size(px(30.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_2()
            .border_color(if selected {
                rgb(p.accent_border)
            } else {
                rgba(0x00000000)
            })
            .cursor_pointer()
            .track_focus(&click_focus)
            .tab_stop(true)
            .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
            .hover(move |this| this.bg(rgb(p.panel_inner)))
            .focus(move |this| this.border_color(rgb(p.accent)).bg(rgb(p.panel_inner)))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                // Keyboard activation is handled on key down; ignore the
                // click GPUI synthesizes on key up so the theme is not
                // applied twice.
                if matches!(event, ClickEvent::Keyboard(_)) {
                    return;
                }
                window.focus(&click_focus, cx);
                match axis {
                    ThemeAxis::Main => this.set_main_theme(id, cx),
                    ThemeAxis::Accent => this.set_accent_theme(id, cx),
                }
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if !event.is_held && (key == "enter" || key == "space") {
                    window.prevent_default();
                    cx.stop_propagation();
                    window.focus(&key_focus, cx);
                    match axis {
                        ThemeAxis::Main => this.set_main_theme(id, cx),
                        ThemeAxis::Accent => this.set_accent_theme(id, cx),
                    }
                }
            }))
            .child(
                div()
                    .size(px(24.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(rgb(swatch))
                    .border_1()
                    .border_color(rgba(with_alpha(p.text, 0x24)))
                    .when(selected, |this| {
                        this.child(
                            div()
                                .text_size(px(12.))
                                .font_semibold()
                                .text_color(rgb(readable_on(swatch)))
                                .child("✓"),
                        )
                    }),
            )
    }

    pub(crate) fn set_main_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = find_main(id);
        if self.settings.ui_theme_main == id.id {
            return;
        }
        let previous = (self.settings.ui_theme_main.clone(), self.palette);
        self.settings.ui_theme_main = id.id.to_string();
        let palette = resolve(&self.settings.ui_theme_main, &self.settings.ui_theme_accent);
        self.palette = palette;
        if let Err(error) = self.settings.save(&self.db) {
            self.settings.ui_theme_main = previous.0;
            self.palette = previous.1;
            self.toast_error(tr_args!("settings.language.save_failed", error = error), cx);
            tracing::warn!(%error, "cannot save theme setting");
            cx.notify();
            return;
        }
        self.apply_palette(self.palette, cx);
    }

    pub(crate) fn set_accent_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = find_accent(id);
        if self.settings.ui_theme_accent == id.id {
            return;
        }
        let previous = (self.settings.ui_theme_accent.clone(), self.palette);
        self.settings.ui_theme_accent = id.id.to_string();
        let palette = resolve(&self.settings.ui_theme_main, &self.settings.ui_theme_accent);
        self.palette = palette;
        if let Err(error) = self.settings.save(&self.db) {
            self.settings.ui_theme_accent = previous.0;
            self.palette = previous.1;
            self.toast_error(tr_args!("settings.language.save_failed", error = error), cx);
            tracing::warn!(%error, "cannot save theme setting");
            cx.notify();
            return;
        }
        self.apply_palette(self.palette, cx);
    }

    pub(crate) fn set_max_fps(&mut self, fps: u32, cx: &mut Context<Self>) {
        if self.settings.ui_max_fps == fps {
            return;
        }
        self.settings.ui_max_fps = fps;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save max fps");
        }
        cx.notify();
    }

    pub(crate) fn set_card_size(&mut self, size: CardSize, cx: &mut Context<Self>) {
        if self.settings.ui_card_size == size {
            return;
        }
        self.capture_display_layout_anchors();
        self.settings.ui_card_size = size;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save card size");
        }
        cx.notify();
    }

    /// Remembers the top-left photo before a display change alters columns or
    /// row height. The next render of each grid restores that photo.
    pub(crate) fn capture_display_layout_anchors(&mut self) {
        let mut library = self.library.borrow_mut();
        if library.pending_anchor.is_none() {
            library.pending_anchor = anchor_photo_id(
                &library.index,
                list_scroll_top(&library.scroll),
                library.columns,
                library.row_height,
            );
        }
        drop(library);
        let mut search = self.search_view.borrow_mut();
        if search.pending_anchor.is_none() {
            search.pending_anchor = anchor_photo_id(
                &search.index,
                list_scroll_top(&search.scroll),
                search.columns,
                search.row_height,
            );
        }
    }

    /// Persists a thumbnail quality or size change. The in-memory images were
    /// generated with the previous spec, so drop them and let the grid request
    /// the new variant on its next frame.
    pub(crate) fn save_thumbnail_settings(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save thumbnail settings");
            self.toast_error(tr_args!("settings.language.save_failed", error = error), cx);
        }
        self.thumbnails.borrow_mut().clear();
        cx.notify();
    }

    pub(crate) fn set_thumbnail_size(&mut self, size: u32, cx: &mut Context<Self>) {
        if self.settings.thumbnail_size == size {
            return;
        }
        self.settings.thumbnail_size = size;
        self.save_thumbnail_settings(cx);
    }

    pub(crate) fn refresh_thumbnail_cache_usage(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = thumbnail::thumbnails_dir() else {
            self.thumbnail_cache_usage = Some(thumbnail::ThumbnailCacheUsage::default());
            return;
        };
        cx.spawn(async move |this, cx| {
            let usage = cx
                .background_executor()
                .spawn(async move { thumbnail::thumbnail_cache_usage_in(&dir).ok() })
                .await;
            this.update(cx, |this, cx| {
                this.thumbnail_cache_usage = usage;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn clear_thumbnail_cache(&mut self, cx: &mut Context<Self>) {
        if !self.ipc_connected {
            self.toast_error(tr!("settings.thumbnail.clear_daemon_required"), cx);
            cx.notify();
            return;
        }
        let Some(endpoint) = self.ipc_endpoint.clone() else {
            self.toast_error(tr!("daemon.ipc.resolve_error"), cx);
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let (result, usage) = cx
                .background_executor()
                .spawn(async move {
                    let result =
                        ipc_bridge::request(endpoint, vrc_photon_ipc::Request::ClearThumbnailCache);
                    let usage = thumbnail::thumbnail_cache_usage().ok();
                    (result, usage)
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Response::ThumbnailCacheCleared {
                        removed_files,
                        removed_bytes,
                        ..
                    }) => {
                        this.thumbnails.borrow_mut().clear();
                        this.thumbnail_cache_usage = usage;
                        this.toast_success(
                            tr_args!(
                                "settings.thumbnail.cleared",
                                files = removed_files,
                                size = format_bytes(removed_bytes)
                            ),
                            cx,
                        );
                    }
                    Ok(Response::Error { message, .. }) => {
                        this.toast_error(message, cx);
                    }
                    Ok(_) => {
                        this.toast_error(tr!("settings.thumbnail.clear_error"), cx);
                    }
                    Err(error) => {
                        this.toast_error(ipc_bridge::error_message(&error), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_reduce_motion(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings.ui_reduce_motion = enabled;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save reduce-motion setting");
        }
        cx.set_reduce_motion(enabled);
        cx.notify();
    }

    fn ai_plugin_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let view = AiPluginCardView::from(&self.ai);
        let state = view.state;
        let has_active = self.ai.active.is_some();
        let (status, status_fg, status_bg) = match state {
            AiPluginCardState::Installing => (
                tr!("settings.ai.status.installing"),
                p.accent,
                p.accent_soft,
            ),
            AiPluginCardState::NotInstalled => (
                tr!("settings.ai.status.not_installed"),
                p.accent,
                p.accent_soft,
            ),
            AiPluginCardState::Downloading => (
                tr!("settings.ai.status.downloading"),
                p.accent,
                p.accent_soft,
            ),
            AiPluginCardState::Verifying => {
                (tr!("settings.ai.status.verifying"), p.accent, p.accent_soft)
            }
            AiPluginCardState::Ready => (tr!("settings.ai.status.ready"), p.green, p.green_soft),
            AiPluginCardState::NeedsProbe => {
                (tr!("settings.ai.status.needs_probe"), p.amber, p.amber_soft)
            }
            AiPluginCardState::Error => (tr!("settings.ai.status.error"), p.red, p.red_soft),
            AiPluginCardState::ModelMissing => (
                tr!("settings.ai.status.model_missing"),
                p.amber,
                p.amber_soft,
            ),
        };

        let mut card = glass_panel(&self.palette)
            .flex()
            .flex_col()
            .p(px(14.))
            .gap(px(12.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("settings.ai.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("settings.ai.hint")),
                            ),
                    )
                    .child(self.badge(status, status_fg, status_bg)),
            );

        if self.ai.install_busy {
            return card.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted))
                    .child(tr!("settings.ai.installing")),
            );
        }

        if !has_active {
            card = card.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted))
                    .child(tr!("settings.ai.not_installed.body")),
            );
            if let Some(error) = self.ai.last_error.clone() {
                card = card.child(self.ai_install_error_box(&error));
            }
            return card
                .child(self.ai_primary_action_button(view.primary_action, false, cx))
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child(tr!("settings.ai.install_hint")),
                );
        }

        let install = self
            .ai
            .active
            .clone()
            .expect("the active installation was checked above");
        let manifest = ai_manifest_view(&install.manifest_json);
        let model_size = self
            .ai
            .state
            .as_ref()
            .and_then(|state| state.model_bytes_total)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .filter(|bytes| *bytes > 0)
            .or(manifest.model_size);
        let size_label = model_size
            .map(format_bytes)
            .unwrap_or_else(|| tr!("settings.ai.not_downloaded"));

        card = card.child(self.ai_install_meta(&install, &size_label));
        if !matches!(
            state,
            AiPluginCardState::Downloading | AiPluginCardState::Verifying
        ) {
            card = card.child(self.ai_variant_list(cx));
        }

        if matches!(
            state,
            AiPluginCardState::Downloading | AiPluginCardState::Verifying
        ) {
            if let Some(download) = self.ai.download.clone() {
                card = card.child(self.ai_download_box(&download));
            }
            card = card.child(
                h_flex().flex_wrap().items_center().gap(px(8.)).child(
                    Button::new("ai-cancel-download")
                        .label(tr!("settings.ai.cancel"))
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_ai_model_download(cx))),
                ),
            );
        } else if matches!(
            state,
            AiPluginCardState::Ready | AiPluginCardState::NeedsProbe
        ) {
            card = card.child(self.ai_model_path_box(&install, &manifest, &size_label));
            if state == AiPluginCardState::NeedsProbe {
                card = card.child(self.ai_warning_box(&tr!("settings.ai.needs_probe")));
            }
            card = card.child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(self.ai_primary_action_button(view.primary_action, false, cx))
                    .child(self.ai_action_button(
                        "ai-import-zip",
                        AiPluginAction::InstallZip,
                        tr!("settings.ai.install"),
                        false,
                        false,
                        cx,
                    ))
                    .child(self.ai_action_button(
                        "ai-delete-model",
                        AiPluginAction::DeleteModel,
                        tr!("settings.ai.delete_model"),
                        false,
                        false,
                        cx,
                    ))
                    .child(self.ai_action_button(
                        "ai-uninstall",
                        AiPluginAction::Uninstall,
                        tr!("settings.ai.uninstall"),
                        false,
                        true,
                        cx,
                    )),
            );
        } else if state == AiPluginCardState::Error {
            let code = self
                .ai
                .state
                .as_ref()
                .and_then(|state| state.model_error.clone())
                .or_else(|| self.ai.last_error.clone())
                .unwrap_or_else(|| "model_download_failed".to_string());
            let error = ai_error_text_in(&vrc_photon_i18n::current_locale(), &code, &code);
            card = card.child(self.ai_error_box(&error)).child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(self.ai_primary_action_button(view.primary_action, true, cx))
                    .child(self.ai_action_button(
                        "ai-error-import",
                        AiPluginAction::ImportModel,
                        tr!("settings.ai.import_model"),
                        false,
                        false,
                        cx,
                    ))
                    .child(self.ai_action_button(
                        "ai-error-recheck",
                        AiPluginAction::Recheck,
                        tr!("settings.ai.recheck"),
                        false,
                        false,
                        cx,
                    ))
                    .child(self.ai_action_button(
                        "ai-error-uninstall",
                        AiPluginAction::Uninstall,
                        tr!("settings.ai.uninstall"),
                        false,
                        true,
                        cx,
                    )),
            );
        } else {
            card = card
                .child(self.ai_model_box(&manifest, &size_label))
                .child(
                    h_flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .child(self.ai_primary_action_button(view.primary_action, false, cx))
                        .child(self.ai_action_button(
                            "ai-import-model",
                            AiPluginAction::ImportModel,
                            tr!("settings.ai.import_model"),
                            false,
                            false,
                            cx,
                        ))
                        .child(self.ai_action_button(
                            "ai-recheck",
                            AiPluginAction::Recheck,
                            tr!("settings.ai.recheck"),
                            false,
                            false,
                            cx,
                        ))
                        .child(self.ai_action_button(
                            "ai-import-zip",
                            AiPluginAction::InstallZip,
                            tr!("settings.ai.install"),
                            false,
                            false,
                            cx,
                        )),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child(tr!("settings.ai.download_note")),
                );
        }

        card
    }

    fn ai_install_meta(&self, install: &PluginInstallation, size_label: &str) -> impl IntoElement {
        v_flex()
            .gap(px(4.))
            .child(self.meta_row(tr!("settings.ai.version"), format!("v{}", install.version)))
            .child(self.meta_row(tr!("settings.ai.variant"), install.variant.clone()))
            .child(self.meta_row(
                tr!("settings.ai.backend"),
                variant_backend_label(&install.variant),
            ))
            .child(self.meta_row(tr!("settings.ai.model_size"), size_label.to_string()))
    }

    fn ai_variant_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let installations = self.ai.installations.clone();
        v_flex()
            .gap(px(6.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(self.section_label(tr!("settings.ai.variants")))
                    .child(self.badge(installations.len().to_string(), p.muted, p.panel_2)),
            )
            .child(
                v_flex()
                    .gap(px(4.))
                    .children(installations.into_iter().map(|install| {
                        let active = is_active_installation(&install);
                        let display_variant = install.variant.clone();
                        let switch_variant = install.variant.clone();
                        let version = install.version.clone();
                        let row_id = installation_row_id(&install);
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .p(px(8.))
                            .rounded(px(8.))
                            .bg(rgb(if active { p.accent_soft } else { p.panel_2 }))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .font_medium()
                                            .text_color(rgb(if active { p.accent } else { p.text }))
                                            .child(display_variant.clone()),
                                    )
                                    .child(
                                        div().text_size(px(9.)).text_color(rgb(p.muted_2)).child(
                                            format!(
                                                "v{version} · {}",
                                                variant_backend_label(&display_variant)
                                            ),
                                        ),
                                    ),
                            )
                            .child(if active {
                                self.badge(tr!("settings.ai.variant.active"), p.green, p.green_soft)
                                    .into_any_element()
                            } else {
                                Button::new(SharedString::from(row_id))
                                    .label(tr!("settings.ai.variant.switch"))
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_ai_active_variant(
                                            &switch_variant,
                                            Some(&version),
                                            cx,
                                        );
                                    }))
                                    .into_any_element()
                            })
                    })),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("settings.ai.variant.note")),
            )
    }

    fn ai_model_box(&self, manifest: &AiManifestView, size_label: &str) -> impl IntoElement {
        let p = self.palette;
        h_flex()
            .items_center()
            .gap(px(10.))
            .p(px(10.))
            .rounded(px(10.))
            .bg(rgb(p.panel_2))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div().text_size(px(11.)).text_color(rgb(p.text)).child(
                            manifest
                                .model_name
                                .clone()
                                .unwrap_or_else(|| tr!("settings.ai.model_unknown")),
                        ),
                    )
                    .child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(p.muted_2))
                            .child(format!(
                                "{} · {size_label}",
                                manifest
                                    .revision
                                    .clone()
                                    .unwrap_or_else(|| tr!("settings.ai.unknown"))
                            )),
                    ),
            )
            .child(self.badge(tr!("settings.ai.not_downloaded"), p.amber, p.amber_soft))
    }

    fn ai_model_path_box(
        &self,
        install: &PluginInstallation,
        manifest: &AiManifestView,
        size_label: &str,
    ) -> impl IntoElement {
        let p = self.palette;
        let models_dir = vrc_photon_core::data_dir()
            .map(|dir| dir.join("plugins").join(&install.plugin_id).join("models"));
        let path = match (models_dir, manifest.revision.clone()) {
            (Some(dir), Some(revision)) => dir.join(revision).display().to_string(),
            (Some(dir), None) => dir.display().to_string(),
            (None, Some(revision)) => revision,
            (None, None) => tr!("settings.ai.unknown"),
        };
        v_flex()
            .gap(px(4.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(self.section_label(tr!("settings.ai.model_path")))
                    .child(self.badge(size_label.to_string(), p.muted, p.panel_2)),
            )
            .child(
                div()
                    .p(px(8.))
                    .rounded(px(8.))
                    .bg(rgb(p.panel_2))
                    .text_size(px(10.))
                    .text_color(rgb(p.muted))
                    .truncate()
                    .child(path),
            )
    }

    fn ai_download_box(&self, download: &ModelDownloadView) -> impl IntoElement {
        let p = self.palette;
        let ratio = model_progress_ratio(download.bytes, download.total);
        let percent = (ratio * 100.0).round() as u32;
        let size = match download.total {
            Some(total) => format!("{} / {}", format_bytes(download.bytes), format_bytes(total)),
            None => format_bytes(download.bytes),
        };
        let status = if download.state == ModelState::Verifying {
            tr!("settings.ai.status.verifying")
        } else {
            tr!("settings.ai.status.downloading")
        };
        v_flex()
            .gap(px(6.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(status),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child(format!("{percent}%")),
                    ),
            )
            .child(status_progress_bar(&p, ratio))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(p.muted_2))
                    .child(size),
            )
    }

    fn ai_error_box(&self, text: &str) -> impl IntoElement {
        let p = self.palette;
        div()
            .p(px(10.))
            .rounded(px(8.))
            .bg(rgb(p.red_soft))
            .border_1()
            .border_color(rgb(p.red_border))
            .text_size(px(10.))
            .text_color(rgb(p.red))
            .child(text.to_string())
    }

    fn ai_install_error_box(&self, detail: &str) -> impl IntoElement {
        let p = self.palette;
        v_flex()
            .gap(px(2.))
            .p(px(10.))
            .rounded(px(8.))
            .bg(rgb(p.red_soft))
            .border_1()
            .border_color(rgb(p.red_border))
            .child(
                div()
                    .text_size(px(10.))
                    .font_medium()
                    .text_color(rgb(p.red))
                    .child(tr!("settings.ai.install_error")),
            )
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(p.muted))
                    .child(detail.to_string()),
            )
    }

    fn ai_warning_box(&self, text: &str) -> impl IntoElement {
        let p = self.palette;
        div()
            .p(px(10.))
            .rounded(px(8.))
            .bg(rgb(p.amber_soft))
            .border_1()
            .border_color(rgb(p.amber))
            .text_size(px(10.))
            .text_color(rgb(p.amber))
            .child(text.to_string())
    }

    fn ai_primary_action_button(
        &self,
        action: AiPluginAction,
        retry: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, danger) = match action {
            AiPluginAction::InstallZip => (tr!("settings.ai.install"), false),
            AiPluginAction::DownloadModel => (
                if retry {
                    tr!("settings.ai.retry")
                } else {
                    tr!("settings.ai.download")
                },
                false,
            ),
            AiPluginAction::ImportModel => (tr!("settings.ai.import_model"), false),
            AiPluginAction::Recheck => (tr!("settings.ai.recheck"), false),
            AiPluginAction::DeleteModel => (tr!("settings.ai.delete_model"), false),
            AiPluginAction::Uninstall => (tr!("settings.ai.uninstall"), true),
            AiPluginAction::None => return div().into_any_element(),
        };
        self.ai_action_button("ai-primary-action", action, label, true, danger, cx)
    }

    #[allow(clippy::too_many_arguments)]
    fn ai_action_button(
        &self,
        id: &'static str,
        action: AiPluginAction,
        label: String,
        primary: bool,
        danger: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut button = Button::new(id).label(label).small();
        if primary {
            button = button.primary();
        }
        if danger {
            button = button.danger();
        }
        button
            .on_click(cx.listener(move |this, _, _, cx| match action {
                AiPluginAction::InstallZip => this.install_ai_plugin(cx),
                AiPluginAction::DownloadModel => this.request_ai_model_download(cx),
                AiPluginAction::ImportModel => this.import_ai_model_files(cx),
                AiPluginAction::Recheck => this.probe_ai_plugin(cx),
                AiPluginAction::DeleteModel => this.delete_ai_model(cx),
                AiPluginAction::Uninstall => {
                    this.ai.uninstall_modal_open = true;
                    this.ai.uninstall_delete_tags = false;
                    cx.notify();
                }
                AiPluginAction::None => {}
            }))
            .into_any_element()
    }

    fn ai_license_row(&self, label: String, value: String) -> impl IntoElement {
        let p = self.palette;
        v_flex()
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(p.muted_2))
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.text))
                    .child(value),
            )
    }

    pub(crate) fn ai_license_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let manifest = self
            .ai
            .active
            .as_ref()
            .map(|install| ai_manifest_view(&install.manifest_json))
            .unwrap_or_default();
        let license = manifest
            .license
            .clone()
            .unwrap_or_else(|| tr!("settings.ai.license.unknown"));
        let license_url = manifest.license_url.clone();
        let license_fallback = manifest
            .license_file
            .clone()
            .or_else(|| license_url.clone())
            .unwrap_or_else(|| "—".to_string());
        let install_line = self
            .ai
            .active
            .as_ref()
            .map(|install| {
                format!(
                    "{} · {} · v{}",
                    install.plugin_id, install.variant, install.version
                )
            })
            .unwrap_or_default();

        let panel = dialog_panel(&self.palette)
            .id("ai-license-panel")
            .w(px(520.))
            .gap(px(14.))
            .p(px(20.))
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .child(
                v_flex()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child(tr!("settings.ai.license.title")),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(install_line),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(tr!("settings.ai.license.body")),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .p(px(12.))
                    .rounded(px(10.))
                    .bg(rgb(p.panel_2))
                    .child(self.ai_license_row(
                        tr!("settings.ai.license.metadata.license"),
                        license,
                    ))
                    .child(self.ai_license_row(
                        tr!("settings.ai.license.metadata.bundled"),
                        license_fallback,
                    ))
                    .when_some(license_url.clone(), |this, url| {
                        this.child(self.ai_license_row(
                            tr!("settings.ai.license.metadata.url"),
                            url,
                        ))
                    })
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("settings.ai.license.metadata.review")),
                    )
                    .when_some(license_url, |this, url| {
                        this.child(
                            Button::new("ai-license-open")
                                .label(tr!("settings.ai.license.open"))
                                .small()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Err(error) = open_path(Path::new(&url)) {
                                        tracing::warn!(%error, %url, "cannot open the model license");
                                        this.toast_error(
                                            tr!("settings.ai.license.open_failed"),
                                            cx,
                                        );
                                    }
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(9.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("settings.ai.license.consent_note")),
                    )
                    .child(
                        Button::new("ai-license-cancel")
                            .label(tr!("common.cancel"))
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_ai_license_modal(cx)
                            })),
                    )
                    .child(
                        Button::new("ai-license-agree")
                            .label(tr!("settings.ai.license.agree_download"))
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.accept_ai_model_license(cx)
                            })),
                    ),
            );

        div()
            .id("ai-license-scrim")
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
            .on_click(cx.listener(|this, _, _, cx| this.close_ai_license_modal(cx)))
            .child(panel)
            .into_any_element()
    }

    pub(crate) fn ai_uninstall_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let install = self.ai.active.clone();
        let manifest = install
            .as_ref()
            .map(|install| ai_manifest_view(&install.manifest_json))
            .unwrap_or_default();
        let size_label = self
            .ai
            .state
            .as_ref()
            .and_then(|state| state.model_bytes_total)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .filter(|bytes| *bytes > 0)
            .or(manifest.model_size)
            .map(format_bytes)
            .unwrap_or_else(|| "—".to_string());
        let install_line = install
            .as_ref()
            .map(|install| {
                format!(
                    "{} · {} · v{}",
                    install.plugin_id, install.variant, install.version
                )
            })
            .unwrap_or_default();
        let delete_tags = self.ai.uninstall_delete_tags;

        let panel = dialog_panel(&self.palette)
            .id("ai-uninstall-panel")
            .w(px(520.))
            .gap(px(14.))
            .p(px(20.))
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .child(
                v_flex()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child(tr!("settings.ai.uninstall.title")),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(install_line),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(tr!("settings.ai.uninstall.body")),
            )
            .child(self.meta_row(tr!("settings.ai.model_size"), size_label))
            .child(
                v_flex()
                    .gap(px(2.))
                    .child(
                        Checkbox::new("ai-uninstall-delete-tags")
                            .checked(delete_tags)
                            .label(tr!("settings.ai.uninstall.delete_tags"))
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.ai.uninstall_delete_tags = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("settings.ai.uninstall.delete_tags_hint")),
                    )
                    .when(!delete_tags, |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(tr!("settings.ai.uninstall.keep_note")),
                        )
                    }),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(9.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("settings.ai.uninstall.stopping")),
                    )
                    .child(
                        Button::new("ai-uninstall-cancel")
                            .label(tr!("common.cancel"))
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ai.uninstall_modal_open = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("ai-uninstall-confirm")
                            .label(tr!("settings.ai.uninstall.confirm"))
                            .small()
                            .danger()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let delete_tags = this.ai.uninstall_delete_tags;
                                this.ai.uninstall_modal_open = false;
                                this.uninstall_ai_plugin(delete_tags, cx);
                            })),
                    ),
            );

        div()
            .id("ai-uninstall-scrim")
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
            .on_click(cx.listener(|this, _, _, cx| {
                this.ai.uninstall_modal_open = false;
                cx.notify();
            }))
            .child(panel)
            .into_any_element()
    }
}
