//! Modal dialogs: the manual-compression confirm sheet.
//!
//! The confirmation state and queueing live in `app::compression`; this module
//! only renders the current dialog state.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::compression::{ManualOutputTarget, manual_output_available};
use crate::ui::components::{dialog_panel, manual_option_row};
use crate::ui::theme::*;

impl App {
    pub(crate) fn manual_compression_overlay(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let Some(dialog) = self.manual_compress.clone() else {
            return div().into_any_element();
        };
        let count = dialog.photo_ids.len();
        let content_height = f32::from(content_viewport(window).height);
        let max_height = dialog_max_height(content_height);
        let selected_profile = self
            .profiles
            .iter()
            .find(|profile| profile.id == dialog.profile_id)
            .cloned();
        let profile_description = selected_profile
            .as_ref()
            .map(|profile| {
                profile
                    .output_dir
                    .clone()
                    .filter(|dir| !dir.trim().is_empty())
                    .unwrap_or_else(|| tr!("dialog.output.profile_default"))
            })
            .unwrap_or_else(|| tr!("dialog.profile.select"));
        let folder_ready = dialog.folder.is_some();
        let can_confirm = match dialog.output {
            ManualOutputTarget::Folder => folder_ready,
            ManualOutputTarget::Profile => selected_profile.is_some(),
            ManualOutputTarget::Clipboard => {
                manual_output_available(count, dialog.output, self.settings.clipboard_enabled)
            }
        };

        let profiles = self.profiles.clone();
        let profile_rows = profiles.into_iter().map(|profile| {
            let id = profile.id.clone();
            let selected = profile.id == dialog.profile_id;
            manual_option_row(
                &self.palette,
                SharedString::from(format!("manual-profile-{}", profile.id)),
                profile.name.clone(),
                profile_summary(&profile),
                selected,
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(dialog) = &mut this.manual_compress {
                    dialog.profile_id = id.clone();
                }
                cx.notify();
            }))
        });

        let folder_row = manual_option_row(
            &self.palette,
            "manual-output-folder",
            tr!("dialog.output.folder"),
            tr!("dialog.output.folder_hint"),
            dialog.output == ManualOutputTarget::Folder,
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(dialog) = &mut this.manual_compress {
                dialog.output = ManualOutputTarget::Folder;
            }
            cx.notify();
        }));

        let profile_row = manual_option_row(
            &self.palette,
            "manual-output-profile",
            tr!("dialog.output.profile_folder"),
            profile_description,
            dialog.output == ManualOutputTarget::Profile,
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(dialog) = &mut this.manual_compress {
                dialog.output = ManualOutputTarget::Profile;
            }
            cx.notify();
        }));

        let clipboard_enabled = manual_output_available(
            count,
            ManualOutputTarget::Clipboard,
            self.settings.clipboard_enabled,
        );
        let clipboard_row = manual_option_row(
            &self.palette,
            "manual-output-clipboard",
            tr!("dialog.output.clipboard"),
            if !self.settings.clipboard_enabled {
                tr!("dialog.output.clipboard_disabled")
            } else if clipboard_enabled {
                tr!("dialog.output.clipboard_hint")
            } else {
                tr!("dialog.output.clipboard_single")
            },
            dialog.output == ManualOutputTarget::Clipboard,
            clipboard_enabled,
        )
        .when(clipboard_enabled, |this| {
            this.on_click(cx.listener(|this, _, _, cx| {
                if let Some(dialog) = &mut this.manual_compress {
                    dialog.output = ManualOutputTarget::Clipboard;
                }
                cx.notify();
            }))
        });

        let keep_row = manual_option_row(
            &self.palette,
            "manual-keep-original",
            tr!("dialog.keep_original"),
            tr!("dialog.keep_original_hint"),
            dialog.keep_original,
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(dialog) = &mut this.manual_compress {
                dialog.keep_original = true;
            }
            cx.notify();
        }));
        let trash_row = manual_option_row(
            &self.palette,
            "manual-trash-original",
            tr!("dialog.trash_original"),
            tr!("dialog.trash_original_hint"),
            !dialog.keep_original,
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(dialog) = &mut this.manual_compress {
                dialog.keep_original = false;
            }
            cx.notify();
        }));

        let folder_path = dialog
            .folder
            .clone()
            .unwrap_or_else(|| tr!("dialog.folder.not_selected"));
        let confirm_label = if dialog.output == ManualOutputTarget::Clipboard {
            tr!("dialog.output.clipboard_target")
        } else {
            tr_args!("dialog.compress_count", count = count)
        };

        let header = v_flex()
            .gap(px(16.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child(tr!("dialog.title")),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(p.muted_2))
                            .child(tr_args!("dialog.target_count", count = count)),
                    ),
            )
            .child(div().text_size(px(11.)).text_color(rgb(p.muted)).child(
                if dialog.hide_output {
                    tr!("dialog.description")
                } else {
                    tr!("dialog.description_no_profile")
                },
            ));

        let body = v_flex()
            .flex_1()
            .min_h_0()
            .gap(px(16.))
            .overflow_y_scrollbar()
            .child(self.section_label(tr!("dialog.section.profile")))
            .child(
                v_flex()
                    .id("manual-profile-list")
                    .max_h(px(190.))
                    .overflow_y_scrollbar()
                    .gap(px(6.))
                    .children(profile_rows),
            )
            .when(!dialog.hide_output, |this| {
                this.child(self.section_label(tr!("dialog.section.output")))
                    .child(v_flex().gap(px(6.)).child(folder_row).when(
                        dialog.output == ManualOutputTarget::Folder,
                        |this| {
                            this.child(
                                h_flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .pl(px(24.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(10.))
                                            .text_color(rgb(if folder_ready {
                                                p.muted
                                            } else {
                                                p.amber
                                            }))
                                            .child(folder_path),
                                    )
                                    .child(
                                        Button::new("manual-choose-folder")
                                            .label(tr!("dialog.choose_folder"))
                                            .small()
                                            .ghost()
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.choose_manual_folder(window, cx)
                                            })),
                                    ),
                            )
                        },
                    ))
                    .child(profile_row)
                    .child(clipboard_row)
            })
            .when(dialog.output != ManualOutputTarget::Clipboard, |this| {
                this.child(self.section_label(tr!("dialog.section.original")))
                    .child(v_flex().gap(px(6.)).child(keep_row).child(trash_row))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("dialog.trash_note")),
                    )
            })
            .when(dialog.output == ManualOutputTarget::Clipboard, |this| {
                this.child(
                    div()
                        .p(px(10.))
                        .rounded(px(8.))
                        .bg(rgb(p.panel_2))
                        .text_size(px(10.))
                        .text_color(rgb(p.muted))
                        .child(tr!("dialog.clipboard_note")),
                )
            });

        let footer = h_flex()
            .flex_shrink_0()
            .justify_end()
            .gap(px(8.))
            .child(
                Button::new("manual-cancel")
                    .label(tr!("common.cancel"))
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.close_manual_compression(cx))),
            )
            .child(
                Button::new("manual-confirm")
                    .label(confirm_label)
                    .small()
                    .primary()
                    .disabled(!can_confirm)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_manual_compression(cx))),
            );

        let panel = dialog_panel(&self.palette)
            .id("manual-compression-panel")
            .w(px(580.))
            .h(px(max_height))
            .gap(px(16.))
            .p(px(22.))
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .child(header)
            .child(body)
            .child(footer);

        let panel = panel.with_animation(
            "manual-compression-panel",
            Animation::new(MOTION_BASE)
                .with_easing(ease_motion)
                .with_max_fps(self.motion_fps(window, cx)),
            |el, t| el.relative().top(px(10.0 * (1.0 - t))),
        );
        div()
            .id("manual-compression-scrim")
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
            .on_click(cx.listener(|this, _, _, cx| this.close_manual_compression(cx)))
            .child(panel)
            .with_animation(
                "manual-compression-scrim",
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(self.motion_fps(window, cx)),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }
}
