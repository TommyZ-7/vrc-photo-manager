//! Compression settings screen: profile cards, profile editor, and job queue.
//!
//! Profile and manual-compression state lives in `app::compression`; screen
//! dispatch lives in `ui::shell`.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::base::v_virtual_list;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::{ScrollableElement as _, Scrollbar};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::{Size as VirtualRowSize, size as virtual_row_size};
use vrc_photon_core::model::{
    CompressionFormat, CompressionJob, CompressionProfile, MAX_JOB_RETRIES, MAX_PROFILE_NAME_CHARS,
    QueueStatus, ResizeMode,
};
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::file_picker::{self, DialogOptions};
use crate::ui::components::{card_header, glass_panel};
use crate::ui::theme::*;

fn section_divider(palette: &Palette) -> AnyElement {
    let p = palette;
    div()
        .h(px(1.))
        .w_full()
        .bg(rgba(with_alpha(p.text, 0x0e)))
        .into_any_element()
}

fn queue_status_badge(status: &QueueStatus, p: &Palette) -> (String, u32, u32) {
    match status {
        QueueStatus::Waiting => (tr!("compression.status.waiting"), p.amber, p.amber_soft),
        QueueStatus::Pending => (tr!("compression.status.pending"), p.muted, p.panel_2),
        QueueStatus::Processing => (
            tr!("compression.status.processing"),
            p.accent,
            p.accent_soft,
        ),
        QueueStatus::Completed => (tr!("compression.status.completed"), p.green, p.green_soft),
        QueueStatus::Failed => (tr!("compression.status.failed"), p.red, p.red_soft),
        QueueStatus::Cancelled => (tr!("compression.status.cancelled"), p.muted, p.panel_2),
        QueueStatus::Unknown => (tr!("compression.status.unknown"), p.muted, p.panel_2),
    }
}

/// Whether a failed job can return to the queue without exceeding the cap.
pub(crate) fn job_retry_available(job: &CompressionJob) -> bool {
    job.status == QueueStatus::Failed && job.retry_count < MAX_JOB_RETRIES
}

/// The failure reason shown on a failed job's row. A job that failed
/// without a recorded reason still gets a line, so the row never reads
/// as if nothing went wrong.
fn job_error_label(job: &CompressionJob) -> String {
    job.error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| tr!("compression.job.unknown_error"))
}

/// Retry usage for a failed job, or the cap explanation once exhausted.
fn job_retry_label(job: &CompressionJob) -> String {
    if job_retry_available(job) {
        tr_args!(
            "compression.retry_label",
            count = job.retry_count,
            max = MAX_JOB_RETRIES
        )
    } else {
        tr_args!("compression.retry_limit", max = MAX_JOB_RETRIES)
    }
}

/// One row of the virtualized queue. Waiting jobs are capped at 20; every
/// loaded failed job gets a row after them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueRow {
    FailedHint,
    Waiting(usize),
    Failed(usize),
    Empty,
}

/// Row heights preserve the previous natural heights (padding + one line at
/// the default golden-ratio line height, plus the bottom border on job rows).
/// The failed error line is truncated to one line so the list can pass fixed
/// sizes without measuring text; the old widget let long errors wrap and grow
/// the row.
const QUEUE_HINT_ROW_HEIGHT: f32 = 32.0;
const QUEUE_WAITING_ROW_HEIGHT: f32 = 41.0;
const QUEUE_FAILED_ROW_HEIGHT: f32 = 93.0;
const QUEUE_FAILED_ROW_HEIGHT_NO_RETRY: f32 = 85.0;
const QUEUE_EMPTY_ROW_HEIGHT: f32 = 43.0;

/// Builds the queue's display order: failed-count hint, waiting rows (max
/// 20), then failed rows, with the empty state last when the queue is empty.
pub(crate) fn queue_rows(
    waiting_count: usize,
    failed_count: usize,
    failed_job_count: i64,
    is_empty: bool,
) -> Vec<QueueRow> {
    let waiting = waiting_count.min(20);
    let mut rows = Vec::with_capacity(
        waiting + failed_count + usize::from(failed_job_count > 100) + usize::from(is_empty),
    );
    if failed_job_count > 100 {
        rows.push(QueueRow::FailedHint);
    }
    rows.extend((0..waiting).map(QueueRow::Waiting));
    rows.extend((0..failed_count).map(QueueRow::Failed));
    if is_empty {
        rows.push(QueueRow::Empty);
    }
    rows
}

/// Fixed row height for the virtual list. A failed row without a retry
/// button is shorter, matching the previous layout.
pub(crate) fn queue_row_height(row: QueueRow, failed_jobs: &[CompressionJob]) -> f32 {
    match row {
        QueueRow::FailedHint => QUEUE_HINT_ROW_HEIGHT,
        QueueRow::Waiting(_) => QUEUE_WAITING_ROW_HEIGHT,
        QueueRow::Failed(index) => {
            if failed_jobs.get(index).is_some_and(job_retry_available) {
                QUEUE_FAILED_ROW_HEIGHT
            } else {
                QUEUE_FAILED_ROW_HEIGHT_NO_RETRY
            }
        }
        QueueRow::Empty => QUEUE_EMPTY_ROW_HEIGHT,
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
                    .child(tr!("compression.no_profiles"))
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
                                        if auto {
                                            tr!("compression.auto")
                                        } else {
                                            tr!("compression.manual")
                                        },
                                        if auto { p.green } else { p.muted },
                                        if auto { p.green_soft } else { p.panel_2 },
                                    ))
                                    .when(profile.clipboard, |this| {
                                        this.child(self.badge(
                                            tr!("compression.clipboard"),
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
        card_header(&p)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child(tr!("compression.profiles")),
            )
            .child(
                Button::new("add-profile")
                    .label(tr!("compression.add"))
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let profile = CompressionProfile {
                            name: tr_args!(
                                "compression.new_profile",
                                index = this.profiles.len() + 1
                            ),
                            auto_enabled: false,
                            ..CompressionProfile::default()
                        };
                        if let Err(error) = this.db.save_profile(&profile) {
                            tracing::warn!(%error, "cannot add compression profile");
                            this.toast_error(tr_args!("compression.add_error", error = error), cx);
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
        if self.profile_name_editing_id.as_deref() != Some(profile.id.as_str()) {
            let name = profile.name.clone();
            self.profile_name_input.update(cx, |input, cx| {
                input.set_value(name, window, cx);
            });
            self.profile_name_editing_id = Some(profile.id.clone());
            self.profile_name_reset_pending = false;
            self.profile_name_error = None;
        } else if self.profile_name_reset_pending {
            // Enter/blur normalizes trims and rejected values. Avoid rewriting
            // an unchanged field so its undo history and caret stay intact.
            if self.profile_name_input.read(cx).value().as_ref() != profile.name.as_str() {
                let name = profile.name.clone();
                self.profile_name_input.update(cx, |input, cx| {
                    input.set_value(name, window, cx);
                });
            }
            self.profile_name_reset_pending = false;
            self.profile_name_error = None;
        }
        let name_error = self.profile_name_error;
        let name_char_count = self
            .profile_name_input
            .read(cx)
            .value()
            .trim()
            .chars()
            .count();
        let p = self.palette;
        let profile_id = profile.id.clone();
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
            None => tr!("compression.output.empty"),
            Some(dir) if Path::new(dir).is_absolute() => tr!("compression.output.absolute"),
            Some(_) => tr!("compression.output.relative"),
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
                card_header(&p)
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
                            .label(tr!("compression.duplicate"))
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.duplicate_profile(cx))),
                    )
                    .child(
                        Button::new("delete-profile")
                            .label(tr!("compression.delete"))
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
                    .child(self.section_label(tr!("compression.section.basic")))
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("compression.profile_name")),
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
                                        Some(error) => error.message(),
                                        None => tr_args!(
                                            "compression.name_hint",
                                            count = name_char_count,
                                            max = MAX_PROFILE_NAME_CHARS
                                        ),
                                    }),
                            ),
                    )
                    .child(section_divider(&p))
                    .child(self.section_label(tr!("compression.section.format")))
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
                                    .child(tr!("compression.lossless")),
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
                                    .child(tr!("compression.quality")),
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
                                    tr!("compression.lossless_no_quality")
                                } else {
                                    tr!("compression.adjust_quality")
                                },
                            )),
                    )
                    .child(section_divider(&p))
                    .child(self.section_label(tr!("compression.section.resize")))
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
                                            .child(tr!("compression.enable_resize")),
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
                                                .child(tr!("compression.resolution")),
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
                                                                        profile.resize_enabled =
                                                                            true;
                                                                        profile.width = width;
                                                                        profile.height = height;
                                                                    },
                                                                    cx,
                                                                );
                                                            },
                                                        ))
                                                        .child(label)
                                                },
                                            ),
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
                                                        .child(tr!("compression.resize_mode")),
                                                )
                                                .child(
                                                    h_flex().gap(px(4.)).children(
                                                        [
                                                            (
                                                                ResizeMode::DownscaleOnly,
                                                                tr!("compression.resize.shrink"),
                                                            ),
                                                            (
                                                                ResizeMode::Always,
                                                                tr!("compression.resize.always"),
                                                            ),
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
                                                                                profile
                                                                                    .resize_mode =
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
                                                        tr!("compression.resize.shrink_hint")
                                                    }
                                                    ResizeMode::Always => {
                                                        tr!("compression.resize.always_hint")
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
                                                .child(tr!("compression.keep_aspect")),
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
                    .child(self.section_label(tr!("compression.section.output")))
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
                                            .child(tr!("compression.output_dir")),
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
                                                    .label(tr!("compression.browse"))
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
                                                            let options = DialogOptions {
                                                                directory: current
                                                                    .map(PathBuf::from),
                                                                ..DialogOptions::default()
                                                            };
                                                            let dialog =
                                                                options.dialog(Some(&*window));
                                                            cx.spawn_in(
                                                                window,
                                                                async move |this, cx| {
                                                                    let Some(dir) = file_picker::pick_folder(
                                                                        dialog,
                                                                    )
                                                                    .await
                                                                    else {
                                                                        return;
                                                                    };
                                                                    let path = dir
                                                                        .to_string_lossy()
                                                                        .to_string();
                                                                    let output_path =
                                                                        path.clone();
                                                                    cx.update(
                                                                        |window, cx| {
                                                                            this.update(
                                                                                cx,
                                                                                |this, cx| {
                                                                                    this.profile_output_input
                                                                                        .update(
                                                                                            cx,
                                                                                            |input, cx| {
                                                                                                input.set_value(
                                                                                                    path,
                                                                                                    window,
                                                                                                    cx,
                                                                                                )
                                                                                            },
                                                                                        );
                                                                                    this.edit_profile(
                                                                                        move |profile| {
                                                                                            profile.output_dir =
                                                                                                Some(output_path)
                                                                                        },
                                                                                        cx,
                                                                                    );
                                                                                },
                                                                            )
                                                                            .ok();
                                                                        },
                                                                    )
                                                                    .ok();
                                                                },
                                                            )
                                                            .detach();
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
                                            .child(tr!("compression.suffix")),
                                    )
                                    .child(Input::new(&self.profile_suffix_input).small().flex_1())
                                    .child(
                                        div()
                                            .text_size(px(9.5))
                                            .text_color(rgb(p.muted_2))
                                            .child(tr!("compression.empty_ok")),
                                    ),
                            ),
                    )
                    .child(section_divider(&p))
                    .child(
                        div()
                            .p(px(12.))
                            .rounded(px(8.))
                            .bg(rgb(p.panel_2))
                            .border_1()
                            .border_color(rgb(p.border))
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(tr!("compression.save_hint")),
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
                        tr!("compression.stats.total"),
                        self.stats.compressed_count.to_string(),
                    ))
                    .child(self.editor_stat(
                        tr!("compression.stats.photos"),
                        self.stats.photo_count.to_string(),
                    ))
                    .child(self.editor_stat(
                        tr!("compression.stats.compressed_total"),
                        format_bytes(self.stats.derivative_bytes),
                    )),
            )
    }

    /// Small stat block inside the editor card's footer.
    fn editor_stat(&self, label: impl Into<String>, value: String) -> impl IntoElement {
        let p = self.palette;
        let label: String = label.into();
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
                    .child(label),
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
        // The loaded page holds at most 100 failed jobs; the aggregate
        // count keeps the retry-all action available when the retryable
        // failures sit beyond that page.
        let retryable_jobs = self.retryable_failed_count > 0;
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
                            .child(tr!("compression.queue.title")),
                    )
                    .when(self.active_job_count > 0, |this| {
                        this.child(self.badge(
                            tr_args!("compression.queue.active", count = self.active_job_count),
                            p.accent,
                            p.accent_soft,
                        ))
                    })
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(self.badge(
                            tr_args!("compression.queue.waiting", count = self.waiting_job_count),
                            p.amber,
                            p.amber_soft,
                        ))
                    })
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(if self.settings.auto_compress_keep_original {
                            self.badge(tr!("compression.queue.auto_keep"), p.green, p.green_soft)
                        } else {
                            self.badge(tr!("compression.queue.auto_replace"), p.red, p.red_soft)
                        })
                    })
                    .when(self.failed_job_count > 0, |this| {
                        this.child(self.badge(
                            tr_args!("compression.queue.failed", count = self.failed_job_count),
                            p.red,
                            p.red_soft,
                        ))
                    })
                    .child(div().flex_1())
                    .when(self.waiting_job_count > 0, |this| {
                        this.child(
                            Button::new("release-waiting-jobs")
                                .label(tr!("compression.queue.release_waiting"))
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
                                .label(tr!("compression.queue.retry_all"))
                                .small()
                                .ghost()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.retry_all_failed_jobs(cx)),
                                ),
                        )
                    }),
            )
            .child({
                let queue_is_empty = self.waiting_jobs.is_empty()
                    && self.jobs.is_empty()
                    && self.failed_jobs.is_empty();
                let rows = queue_rows(
                    self.waiting_jobs.len(),
                    self.failed_jobs.len(),
                    self.failed_job_count,
                    queue_is_empty,
                );
                let item_sizes: Rc<Vec<VirtualRowSize<Pixels>>> = Rc::new(
                    rows.iter()
                        .map(|row| {
                            virtual_row_size(px(0.), px(queue_row_height(*row, &self.failed_jobs)))
                        })
                        .collect(),
                );
                let queue_scroll = self.queue_scroll.clone();
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(
                        v_virtual_list(
                            cx.entity(),
                            "compression-queue",
                            item_sizes,
                            move |app, range: std::ops::Range<usize>, _window, cx| {
                                range
                                    .filter_map(|index| {
                                        rows.get(index).map(|row| render_queue_row(app, *row, cx))
                                    })
                                    .collect::<Vec<_>>()
                            },
                        )
                        .track_scroll(&queue_scroll),
                    )
                    .child(Scrollbar::vertical(&queue_scroll).viewport_from_layout())
            })
    }
}

/// Renders one virtual queue row. The virtual list calls this only for the
/// rows intersecting its viewport, so a 120+ job queue builds a bounded
/// number of widgets per frame.
fn render_queue_row(app: &mut App, row: QueueRow, cx: &mut Context<App>) -> AnyElement {
    let p = app.palette;
    match row {
        QueueRow::FailedHint => div()
            .w_full()
            .h(px(QUEUE_HINT_ROW_HEIGHT))
            .px(px(GALLERY_PAD_X))
            .py(px(8.))
            .text_size(px(10.))
            .text_color(rgb(p.muted_2))
            .child(tr!("compression.queue.failed_hint"))
            .into_any_element(),
        QueueRow::Empty => div()
            .w_full()
            .h(px(QUEUE_EMPTY_ROW_HEIGHT))
            .p(px(GALLERY_PAD_X))
            .text_size(px(12.))
            .text_color(rgb(p.muted_2))
            .child(tr!("compression.queue.empty"))
            .into_any_element(),
        QueueRow::Waiting(index) => {
            let Some(job) = app.waiting_jobs.get(index) else {
                return div().into_any_element();
            };
            let status = queue_status_badge(&job.status, &p);
            let profile_name = app
                .profiles
                .iter()
                .find(|profile| profile.id == job.profile_id)
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| tr!("compression.unknown_profile"));
            h_flex()
                .w_full()
                .h(px(QUEUE_WAITING_ROW_HEIGHT))
                .px(px(16.))
                .py(px(10.))
                .gap(px(8.))
                .items_center()
                .border_b_1()
                .border_color(rgb(p.border))
                .child(app.badge(status.0, status.1, status.2))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(rgb(p.text))
                        .truncate()
                        .child(tr_args!("compression.job.photo", id = job.photo_id)),
                )
                .child(
                    div()
                        .max_w(px(160.))
                        .truncate()
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
                .into_any_element()
        }
        QueueRow::Failed(index) => {
            let Some(job) = app.failed_jobs.get(index) else {
                return div().into_any_element();
            };
            let name = app
                .failed_job_names
                .get(&job.photo_id)
                .cloned()
                .unwrap_or_else(|| tr_args!("compression.job.photo", id = job.photo_id));
            let profile_name = app
                .profiles
                .iter()
                .find(|profile| profile.id == job.profile_id)
                .map(|profile| profile.name.clone());
            let error = job_error_label(job);
            let retryable = job_retry_available(job);
            let id = job.id.clone();
            v_flex()
                .w_full()
                .h(px(queue_row_height(row, &app.failed_jobs)))
                .px(px(16.))
                .py(px(10.))
                .gap(px(6.))
                .border_b_1()
                .border_color(rgb(p.border))
                .child(
                    h_flex()
                        .items_center()
                        .gap(px(8.))
                        .child(app.badge(tr!("compression.status.failed"), p.red, p.red_soft))
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
                                    .max_w(px(160.))
                                    .truncate()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(name),
                            )
                        }),
                )
                .when(!error.is_empty(), |this| {
                    // Fixed-height virtualization: one truncated line instead
                    // of a wrapping error that would change the row height.
                    this.child(
                        div()
                            .w_full()
                            .truncate()
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
                                Button::new(SharedString::from(format!("retry-job-{}", job.id)))
                                    .label(tr!("common.retry"))
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_job(id.clone(), cx)
                                    })),
                            )
                        }),
                )
                .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        QueueRow, job_error_label, job_retry_available, job_retry_label, queue_row_height,
        queue_rows, queue_status_badge,
    };
    use vrc_photon_core::model::{CompressionJob, QueueStatus};

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
    fn job_error_label_falls_back_when_no_reason_was_recorded() {
        assert_eq!(job_error_label(&test_job(QueueStatus::Failed, 0)), "boom");
        let missing = CompressionJob {
            error: None,
            ..test_job(QueueStatus::Failed, 0)
        };
        assert_eq!(job_error_label(&missing), "原因不明");
        let blank = CompressionJob {
            error: Some("  ".to_string()),
            ..test_job(QueueStatus::Failed, 0)
        };
        assert_eq!(job_error_label(&blank), "原因不明");
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
        assert_eq!(queue_status_badge(&QueueStatus::Unknown, &p).0, "不明");
    }

    #[test]
    fn queue_rows_order_waiting_before_failed_and_cap_waiting_at_twenty() {
        let rows = queue_rows(25, 3, 3, false);
        let expected: Vec<QueueRow> = (0..20)
            .map(QueueRow::Waiting)
            .chain((0..3).map(QueueRow::Failed))
            .collect();
        assert_eq!(rows, expected);
    }

    #[test]
    fn queue_rows_show_the_failed_hint_only_past_one_hundred_failures() {
        let hinted = queue_rows(1, 1, 101, false);
        assert_eq!(hinted.first(), Some(&QueueRow::FailedHint));
        assert_eq!(hinted[1], QueueRow::Waiting(0));
        assert_eq!(hinted[2], QueueRow::Failed(0));

        let normal = queue_rows(1, 1, 100, false);
        assert!(!normal.contains(&QueueRow::FailedHint));
    }

    #[test]
    fn queue_rows_empty_queue_shows_the_empty_state() {
        assert_eq!(queue_rows(0, 0, 0, true), vec![QueueRow::Empty]);
        assert!(queue_rows(0, 0, 0, false).is_empty());
    }

    #[test]
    fn queue_rows_over_one_hundred_twenty_only_builds_a_bounded_window() {
        let rows = queue_rows(20, 200, 200, false);
        assert_eq!(rows.first(), Some(&QueueRow::FailedHint));
        assert_eq!(rows.len(), 221);

        let failures = vec![test_job(QueueStatus::Failed, 0); 200];
        let heights: Vec<f32> = rows
            .iter()
            .map(|row| queue_row_height(*row, &failures))
            .collect();
        assert_eq!(heights.len(), rows.len());

        // Simulate the dashboard's queue viewport: only the rows whose
        // cumulative height intersects the window are built.
        let viewport = 420.0;
        let mut bottom = 0.0;
        let mut visible = 0;
        for height in &heights {
            if bottom >= viewport {
                break;
            }
            visible += 1;
            bottom += height;
        }
        assert!(visible <= 12, "visible rows: {visible}");
        assert!(visible < rows.len());
    }

    #[test]
    fn queue_row_height_matches_the_retry_affordance() {
        let retryable = test_job(QueueStatus::Failed, 0);
        let capped = test_job(QueueStatus::Failed, 3);
        assert_eq!(
            queue_row_height(QueueRow::Failed(0), &[retryable]),
            super::QUEUE_FAILED_ROW_HEIGHT
        );
        assert_eq!(
            queue_row_height(QueueRow::Failed(0), &[capped]),
            super::QUEUE_FAILED_ROW_HEIGHT_NO_RETRY
        );
        assert!(
            queue_row_height(QueueRow::Waiting(0), &[]) > 0.0,
            "waiting rows keep a fixed height"
        );
    }
}
