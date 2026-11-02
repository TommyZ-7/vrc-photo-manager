//! App-side deletion request actions and labels for the dashboard card.

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::*;
use vrc_photon_core::model::{DeletionReason, DeletionRequest, DeletionStatus};
use vrc_photon_i18n::{tr, tr_args};

use crate::app::App;

pub(crate) fn deletion_reason_label(reason: DeletionReason) -> String {
    match reason {
        DeletionReason::AutoReplace => tr!("deletion.reason.auto_replace"),
        DeletionReason::Manual => tr!("deletion.reason.manual"),
        DeletionReason::Unknown => tr!("deletion.reason.unknown"),
    }
}

pub(crate) fn deletion_status_label(status: DeletionStatus) -> String {
    match status {
        DeletionStatus::Pending => tr!("deletion.status.pending"),
        DeletionStatus::Blocked => tr!("deletion.status.blocked"),
        DeletionStatus::Running => tr!("deletion.status.running"),
        DeletionStatus::Completed => tr!("deletion.status.completed"),
        DeletionStatus::Cancelled => tr!("deletion.status.cancelled"),
    }
}

pub(crate) fn deletion_blocked_label(request: &DeletionRequest) -> Option<String> {
    match request.blocked_reason.as_deref() {
        Some("job_failed") => Some(tr!("deletion.blocked.compression_failed")),
        Some("file_error") => Some(tr_args!(
            "deletion.blocked.move_failed",
            detail = request
                .error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        )),
        Some("missing_asset") => Some(tr!("deletion.blocked.missing_asset")),
        Some("unknown_reason") => Some(tr!("deletion.blocked.unknown_reason")),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeletionActions {
    pub can_cancel: bool,
    pub retryable: bool,
    pub forceable: bool,
}

/// Actions that can make progress for a request. Unknown reasons are blocked
/// before execution regardless of `force`, so retry and force would loop
/// without deleting anything.
pub(crate) fn deletion_actions(request: &DeletionRequest) -> DeletionActions {
    let unknown = request.reason == DeletionReason::Unknown;
    DeletionActions {
        can_cancel: request.reason == DeletionReason::Manual
            && request.started_at.is_none()
            && (request.status == DeletionStatus::Pending
                || request.status == DeletionStatus::Blocked),
        retryable: request.status == DeletionStatus::Blocked
            && request.blocked_reason.as_deref() != Some("job_failed")
            && !unknown,
        forceable: request.status == DeletionStatus::Blocked && !unknown,
    }
}

impl App {
    pub(crate) fn cancel_deletion(&mut self, id: i64, cx: &mut Context<Self>) {
        match self.db.cancel_deletion(id) {
            Ok(true) => {
                if let Err(error) = self.db.bump_library_revision() {
                    tracing::warn!(%error, "cannot bump library revision after deletion cancel");
                }
                let revision = self.db.library_revision().ok();
                self.apply_library_snapshot(revision, cx);
                self.reload_aux(cx);
                self.toast_success(tr!("deletion.cancel.success"), cx);
            }
            Ok(false) => self.toast_warning(tr!("deletion.cancel.not_possible"), cx),
            Err(error) => {
                tracing::warn!(%error, request = id, "cannot cancel deletion");
                self.toast_error(tr_args!("deletion.cancel.error", error = error), cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn retry_deletion(&mut self, id: i64, cx: &mut Context<Self>) {
        match self.db.retry_deletion(id) {
            Ok(true) => {
                self.reload_job_state(cx);
                self.send_wake_jobs(cx);
                self.toast_success(tr!("deletion.retry.success"), cx);
            }
            Ok(false) => self.toast_warning(tr!("deletion.retry.running"), cx),
            Err(error) => {
                tracing::warn!(%error, request = id, "cannot retry deletion");
                self.toast_error(tr_args!("deletion.retry.error", error = error), cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn request_force_deletion(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        let panel_deep = self.palette.panel_deep;
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            alert
                .title(tr!("deletion.force.title"))
                .bg(rgb(panel_deep))
                .description(tr!("deletion.force.description"))
                .confirm()
                .ok_text(tr!("deletion.force.ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(tr!("common.cancel"))
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.force_deletion(id, cx));
                    true
                })
        });
    }

    pub(crate) fn force_deletion(&mut self, id: i64, cx: &mut Context<Self>) {
        match self.db.force_deletion(id) {
            Ok(true) => {
                if let Err(error) = self.db.bump_library_revision() {
                    tracing::warn!(%error, "cannot bump library revision after force deletion");
                }
                self.reload_job_state(cx);
                let revision = self.db.library_revision().ok();
                self.apply_library_snapshot(revision, cx);
                self.reload_aux(cx);
                self.send_wake_jobs(cx);
                self.toast_success(tr!("deletion.force.started"), cx);
            }
            Ok(false) => self.toast_warning(tr!("deletion.retry.running"), cx),
            Err(error) => {
                tracing::warn!(%error, request = id, "cannot force deletion");
                self.toast_error(tr_args!("deletion.force.error", error = error), cx);
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeletionActions, deletion_actions, deletion_blocked_label, deletion_reason_label,
        deletion_status_label,
    };
    use vrc_photon_core::model::{DeletionReason, DeletionRequest, DeletionStatus};

    #[test]
    fn unknown_reason_is_visible_and_hides_unusable_actions() {
        let mut request = sample_request(DeletionStatus::Blocked);
        request.reason = DeletionReason::Unknown;
        request.blocked_reason = Some("unknown_reason".to_string());
        assert_eq!(
            deletion_blocked_label(&request).as_deref(),
            Some("新しいバージョンで追加された削除理由のため保留中。アプリを更新してください")
        );
        assert_eq!(
            deletion_actions(&request),
            DeletionActions {
                can_cancel: false,
                retryable: false,
                forceable: false,
            }
        );

        let mut manual = sample_request(DeletionStatus::Blocked);
        manual.blocked_reason = Some("file_error".to_string());
        assert_eq!(
            deletion_actions(&manual),
            DeletionActions {
                can_cancel: true,
                retryable: true,
                forceable: true,
            }
        );
    }

    #[test]
    fn deletion_labels_cover_every_status_and_reason() {
        assert_eq!(
            deletion_reason_label(DeletionReason::AutoReplace),
            "自動置換"
        );
        assert_eq!(deletion_reason_label(DeletionReason::Manual), "手動削除");
        assert_eq!(deletion_status_label(DeletionStatus::Pending), "待機中");
        assert_eq!(deletion_status_label(DeletionStatus::Blocked), "保留");
        assert_eq!(deletion_status_label(DeletionStatus::Running), "実行中");
        assert_eq!(deletion_status_label(DeletionStatus::Completed), "完了");
        assert_eq!(deletion_status_label(DeletionStatus::Cancelled), "取消");
    }

    #[test]
    fn blocked_label_explains_job_and_file_failures() {
        let mut request = sample_request(DeletionStatus::Blocked);
        request.blocked_reason = Some("job_failed".to_string());
        assert_eq!(
            deletion_blocked_label(&request).as_deref(),
            Some("圧縮ジョブの失敗で保留中（圧縮画面で再試行してください）")
        );
        request.blocked_reason = Some("file_error".to_string());
        request.error = Some("permission denied".to_string());
        assert!(
            deletion_blocked_label(&request)
                .unwrap()
                .contains("permission denied")
        );
        request.blocked_reason = Some("missing_asset".to_string());
        assert!(deletion_blocked_label(&request).unwrap().contains("圧縮版"));
    }

    fn sample_request(status: DeletionStatus) -> DeletionRequest {
        DeletionRequest {
            id: 1,
            photo_id: 1,
            photo_name: "a.png".to_string(),
            reason: DeletionReason::Manual,
            status,
            force: false,
            blocked_reason: None,
            error: None,
            requested_at: "2026-10-29T00:00:00+00:00".to_string(),
            started_at: None,
            finished_at: None,
        }
    }
}
