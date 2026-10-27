//! Modal dialogs: the manual-compression confirm sheet.
//!
//! The confirmation state and queueing live in `app::compression`; this module
//! only renders the current dialog state.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::App;
use crate::app::compression::{ManualOutputTarget, manual_output_available};
use crate::ui::components::manual_option_row;
use crate::ui::theme::*;

impl App {
    pub(crate) fn manual_compression_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = self.manual_compress.clone() else {
            return div().into_any_element();
        };
        let count = dialog.photo_ids.len();
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
                    .unwrap_or_else(|| "プロファイル既定（元画像の隣の compressed/）".to_string())
            })
            .unwrap_or_else(|| "プロファイルを選択してください".to_string());
        let folder_ready = dialog.folder.is_some();
        let can_confirm = match dialog.output {
            ManualOutputTarget::Folder => folder_ready,
            ManualOutputTarget::Profile => selected_profile.is_some(),
            ManualOutputTarget::Clipboard => manual_output_available(count, dialog.output),
        };

        let profiles = self.profiles.clone();
        let profile_rows = profiles.into_iter().map(|profile| {
            let id = profile.id.clone();
            let selected = profile.id == dialog.profile_id;
            manual_option_row(
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
            "manual-output-folder",
            "指定したフォルダに出力",
            "実行のたびに出力先を選びます",
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
            "manual-output-profile",
            "プロファイル指定のフォルダに出力",
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

        let clipboard_enabled = manual_output_available(count, ManualOutputTarget::Clipboard);
        let clipboard_row = manual_option_row(
            "manual-output-clipboard",
            "クリップボードに出力",
            if clipboard_enabled {
                "PNG 1枚をクリップボードへコピーします（元画像は保持）"
            } else {
                "クリップボード出力は1枚選択時のみ使えます"
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
            "manual-keep-original",
            "元画像を保持",
            "圧縮版を別ファイルで出力します",
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
            "manual-trash-original",
            "元画像をゴミ箱へ移動",
            "圧縮に成功した場合だけOSゴミ箱へ移動します",
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
            .unwrap_or_else(|| "フォルダが選択されていません".to_string());
        let confirm_label = if dialog.output == ManualOutputTarget::Clipboard {
            "クリップボードへコピー".to_string()
        } else {
            format!("{count} 件を圧縮")
        };

        let panel = v_flex()
            .id("manual-compression-panel")
            .w(px(580.))
            .max_h(px(720.))
            .gap(px(16.))
            .p(px(22.))
            .rounded(px(18.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER_STRONG))
            .shadow(vec![
                BoxShadow::new(px(0.), px(24.), rgba(0x00000099).into()).blur_radius(px(60.)),
            ])
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .overflow_y_scrollbar()
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("手動圧縮"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED_2))
                            .child(format!("{count} 枚を対象")),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("使用するプロファイル、出力先、元画像の扱いを選んでから実行します。"),
            )
            .child(self.section_label("プロファイル"))
            .child(
                v_flex()
                    .id("manual-profile-list")
                    .max_h(px(190.))
                    .overflow_y_scrollbar()
                    .gap(px(6.))
                    .children(profile_rows),
            )
            .child(self.section_label("出力先"))
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
                                    .text_color(rgb(if folder_ready { MUTED } else { AMBER }))
                                    .child(folder_path),
                            )
                            .child(
                                Button::new("manual-choose-folder")
                                    .label("フォルダを選択")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_manual_folder(cx)
                                    })),
                            ),
                    )
                },
            ))
            .child(profile_row)
            .child(clipboard_row)
            .when(dialog.output != ManualOutputTarget::Clipboard, |this| {
                this.child(self.section_label("元画像"))
                    .child(v_flex().gap(px(6.)).child(keep_row).child(trash_row))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("「ゴミ箱へ移動」は圧縮ファイルの検証に成功した場合だけ実行します。"),
                    )
            })
            .when(dialog.output == ManualOutputTarget::Clipboard, |this| {
                this.child(
                    div()
                        .p(px(10.))
                        .rounded(px(8.))
                        .bg(rgb(PANEL_2))
                        .text_size(px(10.))
                        .text_color(rgb(MUTED))
                        .child("クリップボード出力では元画像を変更しません。形式はPNG、リサイズ設定のみ反映します。"),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("manual-cancel")
                            .label("キャンセル")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_manual_compression(cx)
                            })),
                    )
                    .child(
                        Button::new("manual-confirm")
                            .label(confirm_label)
                            .small()
                            .primary()
                            .disabled(!can_confirm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm_manual_compression(cx)
                            })),
                    ),
            );

        let panel = panel.with_animation(
            "manual-compression-panel",
            Animation::new(MOTION_BASE)
                .with_easing(ease_motion)
                .with_max_fps(MOTION_FPS),
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
            .bg(rgba(0x05070acc))
            .on_click(cx.listener(|this, _, _, cx| this.close_manual_compression(cx)))
            .child(panel)
            .with_animation(
                "manual-compression-scrim",
                Animation::new(MOTION_FAST)
                    .with_easing(ease_motion)
                    .with_max_fps(MOTION_FPS),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }
}
