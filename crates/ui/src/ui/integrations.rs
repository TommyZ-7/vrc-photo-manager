//! Integrations & automation screen: automatic compression, AI tagging
//! runtime settings, and clipboard settings, with Discord in the same card
//! stack (#142).

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::{ScrollableElement as _, Scrollbar};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photon_ai_protocol::{TaggingJobStatus, TaggingStage};
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::Screen;
use crate::app::ai::{
    AiErrorKind, active_job_photo_label, ai_error_summary_in, backend_options,
    backend_selection_is_disabled_when_not_ready, download_progress_bar_ratio,
    variant_backend_label,
};
use crate::state::motion::ToggleMotion;
use crate::ui::components::{
    chip, compression_policy_warning, glass_panel, manual_option_row, status_progress_bar,
};
use crate::ui::theme::*;

/// Generous bounds for the collapsed-body height animation. The profile lists
/// scroll above these sizes, so the body never exceeds its bound.
const AUTO_BODY_MAX: f32 = 430.0;
const CLIPBOARD_BODY_MAX: f32 = 330.0;
const AI_BODY_MAX: f32 = 520.0;

/// Which runtime setting a switch row writes.
#[derive(Clone, Copy)]
enum AiToggle {
    Enabled,
    Auto,
    Backlog,
}

impl App {
    pub(crate) fn integrations_screen(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(
                v_flex()
                    .id("integrations-sections")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.integrations_scroll)
                    .gap(px(CARD_GAP))
                    .child(self.discord_card(window, cx))
                    .child(self.auto_compression_card(window, cx))
                    .child(self.ai_tagging_card(window, cx))
                    .child(self.clipboard_card(window, cx))
                    .child(self.send_history_card(window, cx)),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(8.))
                    .child(Scrollbar::vertical(&self.integrations_scroll)),
            )
    }

    fn auto_compression_card(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let enabled = self.settings.auto_compress;
        let motion = self.integrations_auto_motion;
        let profiles = self.profiles.clone();

        let body = v_flex()
            .pt(px(12.))
            .gap(px(12.))
            .child(self.setting_switch(
                tr!("settings.startup.immediate"),
                tr!("settings.startup.immediate_hint"),
                self.settings.auto_compress_immediate,
                false,
                cx,
                |settings, value| settings.auto_compress_immediate = value,
            ))
            .child(self.setting_switch(
                tr!("settings.startup.keep_original"),
                tr!("settings.startup.keep_original_hint"),
                self.settings.auto_compress_keep_original,
                false,
                cx,
                |settings, value| settings.auto_compress_keep_original = value,
            ))
            .when_some(
                compression_policy_warning(
                    self.settings.auto_compress_keep_original,
                    self.settings.auto_compress,
                ),
                |this, warning| {
                    this.child(
                        div()
                            .p(px(12.))
                            .rounded(px(8.))
                            .bg(rgb(p.red_soft))
                            .border_1()
                            .border_color(rgb(p.red_border))
                            .text_size(px(10.))
                            .text_color(rgb(p.red))
                            .child(warning),
                    )
                },
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.text))
                            .child(tr!("integrations.auto.profiles")),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("integrations.auto.profiles_hint")),
                    )
                    .child(if profiles.is_empty() {
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("integrations.auto.profiles_empty"))
                            .into_any_element()
                    } else {
                        v_flex()
                            .id("integrations-auto-profiles")
                            .max_h(px(160.))
                            .overflow_y_scrollbar()
                            .gap(px(4.))
                            .children(profiles.into_iter().map(|profile| {
                                let id = profile.id.clone();
                                Checkbox::new(SharedString::from(format!(
                                    "auto-profile-{}",
                                    profile.id
                                )))
                                .checked(profile.auto_enabled)
                                .label(profile.name.clone())
                                .small()
                                .on_click(cx.listener(move |this, checked, _, cx| {
                                    this.edit_profile_by_id(
                                        &id,
                                        |profile| profile.auto_enabled = *checked,
                                        cx,
                                    );
                                }))
                                .into_any_element()
                            }))
                            .into_any_element()
                    }),
            );

        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .p(px(14.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
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
                                    .child(tr!("integrations.auto.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("integrations.auto.hint")),
                            ),
                    )
                    .child(
                        Switch::new("integrations-auto-enabled")
                            .checked(enabled)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.set_auto_compress_enabled(*checked, cx);
                            })),
                    ),
            )
            .child(self.collapsible_body(
                "integrations-auto-body",
                motion,
                AUTO_BODY_MAX,
                window,
                cx,
                body,
            ))
            .into_any_element()
    }

    fn ai_tagging_card(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let ready = self.ai.ready();
        let enabled = self.settings.ai_tagging_enabled;
        let motion = self.integrations_ai_motion;

        let card = glass_panel(&self.palette)
            .flex()
            .flex_col()
            .p(px(14.))
            .rounded(px(17.))
            .child(
                h_flex().items_center().gap(px(12.)).child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_semibold()
                                .text_color(rgb(p.text))
                                .child(tr!("integrations.ai.title")),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(tr!("integrations.ai.hint")),
                        ),
                ),
            );

        if !ready {
            return card
                .child(
                    v_flex()
                        .pt(px(12.))
                        .gap(px(10.))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted))
                                .child(tr!("integrations.ai.not_ready")),
                        )
                        .child(
                            Button::new("integrations-ai-open-settings-not-ready")
                                .label(tr!("integrations.ai.open_settings"))
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.switch_screen(Screen::Settings, cx);
                                })),
                        ),
                )
                .into_any_element();
        }

        let auto = self.settings.ai_tagging_auto;
        let backlog = self.settings.ai_tagging_backlog;
        let counts = self.ai.job_counts;
        let language = vrc_photon_i18n::current_locale();
        let job_names = self.ai.active_job_names.clone();
        let job_error = self.ai.error_of(AiErrorKind::Job).cloned();
        let active_jobs: Vec<_> = self
            .ai
            .active_jobs
            .iter()
            .filter(|job| {
                matches!(
                    job.status,
                    TaggingJobStatus::Pending | TaggingJobStatus::Processing
                )
            })
            .take(3)
            .cloned()
            .collect();
        let backend_selection_disabled =
            backend_selection_is_disabled_when_not_ready(ready, enabled);
        let backends = self
            .ai
            .probe
            .as_ref()
            .map(|probe| backend_options(probe, &self.settings.ai_tagging_backend))
            .unwrap_or_default();

        let details =
            v_flex()
                .gap(px(10.))
                .child(
                    h_flex()
                        .gap(px(14.))
                        .child(ai_job_stat(
                            &p,
                            tr!("integrations.ai.jobs.waiting"),
                            counts.waiting,
                        ))
                        .child(ai_job_stat(
                            &p,
                            tr!("integrations.ai.jobs.processing"),
                            counts.pending + counts.processing,
                        ))
                        .child(ai_job_stat(
                            &p,
                            tr!("integrations.ai.jobs.failed"),
                            counts.failed,
                        )),
                )
                .when_some(job_error, |this, error| {
                    this.child(
                        div()
                            .px(px(10.))
                            .text_size(px(10.))
                            .text_color(rgb(p.red))
                            .child(tr_args!(
                                "integrations.ai.jobs.latest_failure",
                                error = ai_error_summary_in(&language, &error)
                            )),
                    )
                })
                .child(if active_jobs.is_empty() {
                    v_flex()
                        .gap(px(2.))
                        .p(px(10.))
                        .rounded(px(10.))
                        .bg(rgb(p.panel_2))
                        .child(
                            div()
                                .text_size(px(11.))
                                .font_medium()
                                .text_color(rgb(p.muted))
                                .child(tr!("integrations.ai.jobs.empty")),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(tr!("integrations.ai.jobs.empty_hint")),
                        )
                        .into_any_element()
                } else {
                    v_flex()
                        .id("integrations-ai-jobs")
                        .gap(px(6.))
                        .children(active_jobs.into_iter().map(|job| {
                            let job_id = job.id.clone();
                            let photo_label = active_job_photo_label(
                                job_names.get(&job.photo_id).map(String::as_str),
                                job.photo_id,
                                &language,
                            );
                            let ratio = download_progress_bar_ratio(job.progress);
                            let percent = (ratio * 100.0).round() as u32;
                            v_flex()
                                .gap(px(5.))
                                .p(px(10.))
                                .rounded(px(10.))
                                .bg(rgb(p.panel_2))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_size(px(11.))
                                                .font_medium()
                                                .text_color(rgb(p.text))
                                                .truncate()
                                                .child(photo_label),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.))
                                                .text_color(rgb(p.accent))
                                                .child(format!("{percent}%")),
                                        ),
                                )
                                .child(status_progress_bar(&p, ratio))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_size(px(10.))
                                                .text_color(rgb(p.muted_2))
                                                .truncate()
                                                .child(ai_stage_label(job.stage)),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!(
                                                "integrations-ai-cancel-{job_id}"
                                            )))
                                            .label(tr!("integrations.ai.cancel_job"))
                                            .small()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.cancel_ai_job(&job_id, cx);
                                            })),
                                        ),
                                )
                                .into_any_element()
                        }))
                        .into_any_element()
                })
                .when(counts.waiting > 0, |this| {
                    this.child(
                        v_flex()
                            .gap(px(6.))
                            .p(px(10.))
                            .rounded(px(10.))
                            .bg(rgb(p.panel_2))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .font_medium()
                                    .text_color(rgb(p.text))
                                    .child(tr!("integrations.ai.start_backlog")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("integrations.ai.start_backlog_hint")),
                            )
                            .child(
                                Button::new("integrations-ai-start-backlog")
                                    .label(tr!("integrations.ai.start_backlog"))
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.start_ai_backlog(cx);
                                    })),
                            ),
                    )
                });

        let settings_row = h_flex()
            .items_center()
            .gap(px(8.))
            .child(
                Button::new("integrations-ai-open-settings-ready")
                    .label(tr!("integrations.ai.open_settings"))
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.switch_screen(Screen::Settings, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("integrations.ai.jobs.limit_hint")),
            );

        let body = v_flex()
            .pt(px(12.))
            .gap(px(12.))
            .child(self.ai_switch_row(
                tr!("integrations.ai.enabled"),
                tr!("integrations.ai.enabled_hint"),
                enabled,
                true,
                AiToggle::Enabled,
                cx,
            ))
            .child(self.ai_switch_row(
                tr!("integrations.ai.auto"),
                tr!("integrations.ai.auto_hint"),
                auto,
                enabled,
                AiToggle::Auto,
                cx,
            ))
            .child(self.ai_switch_row(
                tr!("integrations.ai.backlog"),
                tr!("integrations.ai.backlog_hint"),
                backlog,
                enabled,
                AiToggle::Backlog,
                cx,
            ))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(p.text))
                                    .child(tr!("integrations.ai.backend")),
                            )
                            .child(div().flex_1())
                            .when(backend_selection_disabled, |this| {
                                this.child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(p.muted_2))
                                        .child(tr!("integrations.ai.backend_enable_hint")),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(4.))
                            .children(backends.into_iter().map(|(id, selected)| {
                                let click_id = id.clone();
                                let label = if id == "auto" {
                                    tr!("integrations.ai.backend.auto")
                                } else {
                                    variant_backend_label(&id)
                                };
                                chip(&p, &label, selected)
                                    .id(SharedString::from(format!("integrations-ai-backend-{id}")))
                                    .opacity(if backend_selection_disabled { 0.5 } else { 1.0 })
                                    .when(!backend_selection_disabled, |this| {
                                        this.cursor_pointer().on_click(cx.listener(
                                            move |this, _, _, cx| {
                                                let backend = click_id.clone();
                                                this.save_ai_runtime_settings(
                                                    cx,
                                                    move |settings| {
                                                        settings.ai_tagging_backend = backend;
                                                    },
                                                );
                                            },
                                        ))
                                    })
                                    .into_any_element()
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("integrations.ai.backend_hint")),
                    ),
            )
            .child(self.collapsible_body(
                "integrations-ai-body",
                motion,
                AI_BODY_MAX,
                window,
                cx,
                details,
            ))
            .child(settings_row);

        card.child(body).into_any_element()
    }

    fn ai_switch_row(
        &self,
        label: String,
        hint: String,
        checked: bool,
        enabled: bool,
        toggle: AiToggle,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.palette;
        let id = match toggle {
            AiToggle::Enabled => "integrations-ai-enabled",
            AiToggle::Auto => "integrations-ai-auto",
            AiToggle::Backlog => "integrations-ai-backlog",
        };
        h_flex()
            .items_center()
            .gap(px(12.))
            .py(px(2.))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(if enabled { p.text } else { p.muted_2 }))
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(hint),
                    ),
            )
            .child(
                Switch::new(id)
                    .checked(checked)
                    .disabled(!enabled)
                    .on_click(cx.listener(move |this, value, _, cx| {
                        if !enabled {
                            return;
                        }
                        match toggle {
                            AiToggle::Enabled => this.set_ai_tagging_enabled(*value, cx),
                            AiToggle::Auto => this.save_ai_runtime_settings(cx, |settings| {
                                settings.ai_tagging_auto = *value;
                            }),
                            AiToggle::Backlog => this.save_ai_runtime_settings(cx, |settings| {
                                settings.ai_tagging_backlog = *value;
                            }),
                        }
                    })),
            )
    }

    fn set_ai_tagging_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.ai_tagging_enabled == enabled {
            return;
        }
        self.save_ai_runtime_settings(cx, move |settings| {
            settings.ai_tagging_enabled = enabled;
        });
        // A failed save rolls the setting back; animate to the persisted
        // value so the runtime section cannot claim a state it did not save.
        let target = if self.settings.ai_tagging_enabled {
            0.0
        } else {
            1.0
        };
        if cx.reduce_motion() {
            self.integrations_ai_motion = ToggleMotion::settled(target);
        } else {
            self.integrations_ai_motion.animate_to(target, MOTION_FAST);
        }
        cx.notify();
    }

    fn clipboard_card(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let enabled = self.settings.clipboard_enabled;
        let motion = self.integrations_clipboard_motion;
        let profiles = self.profiles.clone();

        let body = v_flex()
            .pt(px(12.))
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("integrations.clipboard.profiles_hint")),
            )
            .child(if profiles.is_empty() {
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("integrations.clipboard.profiles_empty"))
                    .into_any_element()
            } else {
                v_flex()
                    .id("integrations-clipboard-profiles")
                    .max_h(px(180.))
                    .overflow_y_scrollbar()
                    .gap(px(4.))
                    .children(profiles.into_iter().map(|profile| {
                        let id = profile.id.clone();
                        let selected = profile.clipboard;
                        manual_option_row(
                            &p,
                            SharedString::from(format!("clipboard-profile-{}", profile.id)),
                            profile.name.clone(),
                            profile_summary(&profile),
                            selected,
                            true,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_clipboard_profile(&id, !selected, cx);
                        }))
                        .into_any_element()
                    }))
                    .into_any_element()
            });

        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .p(px(14.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
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
                                    .child(tr!("integrations.clipboard.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("integrations.clipboard.hint")),
                            ),
                    )
                    .child(
                        Switch::new("integrations-clipboard-enabled")
                            .checked(enabled)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.set_clipboard_enabled(*checked, cx);
                            })),
                    ),
            )
            .child(self.collapsible_body(
                "integrations-clipboard-body",
                motion,
                CLIPBOARD_BODY_MAX,
                window,
                cx,
                body,
            ))
            .into_any_element()
    }

    pub(crate) fn collapsible_body(
        &self,
        id: &'static str,
        motion: ToggleMotion,
        max_height: f32,
        window: &Window,
        cx: &Context<Self>,
        body: impl IntoElement + 'static,
    ) -> AnyElement {
        div()
            .overflow_hidden()
            .with_animation(
                (id, motion.epoch()),
                Animation::new(motion.driver_duration())
                    .with_easing(ease_motion)
                    .with_max_fps(self.motion_fps(window, cx)),
                move |el, _| {
                    let collapsed = motion.value();
                    el.max_h(px((1.0 - collapsed) * max_height))
                        .opacity(1.0 - collapsed)
                },
            )
            .child(body)
            .into_any_element()
    }

    fn set_auto_compress_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.auto_compress == enabled {
            return;
        }
        self.settings.auto_compress = enabled;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save automatic compression setting");
        }
        let target = if enabled { 0.0 } else { 1.0 };
        if cx.reduce_motion() {
            self.integrations_auto_motion = ToggleMotion::settled(target);
        } else {
            self.integrations_auto_motion
                .animate_to(target, MOTION_FAST);
        }
        cx.notify();
    }

    fn set_clipboard_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.clipboard_enabled == enabled {
            return;
        }
        self.settings.clipboard_enabled = enabled;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save clipboard setting");
        }
        let target = if enabled { 0.0 } else { 1.0 };
        if cx.reduce_motion() {
            self.integrations_clipboard_motion = ToggleMotion::settled(target);
        } else {
            self.integrations_clipboard_motion
                .animate_to(target, MOTION_FAST);
        }
        cx.notify();
    }
}

/// One queue counter in the AI runtime card.
fn ai_job_stat(p: &Palette, label: String, value: i64) -> Div {
    h_flex()
        .items_baseline()
        .gap(px(5.))
        .child(
            div()
                .text_size(px(9.5))
                .text_color(rgb(p.muted_2))
                .child(label),
        )
        .child(
            div()
                .text_size(px(12.))
                .font_semibold()
                .text_color(rgb(p.text))
                .child(value.to_string()),
        )
}

/// The localized stage description shown under one job's progress bar.
pub(crate) fn ai_stage_label(stage: Option<TaggingStage>) -> String {
    match stage {
        Some(TaggingStage::Presence) => tr!("integrations.ai.jobs.stage.presence"),
        Some(TaggingStage::PeopleCount) => tr!("integrations.ai.jobs.stage.people_count"),
        Some(TaggingStage::PlaceClass) => tr!("integrations.ai.jobs.stage.place_class"),
        Some(TaggingStage::PlaceTags) => tr!("integrations.ai.jobs.stage.place_tags"),
        Some(TaggingStage::AtmosphereTags) => {
            tr!("integrations.ai.jobs.stage.atmosphere_tags")
        }
        Some(TaggingStage::Write) => tr!("integrations.ai.jobs.stage.write"),
        None => tr!("integrations.ai.jobs.stage.queued"),
    }
}

#[cfg(test)]
mod tests {
    use super::ai_stage_label;
    use vrc_photon_ai_protocol::TaggingStage;

    #[test]
    fn atmosphere_stage_has_a_label() {
        assert_eq!(
            ai_stage_label(Some(TaggingStage::AtmosphereTags)),
            "雰囲気タグを生成中"
        );
    }
}
