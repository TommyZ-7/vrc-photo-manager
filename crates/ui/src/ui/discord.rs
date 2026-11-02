//! Discord integration card: webhook editing, destination label, test send,
//! and the single-select compression profile used for uploads.
//!
//! The full webhook URL never reaches the screen: a stored URL is rendered
//! through [`mask_webhook_url`] and only the edit field shows what the user is
//! typing.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photon_core::discord::mask_webhook_url;
use vrc_photon_i18n::tr;

use crate::app::App;
use crate::ui::components::{glass_panel, manual_option_row};
use crate::ui::theme::*;

/// Phase 2 adds the automatic-send section to this body, so the bound is
/// generous enough that the newer content is not clipped by the animation.
const DISCORD_BODY_MAX: f32 = 900.0;

/// The masked URL when a webhook is stored, otherwise the "not configured"
/// prompt. Never returns the raw value.
pub(crate) fn webhook_state_label(url: Option<&str>) -> String {
    match url {
        Some(url) => mask_webhook_url(url),
        None => tr!("integrations.discord.webhook.missing"),
    }
}

/// Name of the selected Discord profile, or the prompt to pick one.
pub(crate) fn discord_profile_label(name: Option<&str>) -> String {
    match name {
        Some(name) => name.to_string(),
        None => tr!("integrations.discord.profile.none"),
    }
}

impl App {
    pub(crate) fn discord_card(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let enabled = self.settings.discord_enabled;
        let motion = self.integrations_discord_motion;
        let webhook = self.discord_url.clone();
        let editing = self.discord_url_editing;
        let url_error = self.discord_url_error.clone();
        let destination_value = self.discord_destination_input.read(cx).value().to_string();
        let stored_destination = self.settings.discord_destination.clone();
        let can_test = enabled && webhook.is_some();
        let send_original = self.settings.discord_send_original;
        let include_players = self.settings.discord_include_players;
        let profiles = self.profiles.clone();
        let selected_profile = profiles
            .iter()
            .find(|profile| profile.discord)
            .map(|profile| profile.name.clone());

        let body = v_flex()
            .pt(px(12.))
            .gap(px(12.))
            .when(enabled, |this| {
                this.child(
                    div()
                        .p(px(12.))
                        .rounded(px(8.))
                        .bg(rgb(p.red_soft))
                        .border_1()
                        .border_color(rgb(p.red_border))
                        .text_size(px(10.))
                        .text_color(rgb(p.red))
                        .child(tr!("integrations.discord.warning")),
                )
            })
            .child(self.webhook_section(webhook.as_deref(), editing, url_error, cx))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.text))
                            .child(tr!("integrations.discord.destination.label")),
                    )
                    .child(Input::new(&self.discord_destination_input).small())
                    // Enter/blur commit through the Task 11 subscription, which
                    // has no window to rewrite the field. Blank input is saved
                    // as the default, so show that normalized value instead of
                    // leaving the field looking unsaved.
                    .when(destination_value.trim().is_empty(), |this| {
                        this.child(div().text_size(px(10.)).text_color(rgb(p.muted_2)).child(
                            format!(
                                "{}: {stored_destination}",
                                tr!("integrations.discord.destination.label")
                            ),
                        ))
                    }),
            )
            .child(
                h_flex().items_center().gap(px(8.)).child(
                    Button::new("discord-test-send")
                        .label(tr!("integrations.discord.test.button"))
                        .small()
                        .disabled(!can_test)
                        .on_click(cx.listener(|this, _, _, cx| this.send_test_message(cx))),
                ),
            )
            .child(self.discord_toggle_row(
                "discord-send-original",
                tr!("integrations.discord.source.original"),
                tr!("integrations.discord.source.original_hint"),
                send_original,
                cx,
                App::set_discord_send_original,
            ))
            .child(self.discord_toggle_row(
                "discord-include-players",
                tr!("integrations.discord.players.label"),
                tr!("integrations.discord.players.hint"),
                include_players,
                cx,
                App::set_discord_include_players,
            ))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.text))
                            .child(tr!("integrations.discord.profile.label")),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("integrations.discord.profile.hint")),
                    )
                    .when(!profiles.is_empty(), |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(discord_profile_label(selected_profile.as_deref())),
                        )
                    })
                    .child(if profiles.is_empty() {
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(tr!("integrations.auto.profiles_empty"))
                            .into_any_element()
                    } else {
                        v_flex()
                            .id("integrations-discord-profiles")
                            .max_h(px(160.))
                            .overflow_y_scrollbar()
                            .gap(px(4.))
                            .children(profiles.into_iter().map(|profile| {
                                let id = profile.id.clone();
                                let selected = profile.discord;
                                manual_option_row(
                                    &p,
                                    SharedString::from(format!("discord-profile-{}", profile.id)),
                                    profile.name.clone(),
                                    profile_summary(&profile),
                                    selected,
                                    true,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_discord_profile(&id, !selected, cx);
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
                                    .child(tr!("integrations.discord.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("integrations.discord.hint")),
                            ),
                    )
                    .child(
                        Switch::new("integrations-discord-enabled")
                            .checked(enabled)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.set_discord_enabled(*checked, cx);
                            })),
                    ),
            )
            .child(self.collapsible_body(
                "integrations-discord-body",
                motion,
                DISCORD_BODY_MAX,
                window,
                cx,
                body,
            ))
            .into_any_element()
    }

    fn webhook_section(
        &self,
        webhook: Option<&str>,
        editing: bool,
        error: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;

        if editing {
            return v_flex()
                .gap(px(6.))
                .child(Input::new(&self.discord_url_input).small())
                .when_some(error, |this, error| {
                    this.child(div().text_size(px(10.)).text_color(rgb(p.red)).child(error))
                })
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            Button::new("discord-webhook-save")
                                .label(tr!("integrations.discord.webhook.save"))
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, _, cx| this.save_discord_url(cx))),
                        )
                        .child(
                            Button::new("discord-webhook-cancel")
                                .label(tr!("integrations.discord.webhook.cancel"))
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.discord_url_editing = false;
                                    this.discord_url_error = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element();
        }

        match webhook {
            Some(url) => h_flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(8.))
                        .bg(rgb(p.panel_2))
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .child(webhook_state_label(Some(url))),
                )
                .child(
                    Button::new("discord-webhook-change")
                        .label(tr!("integrations.discord.webhook.change"))
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.begin_discord_url_edit(window, cx);
                        })),
                )
                .child(
                    Button::new("discord-webhook-remove")
                        .label(tr!("integrations.discord.webhook.remove"))
                        .small()
                        .danger()
                        .on_click(cx.listener(|this, _, _, cx| this.clear_discord_url(cx))),
                )
                .into_any_element(),
            None => h_flex()
                .items_center()
                .gap(px(8.))
                .child(
                    Button::new("discord-webhook-add")
                        .label(tr!("integrations.discord.webhook.add"))
                        .small()
                        .primary()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.begin_discord_url_edit(window, cx);
                        })),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child(webhook_state_label(None)),
                )
                .into_any_element(),
        }
    }

    fn discord_toggle_row(
        &self,
        id: &'static str,
        label: String,
        hint: String,
        checked: bool,
        cx: &mut Context<Self>,
        toggle: fn(&mut App, bool, &mut Context<App>),
    ) -> AnyElement {
        let p = self.palette;
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
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(hint),
                    ),
            )
            .child(div().flex_1())
            .child(Switch::new(id).checked(checked).on_click(cx.listener(
                move |this, value, _, cx| {
                    toggle(this, *value, cx);
                },
            )))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{discord_profile_label, webhook_state_label};
    use vrc_photon_i18n::tr;

    #[test]
    fn webhook_state_label_distinguishes_missing_and_configured() {
        assert_eq!(
            webhook_state_label(None),
            tr!("integrations.discord.webhook.missing")
        );
        assert!(webhook_state_label(Some("https://discord.com/api/webhooks/1/t")).contains("••••"));
    }

    #[test]
    fn webhook_state_label_never_exposes_the_token() {
        let masked = webhook_state_label(Some("https://discord.com/api/webhooks/1/secret"));
        assert!(!masked.contains("secret"), "{masked}");
    }

    #[test]
    fn discord_profile_label_requires_a_choice_when_missing() {
        assert_eq!(
            discord_profile_label(None),
            tr!("integrations.discord.profile.none")
        );
        assert_eq!(
            discord_profile_label(Some("discord用webp")),
            "discord用webp"
        );
    }
}
