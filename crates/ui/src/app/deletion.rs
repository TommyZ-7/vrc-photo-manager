//! App-side deletion request actions and labels for the dashboard card.

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::*;
use vrc_photo_core::model::{DeletionReason, DeletionRequest, DeletionStatus};

use crate::app::App;

pub(crate) fn deletion_reason_label(reason: DeletionReason) -> &'static str {
    match reason {
        DeletionReason::AutoReplace => "自動置換",
        DeletionReason::Manual => "手動削除",
    }
}

pub(crate) fn deletion_status_label(status: DeletionStatus) -> &'static str {
    match status {
        DeletionStatus::Pending => "待機中",
        DeletionStatus::Blocked => "保留",
        DeletionStatus::Running => "実行中",
        DeletionStatus::Completed => "完了",
        DeletionStatus::Cancelled => "取消",
    }
}

pub(crate) fn deletion_blocked_label(request: &DeletionRequest) -> Option<String> {
    match request.blocked_reason.as_deref() {
        Some("job_failed") => {
            Some("圧縮ジョブの失敗で保留中（圧縮画面で再試行してください）".to_string())
        }
        Some("file_error") => Some(format!(
            "ファイルをゴミ箱へ移動できませんでした{}",
            request
                .error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        )),
        Some("missing_asset") => Some("表示用の圧縮版が見つからないため保留中".to_string()),
        _ => None,
    }
}

impl App {
    pub(crate) fn cancel_deletion(&mut self, id: i64, cx: &mut Context<Self>) {
        match self.db.cancel_deletion(id) {
            Ok(true) => {
                if let Err(error) = self.db.bump_library_revision() {
                    tracing::warn!(%error, "cannot bump library revision after deletion cancel");
                }
                self.reload_library(true, cx);
                self.reload_aux(cx);
                self.toast_success("削除待ちを取り消しました", cx);
            }
            Ok(false) => self.toast_warning("この削除は取り消せません", cx),
            Err(error) => {
                tracing::warn!(%error, request = id, "cannot cancel deletion");
                self.toast_error(format!("削除待ちを取り消せません: {error}"), cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn retry_deletion(&mut self, id: i64, cx: &mut Context<Self>) {
        match self.db.retry_deletion(id) {
            Ok(true) => {
                self.reload_job_state(cx);
                self.send_wake_jobs(cx);
                self.toast_success("削除を再試行します", cx);
            }
            Ok(false) => self.toast_warning("削除は既に実行中です", cx),
            Err(error) => {
                tracing::warn!(%error, request = id, "cannot retry deletion");
                self.toast_error(format!("削除を再試行できません: {error}"), cx);
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
                .title("強制削除")
                .bg(rgb(panel_deep))
                .description(
                    "失敗したジョブを無視して削除します。この写真が唯一のコピーの場合、写真を失う可能性があります。",
                )
                .confirm()
                .ok_text("強制削除")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("キャンセル")
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
                self.reload_library(true, cx);
                self.reload_aux(cx);
                self.send_wake_jobs(cx);
                self.toast_success("強制削除を開始しました", cx);
            }
            Ok(false) => self.toast_warning("削除は既に実行中です", cx),
            Err(error) => {
                tracing::warn!(%error, request = id, "cannot force deletion");
                self.toast_error(format!("強制削除できません: {error}"), cx);
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{deletion_blocked_label, deletion_reason_label, deletion_status_label};
    use vrc_photo_core::model::{DeletionReason, DeletionRequest, DeletionStatus};

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
