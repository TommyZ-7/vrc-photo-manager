//! Compression settings screen: profile cards, profile editor, and job queue.
//!
//! Profile and manual-compression state lives in `app::compression`; screen
//! dispatch lives in `ui::shell`.

use std::path::Path;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photo_core::model::{
    CompressionFormat, CompressionJob, CompressionProfile, MAX_JOB_RETRIES, QueueStatus, ResizeMode,
};

use crate::app::App;
use crate::app::compression::MAX_PROFILE_NAME_CHARS;
use crate::ui::components::glass_panel;
use crate::ui::theme::*;

fn section_divider(palette: &Palette) -> AnyElement {
    let p = palette;
    div()
        .h(px(1.))
        .w_full()
        .bg(rgba(with_alpha(p.text, 0x0e)))
        .into_any_element()
}

fn queue_status_badge(status: &QueueStatus, p: &Palette) -> (&'static str, u32, u32) {
    match status {
        QueueStatus::Waiting => ("未実行", p.amber, p.amber_soft),
        QueueStatus::Pending => ("実行待ち", p.muted, p.panel_2),
        QueueStatus::Processing => ("処理中", p.accent, p.accent_soft),
        QueueStatus::Completed => ("完了", p.green, p.green_soft),
        QueueStatus::Failed => ("失敗", p.red, p.red_soft),
        QueueStatus::Cancelled => ("取消", p.muted, p.panel_2),
    }
}

/// Whether a failed job can return to the queue without exceeding the cap.
fn job_retry_available(job: &CompressionJob) -> bool {
    job.status == QueueStatus::Failed && job.retry_count < MAX_JOB_RETRIES
}

/// Retry usage for a failed job, or the cap explanation once exhausted.
fn job_retry_label(job: &CompressionJob) -> String {
    if job_retry_available(job) {
        format!("再試行 {}/{MAX_JOB_RETRIES}", job.retry_count)
    } else {
        format!("再試行の上限（{MAX_JOB_RETRIES}回）に達しました")
    }
}

impl App {
    pub(crate) fn compression_screen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.palette;
        let profile = self
            .profiles
            .get(self.profile_index)
            .or_else(|| self.profiles.first())
            .cloned();
        h_flex()
            .flex_1()
            .min_h_0()
            .gap(px(CARD_GAP))
            .items_stretch()
            .child(self.profile_list_card(cx))
            .child(match profile {
                Some(profile) => self.profile_editor(&profile, window, cx).into_any_element(),
                None => glass_panel(&self.palette)
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .rounded(px(17.))
                    .text_size(px(12.))
                    .text_color(rgb(p.muted_2))
                    .child("プロファイルがありません")
                    .into_any_element(),
            })
    }

    /// Left card: profile list header with the add action, then the selectable
    /// profile cards in a scroll body.
    fn profile_list_card(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let profile_count = self.profiles.len();
        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .w(px(COMPRESSION_PROFILE_WIDTH))
            .h_full()
            .min_h_0()
            .flex_shrink_0()
            .overflow_hidden()
            .rounded(px(17.))
            .child(self.profile_list_header(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .p(px(10.))
                    .gap(px(8.))
                    .children(self.profiles.iter().enumerate().map(|(index, profile)| {
                        let active = index == self.profile_index;
                        let auto = profile.auto_enabled;
                        let title = profile.name.clone();
                        let meta = profile_summary(profile);
                        glass_panel(&self.palette)
                            .id(SharedString::from(format!("profile-{index}")))
                            .flex()
                            .flex_col()
                            .p(px(12.))
                            .gap(px(6.))
                            .rounded(px(12.))
                            .bg(rgb(if active { p.accent_soft } else { p.panel_inner }))
                            .border_color(rgb(if active { p.accent } else { p.border }))
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
                                                p.accent
                                            } else {
                                                p.muted
                                            }))
                                            .child(title),
                                    )
                                    .child(self.badge(
                                        if auto { "自動" } else { "手動" },
                                        if auto { p.green } else { p.muted },
                                        if auto { p.green_soft } else { p.panel_2 },
                                    ))
                                    .when(profile.clipboard, |this| {
                                        this.child(self.badge(
                                            "クリップボード",
                                            p.accent,
                                            p.accent_soft,
                                        ))
                                    })
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("profile-up-{index}")))
                                            .w(px(20.))
                                            .h(px(20.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(6.))
                                            .text_size(px(11.))
                                            .text_color(rgb(if index == 0 {
                                                p.muted_2
                                            } else {
                                                p.muted
                                            }))
                                            .when(index > 0, |this| {
                                                this.cursor_pointer()
                                                    .hover(|this| {
                                                        this.bg(rgb(p.panel_2))
                                                            .text_color(rgb(p.text))
                                                    })
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.move_profile(index, -1, cx);
                                                        cx.stop_propagation();
                                                    }))
                                            })
                                            .child("↑"),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("profile-down-{index}")))
                                            .w(px(20.))
                                            .h(px(20.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(6.))
                                            .text_size(px(11.))
                                            .text_color(rgb(if index + 1 == profile_count {
                                                p.muted_2
                                            } else {
                                                p.muted
                                            }))
                                            .when(index + 1 < profile_count, |this| {
                                                this.cursor_pointer()
                                                    .hover(|this| {
                                                        this.bg(rgb(p.panel_2))
                                                            .text_color(rgb(p.text))
                                                    })
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.move_profile(index, 1, cx);
                                                        cx.stop_propagation();
                                                    }))
                                            })
                                            .child("↓"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(if active { p.text } else { p.muted_2 }))
                                    .child(meta),
                            )
                    })),
            )
            .into_any_element()
    }

    /// Header bar for the profile column. Rehomed from the deleted page
    /// header; the add action keeps its original save/reload path.
    fn profile_list_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        h_flex()
            .flex_shrink_0()
            .h(px(GALLERY_HEADER_HEIGHT))
            .px(px(GALLERY_PAD_X))
            .gap(px(8.))
            .items_center()
            .border_b_1()
            .border_color(rgba(p.highlight))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child("プロファイル"),
            )
            .child(
                Button::new("add-profile")
                    .label("＋ 追加")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let profile = CompressionProfile {
                            name: format!("新規プロファイル {}", this.profiles.len() + 1),
                            auto_enabled: false,
                            ..CompressionProfile::default()
                        };
                        if let Err(error) = this.db.save_profile(&profile) {
                            tracing::warn!(%error, "cannot add compression profile");
                            this.toast_error(format!("プロファイルを追加できません: {error}"), cx);
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
                    })),
            )
            .into_any_element()
    }

    fn profile_editor(
        &mut self,
        profile: &CompressionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if self.profile_name_editing_id.as_deref() != Some(profile.id.as_str())
            || self.profile_name_reset_pending
        {
            let name = profile.name.clone();
            self.profile_name_input.update(cx, |input, cx| {
                input.set_value(name, window, cx);
            });
            self.profile_name_editing_id = Some(profile.id.clone());
            self.profile_name_reset_pending = false;
            self.profile_name_error = None;
        }
        let name_error = self.profile_name_error.clone();
        let name_char_count = self
            .profile_name_input
            .read(cx)
            .value()
            .trim()
            .chars()
            .count();
        let p = self.palette;
        let profile_id = profile.id.clone();
        let clipboard_profile_id = profile.id.clone();
        let suffix_value = self.profile_suffix_input.read(cx).value();
        if suffix_value.as_ref() != profile.suffix.as_str() {
            self.profile_suffix_input.update(cx, |input, cx| {
                input.set_value(profile.suffix.clone(), window, cx);
            });
        }
        let output_value = self.profile_output_input.read(cx).value();
        let output_current = profile.output_dir.clone().unwrap_or_default();
        if output_value.as_ref() != output_current.as_str() {
            self.profile_output_input.update(cx, |input, cx| {
                input.set_value(output_current, window, cx);
            });
        }
        let output_hint = match profile.output_dir.as_deref() {
            None => "空欄: 元画像と同じフォルダの compressed/".to_string(),
            Some(dir) if Path::new(dir).is_absolute() => "絶対パス".to_string(),
            Some(_) => "相対パス: 元画像のフォルダ基準".to_string(),
        };
        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .rounded(px(17.))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h(px(GALLERY_HEADER_HEIGHT))
                    .px(px(GALLERY_PAD_X))
                    .gap(px(8.))
                    .items_center()
                    .border_b_1()
                    .border_color(rgba(p.highlight))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child(profile.name.clone()),
                    )
                    .child(self.badge(&format_label(&profile.format), p.accent, p.accent_soft))
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
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .p(px(14.))
                    .gap(px(14.))
                    .child(self.section_label("基本"))
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child("プロファイル名"),
                            )
                            .child(Input::new(&self.profile_name_input).small())
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .text_color(rgb(if name_error.is_some() {
                                        p.red
                                    } else {
                                        p.muted_2
                                    }))
                                    .child(match name_error {
                                        Some(error) => error,
                                        None => format!(
                                            "前後の空白は保存時に除去されます · {name_char_count}/{MAX_PROFILE_NAME_CHARS}文字"
                                        ),
                                    }),
                            ),
                    )
                    .child(section_divider(&p))
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
                                            .text_color(rgb(p.text))
                                            .child("自動圧縮に使う"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted_2))
                                            .child("新規写真にこのプロファイルを適用します"),
                                    ),
                            )
                            .child(div().flex_1())
                            .child(
                                Switch::new("profile-auto")
                                    .checked(profile.auto_enabled)
                                    .on_click(cx.listener(move |this, checked, _, cx| {
                                        this.edit_profile(
                                            |profile| profile.auto_enabled = *checked,
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .child(section_divider(&p))
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
                                    .id(SharedString::from(format!(
                                        "format-{}",
                                        format.as_str()
                                    )))
                                    .px(px(10.))
                                    .py(px(5.))
                                    .rounded(px(8.))
                                    .bg(if active {
                                        rgb(p.accent_soft)
                                    } else {
                                        rgba(0x00000000)
                                    })
                                    .text_size(px(12.))
                                    .font_medium()
                                    .text_color(if active { rgb(p.accent) } else { rgb(p.muted) })
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.edit_profile(
                                            |profile| {
                                                apply_profile_format(
                                                    profile,
                                                    selected_format.clone(),
                                                )
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
                                    .text_color(rgb(p.muted))
                                    .child("ロスレス"),
                            )
                            .child(
                                Switch::new("profile-lossless")
                                    .checked(profile.lossless)
                                    .disabled(!lossless_supported(&profile.format))
                                    .on_click(cx.listener(move |this, checked, _, cx| {
                                        this.edit_profile(
                                            |profile| profile.lossless = *checked,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
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
                                    .text_color(rgb(p.muted))
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
                                                profile.quality =
                                                    profile.quality.saturating_sub(1).max(1);
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
                                    .text_color(rgb(p.text))
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
                            .child(div().text_size(px(10.)).text_color(rgb(p.muted_2)).child(
                                if profile.lossless {
                                    "ロスレスでは品質指定を使用しません"
                                } else {
                                    "品質を調整"
                                },
                            )),
                    )
                    .child(section_divider(&p))
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
                                            .text_color(rgb(p.muted))
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
                                                .text_color(rgb(p.muted))
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
                                            .map(|(label, width, height)| {
                                                let active = profile.width == width
                                                    && profile.height == height;
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "resize-{label}"
                                                    )))
                                                    .px(px(8.))
                                                    .py(px(4.))
                                                    .rounded(px(8.))
                                                    .bg(if active {
                                                        rgb(p.accent_soft)
                                                    } else {
                                                        rgba(0x00000000)
                                                    })
                                                    .text_size(px(10.))
                                                    .font_medium()
                                                    .text_color(if active {
                                                        rgb(p.accent)
                                                    } else {
                                                        rgb(p.muted)
                                                    })
                                                    .cursor_pointer()
                                                    .on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            this.edit_profile(
                                                                |profile| {
                                                                    profile.resize_enabled = true;
                                                                    profile.width = width;
                                                                    profile.height = height;
                                                                },
                                                                cx,
                                                            );
                                                        },
                                                    ))
                                                    .child(label)
                                            }),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.))
                                                .text_color(rgb(p.muted_2))
                                                .child(format!(
                                                    "{} × {}",
                                                    profile.width, profile.height
                                                )),
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
                                                        .text_color(rgb(p.muted))
                                                        .child("リサイズ方法"),
                                                )
                                                .child(
                                                    h_flex().gap(px(4.)).children(
                                                        [
                                                            (
                                                                ResizeMode::DownscaleOnly,
                                                                "大きい場合のみ",
                                                            ),
                                                            (ResizeMode::Always, "毎回"),
                                                        ]
                                                        .into_iter()
                                                        .map(|(mode, label)| {
                                                            let active =
                                                                profile.resize_mode == mode;
                                                            div()
                                                                .id(SharedString::from(format!(
                                                                    "resize-mode-{}",
                                                                    mode.as_str()
                                                                )))
                                                                .px(px(8.))
                                                                .py(px(4.))
                                                                .rounded(px(8.))
                                                                .bg(if active {
                                                                    rgb(p.accent_soft)
                                                                } else {
                                                                    rgba(0x00000000)
                                                                })
                                                                .text_size(px(10.))
                                                                .font_medium()
                                                                .text_color(if active {
                                                                    rgb(p.accent)
                                                                } else {
                                                                    rgb(p.muted)
                                                                })
                                                                .cursor_pointer()
                                                                .on_click(cx.listener(
                                                                    move |this, _, _, cx| {
                                                                        this.edit_profile(
                                                                            |profile| {
                                                                                profile.resize_mode =
                                                                                    mode;
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
                                                .text_color(rgb(p.muted_2))
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
                                                .text_color(rgb(p.muted_2))
                                                .child("アスペクト比維持"),
                                        )
                                        .child(
                                            Switch::new("profile-aspect")
                                                .checked(profile.maintain_aspect)
                                                .on_click(cx.listener(|this, checked, _, cx| {
                                                    this.edit_profile(
                                                        |profile| {
                                                            profile.maintain_aspect = *checked
                                                        },
                                                        cx,
                                                    );
                                                })),
                                        ),
                                )
                            }),
                    )
                    .child(section_divider(&p))
                    .child(self.section_label("出力"))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(12.))
                            .items_start()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w(px(300.))
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted_2))
                                            .child("出力先"),
                                    )
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                Input::new(&self.profile_output_input)
                                                    .small()
                                                    .flex_1(),
                                            )
                                            .child(
                                                Button::new("profile-output-pick")
                                                    .label("参照")
                                                    .small()
                                                    .ghost()
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            let current = this
                                                                .profiles
                                                                .get(this.profile_index)
                                                                .and_then(|profile| {
                                                                    profile.output_dir.clone()
                                                                });
                                                            let mut picker = rfd::FileDialog::new();
                                                            if let Some(dir) = current {
                                                                picker = picker.set_directory(dir);
                                                            }
                                                            if let Some(dir) = picker.pick_folder() {
                                                                let path = dir
                                                                    .to_string_lossy()
                                                                    .to_string();
                                                                this.profile_output_input.update(
                                                                    cx,
                                                                    |input, cx| {
                                                                        input.set_value(
                                                                            path.clone(),
                                                                            window,
                                                                            cx,
                                                                        )
                                                                    },
                                                                );
                                                                this.edit_profile(
                                                                    move |profile| {
                                                                        profile.output_dir =
                                                                            Some(path.clone())
                                                                    },
                                                                    cx,
                                                                );
                                                            }
                                                        },
                                                    )),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(9.5))
                                            .text_color(rgb(p.muted_2))
                                            .child(output_hint),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w(px(240.))
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted_2))
                                            .child("サフィックス"),
                                    )
                                    .child(Input::new(&self.profile_suffix_input).small().flex_1())
                                    .child(
                                        div()
                                            .text_size(px(9.5))
                                            .text_color(rgb(p.muted_2))
                                            .child("空欄可"),
                                    ),
                            ),
                    )
                    .child(section_divider(&p))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(p.text))
                                            .child("クリップボード用"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted_2))
                                            .child("システム全体で1つだけ設定できます"),
                                    ),
                            )
                            .child(
                                Switch::new("profile-clipboard")
                                    .checked(profile.clipboard)
                                    .on_click(cx.listener(move |this, checked, _, cx| {
                                        this.set_clipboard_profile(&clipboard_profile_id, *checked, cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .p(px(12.))
                            .rounded(px(8.))
                            .bg(rgb(p.panel_2))
                            .border_1()
                            .border_color(rgb(p.border))
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child("変更は即時保存されます。サフィックスの使用不可文字は _ に置換されます。手動圧縮の出力先と元画像の扱いは実行時に選べます。"),
                    ),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap(px(12.))
                    .p(px(12.))
                    .border_t_1()
                    .border_color(rgba(p.highlight))
                    .child(self.editor_stat(
                        "総圧縮数",
                        self.stats.compressed_count.to_string(),
                    ))
                    .child(self.editor_stat("写真", self.stats.photo_count.to_string()))
                    .child(self.editor_stat(
                        "圧縮後合計",
                        format_bytes(self.stats.derivative_bytes),
                    )),
            )
    }

    /// Small stat block inside the editor card's footer.
    fn editor_stat(&self, label: &str, value: String) -> impl IntoElement {
        let p = self.palette;
        v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(4.))
            .p(px(10.))
            .rounded(px(12.))
            .bg(rgb(p.panel_inner))
            .border_1()
            .border_color(rgba(with_alpha(p.text, 0x0f)))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_size(px(16.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .truncate()
                    .child(value),
            )
    }

    pub(crate) fn queue_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let retryable_jobs = self.failed_jobs.iter().any(job_retry_available);
        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .rounded(px(17.))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .min_h(px(GALLERY_HEADER_HEIGHT))
                    .px(px(GALLERY_PAD_X))
                    .py(px(8.))
                    .gap(px(8.))
                    .flex_wrap()
                    .items_center()
                    .border_b_1()
                    .border_color(rgba(p.highlight))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .text_color(rgb(p.text))
                            .child("圧縮キュー"),
                    )
                    .when(self.active_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("未完了 {}", self.active_job_count),
                            p.accent,
                            p.accent_soft,
                        ))
                    })
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("未実行 {}", self.waiting_job_count),
                            p.amber,
                            p.amber_soft,
                        ))
                    })
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(if self.settings.auto_compress_keep_original {
                            self.badge("自動: 元画像を保持", p.green, p.green_soft)
                        } else {
                            self.badge("自動: 元画像を置換", p.red, p.red_soft)
                        })
                    })
                    .when(self.failed_job_count > 0, |this| {
                        this.child(self.badge(
                            &format!("失敗 {}", self.failed_job_count),
                            p.red,
                            p.red_soft,
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
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .when(self.failed_job_count > 100, |this| {
                this.child(
                    div()
                        .px(px(GALLERY_PAD_X))
                        .py(px(8.))
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child("失敗は最新の100件のみ表示しています（再試行は上限内のすべてが対象です）"),
                )
            })
            .children(self.waiting_jobs.iter().take(20).map(|job| {
                let status = queue_status_badge(&job.status, &p);
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
                    .border_color(rgb(p.border))
                    .child(self.badge(status.0, status.1, status.2))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(rgb(p.text))
                            .truncate()
                            .child(format!("photo #{}", job.photo_id)),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(profile_name),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(format!("{:.0}%", job.progress * 100.0)),
                    )
            }))
            .children(
                self.jobs
                    .iter()
                    .filter(|job| !matches!(job.status, QueueStatus::Waiting | QueueStatus::Failed))
                    .take(6)
                    .map(|job| {
                        let status = queue_status_badge(&job.status, &p);
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
                            .border_color(rgb(p.border))
                            .child(self.badge(status.0, status.1, status.2))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(p.text))
                                    .truncate()
                                    .child(format!("photo #{}", job.photo_id)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(profile_name),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
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
                    .border_color(rgb(p.border))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(self.badge("失敗", p.red, p.red_soft))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(p.text))
                                    .truncate()
                                    .child(name),
                            )
                            .when_some(profile_name, |this, name| {
                                this.child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(p.muted_2))
                                        .child(name),
                                )
                            }),
                    )
                    .when(!error.is_empty(), |this| {
                        this.child(
                            div()
                                .w_full()
                                .text_size(px(10.))
                                .text_color(rgb(p.red))
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
                                    .text_color(rgb(p.muted_2))
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
                            .p(px(GALLERY_PAD_X))
                            .text_size(px(12.))
                            .text_color(rgb(p.muted_2))
                            .child("キューは空です"),
                    )
                },
            ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{job_retry_available, job_retry_label, queue_status_badge};
    use vrc_photo_core::model::{CompressionJob, QueueStatus};

    fn test_job(status: QueueStatus, retry_count: i64) -> CompressionJob {
        CompressionJob {
            id: "job".to_string(),
            photo_id: 1,
            profile_id: "p".to_string(),
            status,
            progress: 0.0,
            error: Some("boom".to_string()),
            output_path: None,
            policy: "keep".to_string(),
            output_dir: None,
            created_at: "now".to_string(),
            started_at: None,
            finished_at: None,
            retry_count,
        }
    }

    #[test]
    fn job_retry_availability_stops_at_the_cap() {
        assert!(job_retry_available(&test_job(QueueStatus::Failed, 0)));
        assert!(job_retry_available(&test_job(QueueStatus::Failed, 2)));
        assert!(!job_retry_available(&test_job(QueueStatus::Failed, 3)));
        assert!(!job_retry_available(&test_job(QueueStatus::Completed, 0)));
        assert!(job_retry_label(&test_job(QueueStatus::Failed, 3)).contains("上限"));
    }

    #[test]
    fn queue_status_badge_distinguishes_waiting_and_pending() {
        let p = crate::ui::palette::resolve("charcoal", "sky");
        assert_eq!(queue_status_badge(&QueueStatus::Waiting, &p).0, "未実行");
        assert_eq!(queue_status_badge(&QueueStatus::Pending, &p).0, "実行待ち");
        assert_eq!(queue_status_badge(&QueueStatus::Processing, &p).0, "処理中");
        assert_eq!(queue_status_badge(&QueueStatus::Completed, &p).0, "完了");
        assert_eq!(queue_status_badge(&QueueStatus::Failed, &p).0, "失敗");
        assert_eq!(queue_status_badge(&QueueStatus::Cancelled, &p).0, "取消");
    }
}
