//! Compression settings screen: profile cards, profile editor, and job queue.
//!
//! Profile and manual-compression state lives in `app::compression`; screen
//! dispatch stays in the crate root for PR1.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::model::{CompressionFormat, CompressionProfile, QueueStatus, ResizeMode};

use crate::app::App;
use crate::ui::components::glass_panel;
use crate::ui::theme::*;
use crate::{job_retry_available, job_retry_label, queue_status_badge};

impl App {
    pub(crate) fn compression_screen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let profile = self.profiles.get(self.profile_index).cloned();
        let profile_count = self.profiles.len();
        let message = self.message.clone();
        let header = self.screen_header(
            "圧縮設定",
            "プロファイルと画質を管理します。手動圧縮の出力先と元画像は実行時に選べます。"
                .to_string(),
            message,
            vec![
                Button::new("add-profile")
                    .label("＋ プロファイル")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let profile = CompressionProfile {
                            name: format!("新規プロファイル {}", this.profiles.len() + 1),
                            auto_enabled: false,
                            ..CompressionProfile::default()
                        };
                        if let Err(error) = this.db.save_profile(&profile) {
                            tracing::warn!(%error, "cannot add compression profile");
                            this.message = Some(format!("プロファイルを追加できません: {error}"));
                        }
                        this.profiles = this.db.list_profiles().unwrap_or_else(|error| {
                            tracing::warn!(%error, "cannot reload compression profiles");
                            Vec::new()
                        });
                        this.profile_index = this
                            .profiles
                            .iter()
                            .position(|item| item.id == profile.id)
                            .unwrap_or(0);
                        cx.notify();
                    }))
                    .into_any_element(),
                Button::new("manual-compress")
                    .label("手動圧縮")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let ids = if this.selected_ids.is_empty() {
                            this.selected.into_iter().collect()
                        } else {
                            this.selected_ids.clone()
                        };
                        this.open_manual_compression(ids, cx);
                    }))
                    .into_any_element(),
            ],
        );

        v_flex().flex_1().min_h_0().child(header).child(
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
                        .gap(px(24.))
                        .items_start()
                        .child(v_flex().w(px(280.)).gap(px(8.)).children(
                            self.profiles.iter().enumerate().map(|(index, profile)| {
                                let active = index == self.profile_index;
                                let auto = profile.auto_enabled;
                                let title = profile.name.clone();
                                let meta = profile_summary(profile);
                                glass_panel()
                                    .id(SharedString::from(format!("profile-{index}")))
                                    .flex()
                                    .flex_col()
                                    .p(px(14.))
                                    .gap(px(6.))
                                    .bg(rgb(if active { ACCENT_SOFT } else { PANEL }))
                                    .border_color(rgb(if active { BLUE } else { BORDER }))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.profile_index = index;
                                        cx.notify();
                                    }))
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_size(px(12.))
                                                    .font_semibold()
                                                    .text_color(rgb(if active {
                                                        BLUE
                                                    } else {
                                                        MUTED
                                                    }))
                                                    .child(title),
                                            )
                                            .child(self.badge(
                                                if auto { "自動" } else { "手動" },
                                                if auto { GREEN } else { MUTED },
                                                if auto { 0x112a22 } else { PANEL_2 },
                                            ))
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "profile-up-{index}"
                                                    )))
                                                    .w(px(20.))
                                                    .h(px(20.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(px(6.))
                                                    .text_size(px(11.))
                                                    .text_color(rgb(if index == 0 {
                                                        MUTED_2
                                                    } else {
                                                        MUTED
                                                    }))
                                                    .when(index > 0, |this| {
                                                        this.cursor_pointer()
                                                            .hover(|this| {
                                                                this.bg(rgb(PANEL_2))
                                                                    .text_color(rgb(TEXT))
                                                            })
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.move_profile(
                                                                        index, -1, cx,
                                                                    );
                                                                    cx.stop_propagation();
                                                                },
                                                            ))
                                                    })
                                                    .child("↑"),
                                            )
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "profile-down-{index}"
                                                    )))
                                                    .w(px(20.))
                                                    .h(px(20.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(px(6.))
                                                    .text_size(px(11.))
                                                    .text_color(rgb(
                                                        if index + 1 == profile_count {
                                                            MUTED_2
                                                        } else {
                                                            MUTED
                                                        },
                                                    ))
                                                    .when(index + 1 < profile_count, |this| {
                                                        this.cursor_pointer()
                                                            .hover(|this| {
                                                                this.bg(rgb(PANEL_2))
                                                                    .text_color(rgb(TEXT))
                                                            })
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.move_profile(index, 1, cx);
                                                                    cx.stop_propagation();
                                                                },
                                                            ))
                                                    })
                                                    .child("↓"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(if active { MUTED } else { MUTED_2 }))
                                            .child(meta),
                                    )
                                    .with_animation(
                                        SharedString::from(format!(
                                            "profile-card-{}-{active}",
                                            profile.id
                                        )),
                                        Animation::new(MOTION_FAST)
                                            .with_easing(ease_motion)
                                            .with_max_fps(MOTION_FPS),
                                        move |el, t| {
                                            let (bg_from, bg_to) = if active {
                                                (PANEL, ACCENT_SOFT)
                                            } else {
                                                (ACCENT_SOFT, PANEL)
                                            };
                                            let (border_from, border_to) = if active {
                                                (BORDER, BLUE)
                                            } else {
                                                (BLUE, BORDER)
                                            };
                                            el.bg(rgb(lerp_color(bg_from, bg_to, t))).border_color(
                                                rgb(lerp_color(border_from, border_to, t)),
                                            )
                                        },
                                    )
                            }),
                        ))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(px(24.))
                                .when_some(profile, |this, profile| {
                                    let profile_index = self.profile_index;
                                    this.child(
                                        v_flex()
                                            .child(self.profile_editor(&profile, window, cx))
                                            .with_animation(
                                                ("profile-editor", profile_index),
                                                Animation::new(MOTION_FAST)
                                                    .with_easing(ease_motion)
                                                    .with_max_fps(MOTION_FPS),
                                                |el, t| {
                                                    el.opacity(t)
                                                        .relative()
                                                        .top(px(4.0 * (1.0 - t)))
                                                },
                                            ),
                                    )
                                })
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .child(self.stat_card(
                                            "総圧縮数",
                                            self.stats.compressed_count.to_string(),
                                        ))
                                        .child(
                                            self.stat_card(
                                                "写真",
                                                self.stats.photo_count.to_string(),
                                            ),
                                        )
                                        .child(self.stat_card(
                                            "圧縮後合計",
                                            format_bytes(self.db.derivative_bytes().unwrap_or(0)),
                                        )),
                                ),
                        ),
                ),
        )
    }

    fn profile_editor(
        &self,
        profile: &CompressionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let profile_id = profile.id.clone();
        let suffix_value = self.profile_suffix_input.read(cx).value();
        if suffix_value.as_ref() != profile.suffix.as_str() {
            self.profile_suffix_input.update(cx, |input, cx| {
                input.set_value(profile.suffix.clone(), window, cx);
            });
        }
        glass_panel()
            .flex()
            .flex_col()
            .p(px(16.))
            .gap(px(14.))
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
                            .child(profile.name.clone()),
                    )
                    .child(self.badge(&format_label(&profile.format), BLUE, 0x14283d))
                    .child(div().flex_1())
                    .child(
                        Button::new("duplicate-profile")
                            .label("複製")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.duplicate_profile(cx))),
                    )
                    .child(
                        Button::new("delete-profile")
                            .label("削除")
                            .small()
                            .danger()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_delete_profile(&profile_id, window, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        v_flex()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT))
                                    .child("自動圧縮に使う"),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child("新規写真にこのプロファイルを適用します"),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        Switch::new("profile-auto")
                            .checked(profile.auto_enabled)
                            .on_click(cx.listener(move |this, checked, _, cx| {
                                this.edit_profile(|profile| profile.auto_enabled = *checked, cx);
                            })),
                    ),
            )
            .child(self.section_label("形式と画質"))
            .child(
                h_flex().gap(px(4.)).children(
                    [
                        CompressionFormat::Jpeg,
                        CompressionFormat::Webp,
                        CompressionFormat::Avif,
                        CompressionFormat::JpegXl,
                    ]
                    .into_iter()
                    .map(|format| {
                        let active = profile.format == format;
                        let label = format_label(&format);
                        let selected_format = format.clone();
                        div()
                            .id(SharedString::from(format!("format-{}", format.as_str())))
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(8.))
                            .bg(if active {
                                rgb(ACCENT_SOFT)
                            } else {
                                rgba(0x00000000)
                            })
                            .text_size(px(12.))
                            .font_medium()
                            .text_color(if active { rgb(BLUE) } else { rgb(MUTED) })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_profile(
                                    |profile| {
                                        apply_profile_format(profile, selected_format.clone())
                                    },
                                    cx,
                                );
                            }))
                            .child(label)
                    }),
                ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(80.))
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("ロスレス"),
                    )
                    .child(
                        Switch::new("profile-lossless")
                            .checked(profile.lossless)
                            .disabled(!lossless_supported(&profile.format))
                            .on_click(cx.listener(move |this, checked, _, cx| {
                                this.edit_profile(|profile| profile.lossless = *checked, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(lossless_hint(&profile.format)),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(80.))
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("品質"),
                    )
                    .child(
                        Button::new("quality-minus")
                            .label("−")
                            .small()
                            .disabled(profile.lossless)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_profile(
                                    |profile| {
                                        profile.quality = profile.quality.saturating_sub(1).max(1);
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(
                        div()
                            .w(px(36.))
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child(profile.quality.to_string()),
                    )
                    .child(
                        Button::new("quality-plus")
                            .label("＋")
                            .small()
                            .disabled(profile.lossless)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_profile(
                                    |profile| {
                                        profile.quality =
                                            profile.quality.saturating_add(1).min(100);
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(div().text_size(px(10.)).text_color(rgb(MUTED_2)).child(
                        if profile.lossless {
                            "ロスレスでは品質指定を使用しません"
                        } else {
                            "品質を調整"
                        },
                    )),
            )
            .child(self.section_label("リサイズ"))
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(MUTED))
                                    .child("リサイズを有効にする"),
                            )
                            .child(
                                Switch::new("profile-resize")
                                    .checked(profile.resize_enabled)
                                    .on_click(cx.listener(|this, checked, _, cx| {
                                        this.edit_profile(
                                            |profile| profile.resize_enabled = *checked,
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .w(px(96.))
                                        .flex_shrink_0()
                                        .text_size(px(12.))
                                        .text_color(rgb(MUTED))
                                        .child("解像度"),
                                )
                                .children(
                                    [
                                        ("HD", 1280u32, 720u32),
                                        ("FHD", 1920, 1080),
                                        ("2K", 2560, 1440),
                                        ("4K", 3840, 2160),
                                    ]
                                    .into_iter()
                                    .map(
                                        |(label, width, height)| {
                                            let active =
                                                profile.width == width && profile.height == height;
                                            div()
                                                .id(SharedString::from(format!("resize-{label}")))
                                                .px(px(8.))
                                                .py(px(4.))
                                                .rounded(px(8.))
                                                .bg(if active {
                                                    rgb(ACCENT_SOFT)
                                                } else {
                                                    rgba(0x00000000)
                                                })
                                                .text_size(px(10.))
                                                .font_medium()
                                                .text_color(if active {
                                                    rgb(BLUE)
                                                } else {
                                                    rgb(MUTED)
                                                })
                                                .cursor_pointer()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.edit_profile(
                                                        |profile| {
                                                            profile.resize_enabled = true;
                                                            profile.width = width;
                                                            profile.height = height;
                                                        },
                                                        cx,
                                                    );
                                                }))
                                                .child(label)
                                        },
                                    ),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED_2))
                                        .child(format!("{} × {}", profile.width, profile.height)),
                                ),
                        )
                    })
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            v_flex()
                                .gap(px(4.))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(96.))
                                                .flex_shrink_0()
                                                .text_size(px(12.))
                                                .text_color(rgb(MUTED))
                                                .child("リサイズ方法"),
                                        )
                                        .child(
                                            h_flex().gap(px(4.)).children(
                                                [
                                                    (ResizeMode::DownscaleOnly, "大きい場合のみ"),
                                                    (ResizeMode::Always, "毎回"),
                                                ]
                                                .into_iter()
                                                .map(|(mode, label)| {
                                                    let active = profile.resize_mode == mode;
                                                    div()
                                                        .id(SharedString::from(format!(
                                                            "resize-mode-{}",
                                                            mode.as_str()
                                                        )))
                                                        .px(px(8.))
                                                        .py(px(4.))
                                                        .rounded(px(8.))
                                                        .bg(if active {
                                                            rgb(ACCENT_SOFT)
                                                        } else {
                                                            rgba(0x00000000)
                                                        })
                                                        .text_size(px(10.))
                                                        .font_medium()
                                                        .text_color(if active {
                                                            rgb(BLUE)
                                                        } else {
                                                            rgb(MUTED)
                                                        })
                                                        .cursor_pointer()
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.edit_profile(
                                                                    |profile| {
                                                                        profile.resize_mode = mode;
                                                                    },
                                                                    cx,
                                                                );
                                                            },
                                                        ))
                                                        .child(label)
                                                }),
                                            ),
                                        ),
                                )
                                .child(
                                    div()
                                        .pl(px(104.))
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED_2))
                                        .child(match profile.resize_mode {
                                            ResizeMode::DownscaleOnly => {
                                                "指定解像度より大きい画像だけ縮小します"
                                            }
                                            ResizeMode::Always => {
                                                "小さい画像も指定解像度に合わせて拡大します"
                                            }
                                        }),
                                ),
                        )
                    })
                    .when(profile.resize_enabled, |this| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(96.))
                                        .flex_shrink_0()
                                        .text_size(px(12.))
                                        .text_color(rgb(MUTED_2))
                                        .child("アスペクト比維持"),
                                )
                                .child(
                                    Switch::new("profile-aspect")
                                        .checked(profile.maintain_aspect)
                                        .on_click(cx.listener(|this, checked, _, cx| {
                                            this.edit_profile(
                                                |profile| profile.maintain_aspect = *checked,
                                                cx,
                                            );
                                        })),
                                ),
                        )
                    }),
            )
            .child(self.section_label("出力"))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("出力先"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(
                                profile
                                    .output_dir
                                    .clone()
                                    .unwrap_or_else(|| "compressed/".to_string()),
                            ),
                    )
                    .child(
                        Button::new("profile-output-pick")
                            .label("フォルダを選択")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let current = this
                                    .profiles
                                    .get(this.profile_index)
                                    .and_then(|profile| profile.output_dir.clone());
                                let mut picker = rfd::FileDialog::new();
                                if let Some(dir) = current {
                                    picker = picker.set_directory(dir);
                                }
                                if let Some(dir) = picker.pick_folder() {
                                    let path = dir.to_string_lossy().to_string();
                                    this.edit_profile(
                                        move |profile| profile.output_dir = Some(path.clone()),
                                        cx,
                                    );
                                }
                            })),
                    )
                    .when(profile.output_dir.is_some(), |this| {
                        this.child(
                            Button::new("profile-output-clear")
                                .label("既定に戻す")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.edit_profile(|profile| profile.output_dir = None, cx);
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
                            .w(px(86.))
                            .flex_shrink_0()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("サフィックス"),
                    )
                    .child(Input::new(&self.profile_suffix_input).small().flex_1())
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child("空欄可"),
                    ),
            )
            .child(
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(rgb(PANEL_2))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child("変更は即時保存されます。サフィックスの使用不可文字は _ に置換されます。手動圧縮の出力先と元画像の扱いは実行時に選べます。"),
            )
    }

    pub(crate) fn queue_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let retryable_jobs = self.failed_jobs.iter().any(job_retry_available);
        glass_panel()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .child(
                h_flex()
                    .px(px(16.))
                    .py(px(14.))
                    .gap(px(8.))
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("圧縮キュー"),
                    )
                    .when(self.active_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("未完了 {}", self.active_job_count),
                            BLUE,
                            0x14283d,
                        ))
                    })
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("未実行 {}", self.waiting_job_count),
                            AMBER,
                            0x2a2412,
                        ))
                    })
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(if self.settings.auto_compress_keep_original {
                            self.badge("自動: 元画像を保持", GREEN, 0x112a22)
                        } else {
                            self.badge("自動: 元画像を置換", RED, 0x2a1414)
                        })
                    })
                    .when(self.failed_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("失敗 {}", self.failed_job_count),
                            RED,
                            0x2a1414,
                        ))
                    })
                    .child(div().flex_1())
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(
                            Button::new("release-waiting-jobs")
                                .label("待機をすべて圧縮")
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.request_release_waiting_jobs(window, cx)
                                })),
                        )
                    })
                    .when(retryable_jobs, |this| {
                        this.child(
                            Button::new("retry-all-failed-jobs")
                                .label("失敗をすべて再試行")
                                .small()
                                .ghost()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.retry_all_failed_jobs(cx)),
                                ),
                        )
                    }),
            )
            .when(self.failed_job_count > 100, |this| {
                this.child(
                    div()
                        .px(px(16.))
                        .py(px(8.))
                        .text_size(px(10.))
                        .text_color(rgb(MUTED_2))
                        .child("失敗は最新の100件のみ表示しています（再試行は上限内のすべてが対象です）"),
                )
            })
            .children(self.waiting_jobs.iter().take(20).map(|job| {
                let status = queue_status_badge(&job.status);
                let profile_name = self
                    .profiles
                    .iter()
                    .find(|profile| profile.id == job.profile_id)
                    .map(|profile| profile.name.clone())
                    .unwrap_or_else(|| "不明なプロファイル".to_string());
                h_flex()
                    .px(px(16.))
                    .py(px(10.))
                    .gap(px(8.))
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(self.badge(status.0, status.1, status.2))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .truncate()
                            .child(format!("photo #{}", job.photo_id)),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(profile_name),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED_2))
                            .child(format!("{:.0}%", job.progress * 100.0)),
                    )
            }))
            .children(
                self.jobs
                    .iter()
                    .filter(|job| !matches!(job.status, QueueStatus::Waiting | QueueStatus::Failed))
                    .take(6)
                    .map(|job| {
                        let status = queue_status_badge(&job.status);
                        let profile_name = self
                            .profiles
                            .iter()
                            .find(|profile| profile.id == job.profile_id)
                            .map(|profile| profile.name.clone())
                            .unwrap_or_else(|| "不明なプロファイル".to_string());
                        h_flex()
                            .px(px(16.))
                            .py(px(10.))
                            .gap(px(8.))
                            .items_center()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(self.badge(status.0, status.1, status.2))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT))
                                    .truncate()
                                    .child(format!("photo #{}", job.photo_id)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(profile_name),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(format!("{:.0}%", job.progress * 100.0)),
                            )
                    }),
            )
            .children(self.failed_jobs.iter().map(|job| {
                let name = self
                    .failed_job_names
                    .get(&job.photo_id)
                    .cloned()
                    .unwrap_or_else(|| format!("photo #{}", job.photo_id));
                let profile_name = self
                    .profiles
                    .iter()
                    .find(|profile| profile.id == job.profile_id)
                    .map(|profile| profile.name.clone());
                let error = job.error.clone().unwrap_or_default();
                let retryable = job_retry_available(job);
                let id = job.id.clone();
                v_flex()
                    .px(px(16.))
                    .py(px(10.))
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(self.badge("失敗", RED, 0x2a1414))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT))
                                    .truncate()
                                    .child(name),
                            )
                            .when_some(profile_name, |this, name| {
                                this.child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(MUTED_2))
                                        .child(name),
                                )
                            }),
                    )
                    .when(!error.is_empty(), |this| {
                        this.child(
                            div()
                                .w_full()
                                .text_size(px(10.))
                                .text_color(rgb(RED))
                                .child(error),
                        )
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED_2))
                                    .child(job_retry_label(job)),
                            )
                            .when(retryable, |this| {
                                this.child(
                                    Button::new(SharedString::from(format!(
                                        "retry-job-{}",
                                        job.id
                                    )))
                                    .label("再試行")
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_job(id.clone(), cx)
                                    })),
                                )
                            }),
                    )
            }))
            .when(
                self.waiting_jobs.is_empty()
                    && self.jobs.is_empty()
                    && self.failed_jobs.is_empty(),
                |this| {
                    this.child(
                        div()
                            .p(px(16.))
                            .text_size(px(12.))
                            .text_color(rgb(MUTED_2))
                            .child("キューは空です"),
                    )
                },
            )
    }
}
