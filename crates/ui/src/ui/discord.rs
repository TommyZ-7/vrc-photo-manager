//! Discord integration cards: webhook editing, destination label, test send,
//! the single-select compression profile used for uploads, and the send
//! history with its retry/cancel actions.
//!
//! The full webhook URL never reaches the screen: a stored URL is rendered
//! through [`mask_webhook_url`] and only the edit field shows what the user is
//! typing.

use std::path::Path;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use vrc_photon_core::discord::{mask_webhook_url, validate_webhook_url};
use vrc_photon_core::model::{
    CompressionFormat, CompressionProfile, SendErrorCode, SendHistoryEntry, SendKind, SendSource,
    SendStatus,
};
use vrc_photon_core::settings::DiscordAutoMode;
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;
use crate::app::daemon::{format_timestamp, open_path};
use crate::ui::components::{glass_panel, manual_option_row};
use crate::ui::theme::*;

/// Phase 2 adds the automatic-send section to this body, so the bound is
/// generous enough that the newer content is not clipped by the animation.
const DISCORD_BODY_MAX: f32 = 900.0;

/// The database read and the card both cap the history at the newest rows;
/// the design keeps 500 terminal rows and shows the latest 100.
const SEND_HISTORY_LIMIT: usize = 100;

/// The history scrolls above this height so the card stays usable inside the
/// integrations column.
const SEND_HISTORY_MAX: f32 = 340.0;

/// Badge label for a send outbox status.
pub(crate) fn send_status_label(status: SendStatus) -> String {
    match status {
        SendStatus::Waiting => tr!("send.status.waiting"),
        SendStatus::Pending => tr!("send.status.pending"),
        SendStatus::Processing => tr!("send.status.processing"),
        SendStatus::Sent => tr!("send.status.sent"),
        SendStatus::Failed => tr!("send.status.failed"),
        SendStatus::Cancelled => tr!("send.status.cancelled"),
        SendStatus::Unknown => tr!("send.status.unknown"),
    }
}

/// History label for the image a send uploaded.
pub(crate) fn send_source_label(source: SendSource) -> String {
    match source {
        SendSource::Original => tr!("integrations.discord.source.original"),
        SendSource::Profile => tr!("integrations.discord.source.profile"),
        SendSource::Derivative => tr!("integrations.discord.source.derivative"),
    }
}

/// Translated failure reason for a stable `error_code`. Rows without a code
/// (still queued, or sent) render nothing; a code written by a newer version
/// falls back to the unknown label instead of leaking the raw token.
pub(crate) fn send_error_label(code: Option<&str>) -> String {
    let Some(code) = code.map(str::trim).filter(|code| !code.is_empty()) else {
        return String::new();
    };
    match SendErrorCode::parse(code) {
        SendErrorCode::Network => tr!("send.error.network"),
        SendErrorCode::RateLimited => tr!("send.error.rate_limited"),
        SendErrorCode::Http400 => tr!("send.error.http_400"),
        SendErrorCode::Http401 => tr!("send.error.http_401"),
        SendErrorCode::Http403 => tr!("send.error.http_403"),
        SendErrorCode::Http404 => tr!("send.error.http_404"),
        SendErrorCode::Http413 => tr!("send.error.http_413"),
        SendErrorCode::Http5xx => tr!("send.error.http_5xx"),
        SendErrorCode::TooLarge => tr!("send.error.too_large"),
        SendErrorCode::PhotoMissing => tr!("send.error.photo_missing"),
        SendErrorCode::ProfileMissing => tr!("send.error.profile_missing"),
        SendErrorCode::EncodeFailed => tr!("send.error.encode_failed"),
        SendErrorCode::Interrupted => tr!("send.error.interrupted"),
        SendErrorCode::WebhookInvalid => tr!("send.error.webhook_invalid"),
        SendErrorCode::Unknown => tr!("send.error.unknown"),
    }
}

/// Discord deep link for a sent message. Guild, channel, and message are all
/// required; the IDs must be decimal snowflakes so a malformed or hostile
/// database value cannot become an arbitrary URL.
pub(crate) fn discord_message_url(
    guild_id: Option<&str>,
    channel_id: Option<&str>,
    message_id: &str,
) -> Option<String> {
    fn snowflake(value: &str) -> Option<&str> {
        let value = value.trim();
        (!value.is_empty() && value.chars().all(|c| c.is_ascii_digit())).then_some(value)
    }

    let guild = snowflake(guild_id?)?;
    let channel = snowflake(channel_id?)?;
    let message = snowflake(message_id)?;
    Some(format!(
        "https://discord.com/channels/{guild}/{channel}/{message}"
    ))
}

/// Status colours matching the compression queue badges so the two history
/// lists read the same way.
fn send_status_colors(status: SendStatus, p: &Palette) -> (u32, u32) {
    match status {
        SendStatus::Waiting => (p.amber, p.amber_soft),
        SendStatus::Pending => (p.muted, p.panel_2),
        SendStatus::Processing => (p.accent, p.accent_soft),
        SendStatus::Sent => (p.green, p.green_soft),
        SendStatus::Failed => (p.red, p.red_soft),
        SendStatus::Cancelled | SendStatus::Unknown => (p.muted, p.panel_2),
    }
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

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

/// Warning shown when the selected profile encodes a format Discord embeds do
/// not render. Selection is not blocked: the upload still works and Discord
/// may show a download link instead of an inline preview.
pub(crate) fn discord_profile_format_warning(
    profile: Option<&CompressionProfile>,
) -> Option<String> {
    let profile = profile?;
    matches!(
        profile.format,
        CompressionFormat::Avif | CompressionFormat::JpegXl
    )
    .then(|| tr!("integrations.discord.profile.format_warning"))
}

/// Localized "retry at" line for a pending row carrying `next_attempt_at`.
pub(crate) fn retry_at_label(next_attempt_at: Option<i64>) -> Option<String> {
    let timestamp = next_attempt_at?;
    let local = chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)?
        .with_timezone(&chrono::Local)
        .format("%m/%d %H:%M")
        .to_string();
    Some(tr_args!(
        "integrations.discord.history.retry_at",
        time = local
    ))
}

/// Inline message for a stored webhook that no longer validates.
fn stored_webhook_error(url: Option<&str>) -> Option<String> {
    url.filter(|url| validate_webhook_url(url).is_err())
        .map(|_| tr!("integrations.discord.webhook.invalid_url"))
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
        let can_test = enabled
            && webhook
                .as_deref()
                .is_some_and(|url| validate_webhook_url(url).is_ok());
        let stored_webhook_error = stored_webhook_error(webhook.as_deref());
        let send_original = self.settings.discord_send_original;
        let include_players = self.settings.discord_include_players;
        let profiles = self.profiles.clone();
        let selected_discord_profile = profiles.iter().find(|profile| profile.discord);
        let selected_profile = selected_discord_profile.map(|profile| profile.name.clone());
        let format_warning = discord_profile_format_warning(selected_discord_profile);

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
            .when_some(stored_webhook_error, |this, error| {
                this.child(
                    div()
                        .px(px(4.))
                        .text_size(px(10.))
                        .text_color(rgb(p.red))
                        .child(error),
                )
            })
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
                tr!("integrations.discord.source.prefer_original"),
                tr!("integrations.discord.source.prefer_original_hint"),
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
                    .when_some(format_warning, |this, warning| {
                        this.child(
                            div()
                                .p(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(p.amber_soft))
                                .text_size(px(10.))
                                .text_color(rgb(p.amber))
                                .child(warning),
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
                                let selectable = profile.enabled;
                                manual_option_row(
                                    &p,
                                    SharedString::from(format!("discord-profile-{}", profile.id)),
                                    profile.name.clone(),
                                    profile_summary(&profile),
                                    selected,
                                    selectable,
                                )
                                .when(selectable, |this| {
                                    this.on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_discord_profile(&id, !selected, cx);
                                    }))
                                })
                                .into_any_element()
                            }))
                            .into_any_element()
                    }),
            )
            .child(self.discord_auto_section(cx));

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

    /// Send history: the newest rows plus the retry/cancel actions and the
    /// dialog that clears terminal rows. The card has no enable switch because
    /// the history is observational; it follows the Discord feature state but
    /// stays readable after the feature is turned off.
    pub(crate) fn send_history_card(
        &mut self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let history: Vec<SendHistoryEntry> = self
            .send_history
            .iter()
            .take(SEND_HISTORY_LIMIT)
            .cloned()
            .collect();
        let waiting = self.send_waiting_count;

        glass_panel(&self.palette)
            .flex()
            .flex_col()
            .p(px(14.))
            .rounded(px(17.))
            .gap(px(10.))
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
                                    .child(tr!("integrations.discord.history.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted_2))
                                    .child(tr!("integrations.discord.history.hint")),
                            ),
                    )
                    .when(waiting > 0, |this| {
                        this.child(self.badge(
                            format!("{} {waiting}", tr!("send.status.waiting")),
                            p.amber,
                            p.amber_soft,
                        ))
                    })
                    .child(
                        Button::new("discord-history-release")
                            .label(tr!("integrations.discord.history.release"))
                            .small()
                            .primary()
                            .disabled(waiting == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.release_waiting_sends(cx);
                            })),
                    )
                    .child(
                        Button::new("discord-history-clear")
                            .label(tr!("integrations.discord.history.clear"))
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.clear_send_history(window, cx);
                            })),
                    ),
            )
            .when(waiting > 0, |this| {
                this.child(
                    div()
                        .px(px(4.))
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child(tr!("integrations.discord.history.release_hint")),
                )
            })
            .child(if history.is_empty() {
                div()
                    .px(px(4.))
                    .py(px(8.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("integrations.discord.history.empty"))
                    .into_any_element()
            } else {
                v_flex()
                    .id("integrations-send-history")
                    .max_h(px(SEND_HISTORY_MAX))
                    .overflow_y_scrollbar()
                    .children(
                        history
                            .into_iter()
                            .map(|entry| self.send_history_row(entry, cx)),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    fn send_history_row(&self, entry: SendHistoryEntry, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let item = &entry.entry;
        let status = item.status;
        let (fg, bg) = send_status_colors(status, &p);
        let name = match item.kind {
            SendKind::Test => tr!("integrations.discord.history.test_label"),
            SendKind::Photo => entry
                .photo_name
                .clone()
                .and_then(non_empty)
                .unwrap_or_else(|| match item.photo_id {
                    Some(photo_id) => tr_args!("compression.job.photo", id = photo_id),
                    None => tr!("integrations.discord.history.unknown_photo"),
                }),
        };
        let world = entry.world_name.clone().and_then(non_empty);
        let destination = item.destination.clone().and_then(non_empty);
        let source = item.source.map(send_source_label);
        // The finish time is the useful one for terminal rows; queued rows
        // only have a creation time.
        let timestamp = item
            .finished_at
            .as_deref()
            .or(item.started_at.as_deref())
            .unwrap_or(&item.created_at);
        let message = item.discord_message_id.clone().and_then(non_empty);
        let message_url = message.as_deref().and_then(|message| {
            discord_message_url(
                item.discord_guild_id.as_deref(),
                item.discord_channel_id.as_deref(),
                message,
            )
        });
        let error = send_error_label(item.error_code.as_deref());
        let detail = item.error_detail.clone().and_then(non_empty);
        let retry_at = (status == SendStatus::Pending)
            .then(|| retry_at_label(item.next_attempt_at))
            .flatten();
        let retryable = status == SendStatus::Failed;
        let cancellable = matches!(
            status,
            SendStatus::Waiting | SendStatus::Pending | SendStatus::Failed
        );
        let id = item.id;

        v_flex()
            .px(px(4.))
            .py(px(9.))
            .gap(px(5.))
            .border_b_1()
            .border_color(rgb(p.border))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(self.badge(send_status_label(status), fg, bg))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(rgb(p.text))
                            .truncate()
                            .child(name),
                    )
                    .when_some(world, |this, world| {
                        this.child(
                            div()
                                .max_w(px(160.))
                                .truncate()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(world),
                        )
                    })
                    .when_some(destination, |this, destination| {
                        this.child(
                            div()
                                .max_w(px(120.))
                                .truncate()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(destination),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted_2))
                            .child(format_timestamp(timestamp)),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .when_some(source, |this, source| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted))
                                .child(source),
                        )
                    })
                    .when(item.attempts > 0, |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(format!(
                                    "{}: {}",
                                    tr!("integrations.discord.history.attempts"),
                                    item.attempts
                                )),
                        )
                    })
                    .when_some(message, |this, message| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted_2))
                                .child(format!(
                                    "{}: {message}",
                                    tr!("integrations.discord.history.message_id")
                                )),
                        )
                    })
                    .when_some(message_url, |this, url| {
                        this.child(
                            Button::new(SharedString::from(format!("send-message-{id}")))
                                .label(tr!("integrations.discord.history.message_link"))
                                .small()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Err(error) = open_path(Path::new(&url)) {
                                        tracing::warn!(%error, %url, "cannot open a Discord message");
                                        this.toast_error(error, cx);
                                    }
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .when(retryable, |this| {
                        this.child(
                            Button::new(SharedString::from(format!("send-retry-{id}")))
                                .label(tr!("integrations.discord.history.retry"))
                                .small()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.retry_send(id, cx);
                                })),
                        )
                    })
                    .when(cancellable, |this| {
                        this.child(
                            Button::new(SharedString::from(format!("send-cancel-{id}")))
                                .label(tr!("integrations.discord.history.cancel"))
                                .small()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.cancel_send(id, cx);
                                })),
                        )
                    }),
            )
            .when_some(retry_at, |this, retry_at| {
                this.child(
                    div()
                        .w_full()
                        .text_size(px(10.))
                        .text_color(rgb(p.amber))
                        .child(retry_at),
                )
            })
            .when(!error.is_empty(), |this| {
                this.child(
                    div()
                        .w_full()
                        .text_size(px(10.))
                        .text_color(rgb(p.red))
                        .child(error),
                )
            })
            .when_some(detail, |this, detail| {
                this.child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child(detail),
                )
            })
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

    /// Off / Immediate / Waiting. The first switch away from Off opens the
    /// consent dialog from `App::set_discord_auto_mode`; the note below the
    /// rows repeats the privacy caveat next to the controls that cause it.
    fn discord_auto_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let current = self.settings.discord_auto_mode;
        let options = [
            (
                DiscordAutoMode::Off,
                "discord-auto-off",
                tr!("integrations.discord.auto.off"),
                tr!("integrations.discord.auto.off_hint"),
            ),
            (
                DiscordAutoMode::Immediate,
                "discord-auto-immediate",
                tr!("integrations.discord.auto.immediate"),
                tr!("integrations.discord.auto.immediate_hint"),
            ),
            (
                DiscordAutoMode::Waiting,
                "discord-auto-waiting",
                tr!("integrations.discord.auto.waiting"),
                tr!("integrations.discord.auto.waiting_hint"),
            ),
        ];

        v_flex()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(p.text))
                    .child(tr!("integrations.discord.auto.title")),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("integrations.discord.auto.hint")),
            )
            .children(options.into_iter().map(|(mode, id, title, description)| {
                manual_option_row(
                    &p,
                    SharedString::from(id),
                    title,
                    description,
                    current == mode,
                    true,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_discord_auto_mode(mode, window, cx);
                }))
                .into_any_element()
            }))
            .when(current != DiscordAutoMode::Off, |this| {
                this.child(
                    div()
                        .p(px(8.))
                        .rounded(px(8.))
                        .bg(rgb(p.amber_soft))
                        .text_size(px(10.))
                        .text_color(rgb(p.amber))
                        .child(tr!("warning.private_instances")),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        discord_message_url, discord_profile_format_warning, discord_profile_label, retry_at_label,
        send_error_label, send_source_label, send_status_label, stored_webhook_error,
        webhook_state_label,
    };
    use vrc_photon_core::model::{CompressionFormat, CompressionProfile, SendSource, SendStatus};
    use vrc_photon_i18n::{tr, tr_args};

    #[test]
    fn history_helpers_describe_status_and_source() {
        assert_eq!(send_status_label(SendStatus::Sent), tr!("send.status.sent"));
        assert_eq!(
            send_source_label(SendSource::Derivative),
            tr!("integrations.discord.source.derivative")
        );
        assert_eq!(
            send_error_label(Some("http_401")),
            tr!("send.error.http_401")
        );
        assert_eq!(send_error_label(None), "");
    }

    #[test]
    fn message_url_is_built_only_with_guild_and_channel() {
        assert_eq!(
            discord_message_url(Some("9"), Some("7"), "42").as_deref(),
            Some("https://discord.com/channels/9/7/42")
        );
        assert_eq!(discord_message_url(None, Some("7"), "42"), None);
    }

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

    #[test]
    fn avif_and_jxl_profiles_warn_that_embeds_may_not_render() {
        let avif = CompressionProfile {
            format: CompressionFormat::Avif,
            ..CompressionProfile::default()
        };
        assert!(discord_profile_format_warning(Some(&avif)).is_some());
        let jxl = CompressionProfile {
            format: CompressionFormat::JpegXl,
            ..CompressionProfile::default()
        };
        assert!(discord_profile_format_warning(Some(&jxl)).is_some());
        let webp = CompressionProfile {
            format: CompressionFormat::Webp,
            ..CompressionProfile::default()
        };
        assert!(discord_profile_format_warning(Some(&webp)).is_none());
        assert!(discord_profile_format_warning(None).is_none());
    }

    #[test]
    fn pending_rows_get_a_localized_retry_time() {
        assert_eq!(retry_at_label(None), None);
        let local = chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m/%d %H:%M")
            .to_string();
        assert_eq!(
            retry_at_label(Some(0)),
            Some(tr_args!(
                "integrations.discord.history.retry_at",
                time = local
            ))
        );
    }

    #[test]
    fn stored_webhooks_that_no_longer_validate_show_the_inline_error() {
        assert!(stored_webhook_error(None).is_none());
        assert!(stored_webhook_error(Some("https://discord.com/api/webhooks/1/token")).is_none());
        assert_eq!(
            stored_webhook_error(Some("http://discord.com/api/webhooks/1/token")),
            Some(tr!("integrations.discord.webhook.invalid_url"))
        );
    }
}
