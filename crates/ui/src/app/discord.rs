//! App-side Discord integration state: webhook editing, destination label,
//! integration toggles, profile selection, automatic-send mode and consent,
//! and send-history refresh.
//!
//! Rendering stays in `ui::discord`; these methods only touch App fields and
//! the send outbox.

use chrono::Utc;
use gpui_kit::component::WindowExt;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::*;
use vrc_photon_core::discord::{WebhookUrlError, validate_webhook_url};
use vrc_photon_core::model::{PhotoStatus, SendKind, SendOrigin, SendStatus};
use vrc_photon_core::settings::{
    AppSettings, DiscordAutoMode, clear_discord_webhook, save_discord_webhook,
};
use vrc_photon_i18n::{tr, tr_args};

use crate::app::{App, Screen};
use crate::state::motion::ToggleMotion;
use crate::ui::theme::MOTION_FAST;

/// Label used when the user leaves the destination field blank.
const DEFAULT_DESTINATION: &str = "Discord";

/// Why a manual or test send cannot start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscordSendGuardError {
    Disabled,
    MissingWebhook,
}

/// Blocks a send until the feature is enabled and a webhook is stored.
pub(crate) fn discord_send_guard(
    settings: &AppSettings,
    webhook_present: bool,
) -> Result<(), DiscordSendGuardError> {
    if !settings.discord_enabled {
        return Err(DiscordSendGuardError::Disabled);
    }
    if !webhook_present {
        return Err(DiscordSendGuardError::MissingWebhook);
    }
    Ok(())
}

/// Toast text for a manual send.
///
/// `Ok(true)` means the send was queued and is shown as a success toast;
/// `Ok(false)` means an active send for the photo already exists and is shown
/// as an info toast. A guard error maps to the warning shown before the user is
/// sent to the Integrations screen.
pub(crate) fn manual_send_toast(result: Result<bool, DiscordSendGuardError>) -> String {
    match result {
        Ok(true) => tr!("photos.action.discord_queued"),
        Ok(false) => tr!("photos.action.discord_waiting"),
        Err(DiscordSendGuardError::Disabled) => tr!("photos.action.discord_disabled"),
        Err(DiscordSendGuardError::MissingWebhook) => tr!("photos.action.discord_missing_webhook"),
    }
}

/// Trims the destination label and falls back to `Discord` when blank, the
/// same normalization `AppSettings` applies when it is loaded or saved.
pub(crate) fn normalized_destination(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        DEFAULT_DESTINATION.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Catalog key for an inline webhook validation error.
pub(crate) fn webhook_error_key(error: WebhookUrlError) -> &'static str {
    match error {
        WebhookUrlError::HostNotAllowed => "integrations.discord.webhook.invalid_host",
        WebhookUrlError::InvalidPath => "integrations.discord.webhook.invalid_path",
        WebhookUrlError::Empty
        | WebhookUrlError::Invalid
        | WebhookUrlError::NotHttps
        | WebhookUrlError::PortNotAllowed => "integrations.discord.webhook.invalid_url",
    }
}

/// Automatic sending needs an explicit opt-in once. The stored timestamp is
/// the only signal, so upgrades never show the dialog again after consent.
pub(crate) fn needs_discord_consent(settings: &AppSettings) -> bool {
    settings.discord_consent_at.is_none()
}

/// Applies an automatic-send mode in place.
///
/// `was_off` is the mode observed before the click. The capture-time cutoff
/// only moves when automatic sending leaves `Off`, so switching between
/// `Immediate` and `Waiting` keeps the backlog boundary, and turning the mode
/// off never clears it.
pub(crate) fn apply_discord_auto_mode(
    settings: &mut AppSettings,
    mode: DiscordAutoMode,
    now: i64,
    was_off: bool,
) {
    settings.discord_auto_mode = mode;
    if mode != DiscordAutoMode::Off && was_off {
        settings.discord_auto_since = Some(now);
    }
}

impl App {
    pub(crate) fn set_discord_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.discord_enabled == enabled {
            return;
        }
        self.settings.discord_enabled = enabled;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save Discord integration setting");
        }
        // Only the card header animates here. Turning the feature on does not
        // change the automatic mode; that is a separate, consent-gated toggle.
        let target = if enabled { 0.0 } else { 1.0 };
        if cx.reduce_motion() {
            self.integrations_discord_motion = ToggleMotion::settled(target);
        } else {
            self.integrations_discord_motion
                .animate_to(target, MOTION_FAST);
        }
        cx.notify();
    }

    pub(crate) fn begin_discord_url_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.discord_url_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.discord_url_error = None;
        self.discord_url_editing = true;
        cx.notify();
    }

    pub(crate) fn save_discord_url(&mut self, cx: &mut Context<Self>) {
        let raw = self.discord_url_input.read(cx).value().trim().to_string();
        match validate_webhook_url(&raw) {
            Ok(parsed) => {
                let url = parsed.as_str().to_string();
                if let Err(error) = save_discord_webhook(&self.db, &url) {
                    tracing::warn!(%error, "cannot save the Discord webhook URL");
                    self.toast_error(tr_args!("settings.language.save_failed", error = error), cx);
                    cx.notify();
                    return;
                }
                self.discord_url = Some(url);
                self.discord_url_editing = false;
                self.discord_url_error = None;
                self.toast_success(tr!("integrations.discord.webhook.saved"), cx);
            }
            Err(error) => {
                // Never echo the URL itself: it contains the webhook token.
                self.discord_url_error = Some(tr(webhook_error_key(error)).into());
            }
        }
        cx.notify();
    }

    pub(crate) fn clear_discord_url(&mut self, cx: &mut Context<Self>) {
        match clear_discord_webhook(&self.db) {
            Ok(()) => {
                self.discord_url = None;
                self.discord_url_editing = false;
                self.discord_url_error = None;
                self.toast_success(tr!("integrations.discord.webhook.removed"), cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot clear the Discord webhook URL");
                self.toast_error(tr_args!("settings.language.save_failed", error = error), cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn save_discord_destination(&mut self, cx: &mut Context<Self>) {
        let raw = self.discord_destination_input.read(cx).value();
        let destination = normalized_destination(&raw);
        if self.settings.discord_destination != destination {
            self.settings.discord_destination = destination;
            if let Err(error) = self.settings.save(&self.db) {
                tracing::warn!(%error, "cannot save the Discord destination");
            }
        }
        cx.notify();
    }

    pub(crate) fn set_discord_send_original(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.settings.discord_send_original == value {
            return;
        }
        self.settings.discord_send_original = value;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save the Discord original-image setting");
        }
        cx.notify();
    }

    pub(crate) fn set_discord_include_players(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.settings.discord_include_players == value {
            return;
        }
        self.settings.discord_include_players = value;
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save the Discord player-name setting");
        }
        cx.notify();
    }

    /// Switches the automatic-send mode.
    ///
    /// Turning it off also cancels queued waiting rows, because those are the
    /// automatic sends the user no longer wants. The first switch away from
    /// `Off` asks for consent and only applies the mode when the dialog is
    /// accepted.
    pub(crate) fn set_discord_auto_mode(
        &mut self,
        mode: DiscordAutoMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_off = self.settings.discord_auto_mode == DiscordAutoMode::Off;

        if mode == DiscordAutoMode::Off {
            self.settings.discord_auto_mode = DiscordAutoMode::Off;
            if let Err(error) = self.settings.save(&self.db) {
                tracing::warn!(%error, "cannot save the Discord automatic-send mode");
            }
            match self.db.cancel_waiting_auto_sends() {
                Ok(0) => {}
                Ok(count) => {
                    tracing::info!(count, "cancelled waiting automatic Discord sends");
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot cancel waiting automatic Discord sends");
                }
            }
            self.reload_send_history(cx);
            return;
        }

        if needs_discord_consent(&self.settings) {
            let entity = cx.entity();
            let panel_deep = self.palette.panel_deep;
            window.open_alert_dialog(cx, move |alert, _, _| {
                let entity = entity.clone();
                alert
                    .title(tr!("integrations.discord.auto.consent.title"))
                    .bg(rgb(panel_deep))
                    .description(tr!("integrations.discord.auto.consent.body"))
                    .confirm()
                    .ok_text(tr!("integrations.discord.auto.consent.ok"))
                    .ok_variant(ButtonVariant::Primary)
                    .cancel_text(tr!("common.cancel"))
                    .on_ok(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.confirm_discord_auto_mode(mode, cx));
                        true
                    })
            });
            return;
        }

        self.save_discord_auto_mode(mode, was_off, cx);
    }

    /// Records consent and applies the mode chosen in the consent dialog.
    pub(crate) fn confirm_discord_auto_mode(
        &mut self,
        mode: DiscordAutoMode,
        cx: &mut Context<Self>,
    ) {
        let was_off = self.settings.discord_auto_mode == DiscordAutoMode::Off;
        self.settings.discord_consent_at = Some(Utc::now().timestamp());
        self.save_discord_auto_mode(mode, was_off, cx);
    }

    fn save_discord_auto_mode(
        &mut self,
        mode: DiscordAutoMode,
        was_off: bool,
        cx: &mut Context<Self>,
    ) {
        apply_discord_auto_mode(&mut self.settings, mode, Utc::now().timestamp(), was_off);
        if let Err(error) = self.settings.save(&self.db) {
            tracing::warn!(%error, "cannot save the Discord automatic-send mode");
        }
        cx.notify();
    }

    /// Refreshes the history page and the waiting-queue badge. A queued test
    /// send reports its result once its row reaches a terminal state.
    pub(crate) fn reload_send_history(&mut self, cx: &mut Context<Self>) {
        match self.db.send_history(100) {
            Ok(history) => self.send_history = history,
            Err(error) => tracing::warn!(%error, "cannot load the Discord send history"),
        }
        match self.db.count_waiting_sends() {
            Ok(count) => self.send_waiting_count = count,
            Err(error) => tracing::warn!(%error, "cannot count waiting Discord sends"),
        }
        if let Some(id) = self.pending_test_send {
            // The row was just enqueued, so it leads the page; the direct read
            // only covers a failed history query.
            let status = self
                .send_history
                .iter()
                .find(|entry| entry.entry.id == id)
                .map(|entry| entry.entry.status)
                .or_else(|| {
                    self.db
                        .send_entry_by_id(id)
                        .ok()
                        .flatten()
                        .map(|entry| entry.status)
                });
            match status {
                Some(SendStatus::Sent) => {
                    self.pending_test_send = None;
                    self.toast_success(tr!("integrations.discord.test.success"), cx);
                }
                Some(SendStatus::Failed) => {
                    self.pending_test_send = None;
                    self.toast_error(tr!("integrations.discord.test.failed"), cx);
                }
                // A cancelled test send is not a webhook failure; the cancel
                // action already reported success, so clear it silently.
                Some(SendStatus::Cancelled) => {
                    self.pending_test_send = None;
                }
                _ => {}
            }
        }
        cx.notify();
    }

    pub(crate) fn send_test_message(&mut self, cx: &mut Context<Self>) {
        match self.db.enqueue_test_send() {
            Ok(id) => {
                self.pending_test_send = Some(id);
                self.reload_send_history(cx);
                self.send_wake_sends(cx);
                self.toast_info(tr!("integrations.discord.test.queued"), cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot enqueue a Discord test send");
                self.toast_error(tr!("integrations.discord.test.enqueue_failed"), cx);
            }
        }
        cx.notify();
    }

    /// Queues one photo for a manual Discord send.
    ///
    /// Returns `true` when the integration guard rejected the send and the app
    /// navigated to the Integrations screen. A modal viewer uses that to close
    /// itself and reveal the screen; every other outcome returns `false`.
    pub(crate) fn enqueue_photo_send(&mut self, photo_id: i64, cx: &mut Context<Self>) -> bool {
        if let Err(error) = discord_send_guard(&self.settings, self.discord_url.is_some()) {
            self.toast_warning(manual_send_toast(Err(error)), cx);
            self.switch_screen(Screen::Integrations, cx);
            return true;
        }
        match self.db.photo_by_id(photo_id) {
            // A missing photo has no source to read, so it shares the rejection
            // path with a photo that is being deleted.
            Ok(Some(photo))
                if !matches!(photo.status, PhotoStatus::Deleting | PhotoStatus::Missing) => {}
            Ok(Some(photo)) => {
                let message = match photo.status {
                    PhotoStatus::Deleting => tr!("photos.action.discord_deleting"),
                    _ => tr!("photos.action.discord_error"),
                };
                self.toast_error(message, cx);
                cx.notify();
                return false;
            }
            Ok(None) => {
                self.toast_error(tr!("photos.action.discord_error"), cx);
                cx.notify();
                return false;
            }
            Err(error) => {
                tracing::warn!(%error, photo_id, "cannot load a photo for a Discord send");
                self.toast_error(
                    format!("{}: {error}", tr!("photos.action.discord_error")),
                    cx,
                );
                cx.notify();
                return false;
            }
        }
        match self.db.enqueue_send(
            photo_id,
            SendKind::Photo,
            SendOrigin::Manual,
            SendStatus::Pending,
        ) {
            Ok(Some(_)) => {
                self.reload_send_history(cx);
                self.send_wake_sends(cx);
                self.toast_success(manual_send_toast(Ok(true)), cx);
            }
            // An active row for this photo already exists; the worker owns it.
            Ok(None) => self.toast_info(manual_send_toast(Ok(false)), cx),
            Err(error) => {
                tracing::warn!(%error, photo_id, "cannot enqueue a Discord photo send");
                self.toast_error(
                    format!("{}: {error}", tr!("photos.action.discord_error")),
                    cx,
                );
            }
        }
        cx.notify();
        false
    }

    /// Returns a failed row to the queue. The database rejects the retry when
    /// the photo already has an active send, which the warning toast covers.
    pub(crate) fn retry_send(&mut self, id: i64, cx: &mut Context<Self>) {
        match self.db.retry_send(id) {
            Ok(true) => {
                self.reload_send_history(cx);
                self.send_wake_sends(cx);
                self.toast_success(tr!("integrations.discord.history.retry.queued"), cx);
            }
            Ok(false) => {
                self.reload_send_history(cx);
                self.toast_warning(tr!("integrations.discord.history.retry.unavailable"), cx);
            }
            Err(error) => {
                tracing::warn!(%error, id, "cannot retry a Discord send");
                self.toast_error(
                    format!(
                        "{}: {error}",
                        tr!("integrations.discord.history.retry.error")
                    ),
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Cancels a queued or failed row. `processing` is not cancellable because
    /// the upload is already in flight; the warning toast covers a row that
    /// changed state after the click.
    pub(crate) fn cancel_send(&mut self, id: i64, cx: &mut Context<Self>) {
        match self.db.cancel_send(id) {
            Ok(true) => {
                self.reload_send_history(cx);
                self.toast_success(tr!("integrations.discord.history.cancel.done"), cx);
            }
            Ok(false) => {
                self.reload_send_history(cx);
                self.toast_warning(tr!("integrations.discord.history.cancel.unavailable"), cx);
            }
            Err(error) => {
                tracing::warn!(%error, id, "cannot cancel a Discord send");
                self.toast_error(
                    format!(
                        "{}: {error}",
                        tr!("integrations.discord.history.cancel.error")
                    ),
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Asks before deleting every terminal row. Active sends stay in the
    /// outbox, so clearing history never drops pending work.
    pub(crate) fn clear_send_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.entity();
        let panel_deep = self.palette.panel_deep;
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            alert
                .title(tr!("integrations.discord.history.clear.title"))
                .bg(rgb(panel_deep))
                .description(tr!("integrations.discord.history.clear.description"))
                .confirm()
                .ok_text(tr!("integrations.discord.history.clear.ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(tr!("common.cancel"))
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.delete_send_history(cx));
                    true
                })
        });
    }

    fn delete_send_history(&mut self, cx: &mut Context<Self>) {
        match self.db.clear_send_history() {
            Ok(_) => {
                self.reload_send_history(cx);
                self.toast_success(tr!("integrations.discord.history.clear.done"), cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot clear the Discord send history");
                self.toast_error(
                    format!(
                        "{}: {error}",
                        tr!("integrations.discord.history.clear.error")
                    ),
                    cx,
                );
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DiscordSendGuardError, apply_discord_auto_mode, discord_send_guard, manual_send_toast,
        needs_discord_consent, normalized_destination, webhook_error_key,
    };
    use vrc_photon_core::discord::{WebhookUrlError, mask_webhook_url, validate_webhook_url};
    use vrc_photon_core::settings::{AppSettings, DiscordAutoMode};
    use vrc_photon_i18n::tr;

    #[test]
    fn enabling_auto_requires_consent_only_the_first_time() {
        let mut settings = AppSettings::default();
        assert!(needs_discord_consent(&settings));
        settings.discord_consent_at = Some(1);
        assert!(!needs_discord_consent(&settings));
    }

    #[test]
    fn enabling_auto_sets_auto_since_only_when_leaving_off() {
        let mut settings = AppSettings::default();
        let now = 1234;
        apply_discord_auto_mode(&mut settings, DiscordAutoMode::Immediate, now, true);
        assert_eq!(settings.discord_auto_since, Some(now));
        assert_eq!(settings.discord_auto_mode, DiscordAutoMode::Immediate);
        apply_discord_auto_mode(&mut settings, DiscordAutoMode::Waiting, 5678, false);
        assert_eq!(settings.discord_auto_since, Some(now)); // 即時→待機ではリセットしない
    }

    #[test]
    fn disabling_auto_keeps_the_capture_cutoff() {
        let mut settings = AppSettings {
            discord_auto_since: Some(42),
            ..AppSettings::default()
        };
        apply_discord_auto_mode(&mut settings, DiscordAutoMode::Off, 99, false);
        assert_eq!(settings.discord_auto_mode, DiscordAutoMode::Off);
        assert_eq!(settings.discord_auto_since, Some(42));
    }

    #[test]
    fn manual_send_reports_queued_and_duplicate_states() {
        assert_eq!(
            manual_send_toast(Ok(true)),
            tr!("photos.action.discord_queued")
        );
        assert_eq!(
            manual_send_toast(Ok(false)),
            tr!("photos.action.discord_waiting")
        );
        assert_eq!(
            manual_send_toast(Err(DiscordSendGuardError::Disabled)),
            tr!("photos.action.discord_disabled")
        );
        assert_eq!(
            manual_send_toast(Err(DiscordSendGuardError::MissingWebhook)),
            tr!("photos.action.discord_missing_webhook")
        );
    }

    #[test]
    fn webhook_mask_and_validation_feed_the_editor() {
        assert!(validate_webhook_url("https://discord.com/api/webhooks/1/t").is_ok());
        assert!(!mask_webhook_url("https://discord.com/api/webhooks/1/secret").contains("secret"));
    }

    #[test]
    fn save_destination_falls_back_to_discord_for_blank_input() {
        assert_eq!(normalized_destination("   "), "Discord");
        assert_eq!(normalized_destination(" My server "), "My server");
    }

    #[test]
    fn discord_send_guard_requires_enabled_and_webhook() {
        let mut settings = AppSettings::default();
        assert_eq!(
            discord_send_guard(&settings, false),
            Err(DiscordSendGuardError::Disabled)
        );
        settings.discord_enabled = true;
        assert_eq!(
            discord_send_guard(&settings, false),
            Err(DiscordSendGuardError::MissingWebhook)
        );
        assert_eq!(discord_send_guard(&settings, true), Ok(()));
    }

    #[test]
    fn webhook_errors_map_to_catalog_keys() {
        assert_eq!(
            webhook_error_key(WebhookUrlError::NotHttps),
            "integrations.discord.webhook.invalid_url"
        );
        assert_eq!(
            webhook_error_key(WebhookUrlError::HostNotAllowed),
            "integrations.discord.webhook.invalid_host"
        );
        assert_eq!(
            webhook_error_key(WebhookUrlError::InvalidPath),
            "integrations.discord.webhook.invalid_path"
        );
    }
}
