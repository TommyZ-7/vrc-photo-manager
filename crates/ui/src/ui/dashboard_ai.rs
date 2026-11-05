//! Dashboard AI-tagging card.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::*;
use gpui_kit::*;
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::Screen;
use crate::app::ai::{
    AiDashboardState, AiErrorKind, active_job_photo_label, ai_dashboard_view, ai_error_summary_in,
    download_progress_bar_ratio,
};
use crate::ui::components::{glass_panel, status_progress_bar};
use crate::ui::integrations::ai_stage_label;
use crate::ui::theme::*;

impl App {
    pub(crate) fn ai_dashboard_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let language = vrc_photon_i18n::current_locale();
        let view = ai_dashboard_view(&self.ai, self.settings.ai_tagging_enabled);
        let counts = self.ai.job_counts;
        let jobs = view.jobs.clone();
        let more = view.more;
        let job_names = self.ai.active_job_names.clone();
        let job_error = self.ai.error_of(AiErrorKind::Job).cloned();
        let (state_label, state_color) = match view.state {
            AiDashboardState::NotReady => (tr!("dashboard.ai.state.not_ready"), p.amber),
            AiDashboardState::Idle => (tr!("dashboard.ai.state.idle"), p.green),
            AiDashboardState::Active => (tr!("dashboard.ai.state.active"), p.accent),
            AiDashboardState::Failed => (tr!("dashboard.ai.state.failed"), p.red),
        };

        let mut card = glass_panel(&self.palette)
            .flex()
            .flex_col()
            .flex_shrink_0()
            .p(px(14.))
            .gap(px(10.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(9.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(30.))
                            .rounded(px(9.))
                            .bg(rgb(p.accent_soft))
                            .child(
                                Icon::new(IconName::Bot)
                                    .with_size(px(16.))
                                    .text_color(rgb(p.accent)),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(1.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_semibold()
                                    .text_color(rgb(p.text))
                                    .child(tr!("dashboard.ai.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(9.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("dashboard.ai.hint")),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(9.5))
                            .text_color(rgb(state_color))
                            .child(state_label),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(12.))
                    .child(ai_count(&p, tr!("dashboard.ai.waiting"), counts.waiting))
                    .child(ai_count(
                        &p,
                        tr!("dashboard.ai.processing"),
                        counts.pending + counts.processing,
                    ))
                    .child(ai_count(&p, tr!("dashboard.ai.failed"), counts.failed)),
            );

        match view.state {
            AiDashboardState::NotReady => {
                card = card
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(tr!("dashboard.ai.not_ready_hint")),
                    )
                    .child(
                        Button::new("dashboard-ai-open-settings")
                            .label(tr!("dashboard.ai.open_settings"))
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.switch_screen(Screen::Settings, cx)
                            })),
                    );
            }
            AiDashboardState::Idle | AiDashboardState::Active | AiDashboardState::Failed => {
                if view.state == AiDashboardState::Idle {
                    card = card.child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(tr!("dashboard.ai.idle_hint")),
                    );
                }

                if !jobs.is_empty() {
                    card = card.child(v_flex().id("dashboard-ai-jobs").gap(px(6.)).children(
                        jobs.into_iter().map(|job| {
                            let job_id = job.id.clone();
                            let photo_label = active_job_photo_label(
                                job_names.get(&job.photo_id).map(String::as_str),
                                job.photo_id,
                                &language,
                            );
                            let ratio = download_progress_bar_ratio(job.progress);
                            let percent = (ratio * 100.0).round() as u32;
                            v_flex()
                                .gap(px(4.))
                                .p(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(p.panel_2))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(10.))
                                                .text_color(rgb(p.text))
                                                .child(photo_label),
                                        )
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .text_size(px(9.))
                                                .text_color(rgb(p.accent))
                                                .child(format!("{percent}%")),
                                        ),
                                )
                                .child(status_progress_bar(&p, ratio))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(9.))
                                                .text_color(rgb(p.muted_2))
                                                .child(ai_stage_label(job.stage)),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!(
                                                "dashboard-ai-cancel-{job_id}"
                                            )))
                                            .label(tr!("dashboard.ai.cancel"))
                                            .small()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.cancel_ai_job(&job_id, cx);
                                            })),
                                        ),
                                )
                                .into_any_element()
                        }),
                    ));
                }

                if more > 0 {
                    card = card.child(
                        h_flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(9.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr_args!("dashboard.ai.jobs_more", count = more)),
                            )
                            .child(
                                Button::new("dashboard-ai-more")
                                    .label(tr!("dashboard.ai.open_integrations"))
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.switch_screen(Screen::Integrations, cx)
                                    })),
                            ),
                    );
                }

                if view.state == AiDashboardState::Failed {
                    card = card.child(div().text_size(px(9.5)).text_color(rgb(p.red)).child(
                        job_error.as_ref().map_or_else(
                            || tr!("dashboard.ai.failed_hint"),
                            |error| ai_error_summary_in(&language, error),
                        ),
                    ));
                }

                card = card.child(
                    Button::new("dashboard-ai-open-integrations")
                        .label(tr!("dashboard.ai.open_integrations"))
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.switch_screen(Screen::Integrations, cx)
                        })),
                );
            }
        }

        card
    }
}

fn ai_count(p: &Palette, label: String, value: i64) -> Div {
    h_flex()
        .items_baseline()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(9.))
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
