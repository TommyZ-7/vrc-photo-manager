//! App-side compression profile state: profile ordering, duplication, deletion,
//! and manual-compression queueing.
//!
//! Rendering stays in `ui::compression`; these methods only touch App fields and
//! the compression queue.

use std::path::PathBuf;

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::*;
use vrc_photo_core::compressor;
use vrc_photo_core::model::{CompressionProfile, MAX_JOB_RETRIES};

use crate::app::App;
use crate::ui::components::release_confirmation;
use crate::ui::theme::{compression_policy, moved_profile_order, step_index};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManualOutputTarget {
    Folder,
    Profile,
    Clipboard,
}

pub(crate) fn manual_output_available(photo_count: usize, target: ManualOutputTarget) -> bool {
    target != ManualOutputTarget::Clipboard || photo_count == 1
}

#[derive(Debug, Clone)]
pub(crate) struct ManualCompressionDialog {
    pub(crate) photo_ids: Vec<i64>,
    pub(crate) profile_id: String,
    pub(crate) output: ManualOutputTarget,
    pub(crate) folder: Option<String>,
    pub(crate) keep_original: bool,
    /// Header actions choose the output before opening the modal, so the
    /// modal must not show the output-destination menu again.
    pub(crate) hide_output: bool,
}

impl App {
    pub(crate) fn set_clipboard_profile(
        &mut self,
        profile_id: &str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let mut updates = Vec::new();
        for profile in &mut self.profiles {
            let should_enable = enabled && profile.id == profile_id;
            if profile.clipboard != should_enable {
                profile.clipboard = should_enable;
                updates.push(profile.clone());
            }
        }
        for profile in updates {
            if let Err(error) = self.db.save_profile(&profile) {
                tracing::warn!(%error, profile_id = %profile.id, "cannot save clipboard flag");
            }
        }
        cx.notify();
    }

    pub(crate) fn open_manual_compression(&mut self, photo_ids: Vec<i64>, cx: &mut Context<Self>) {
        let mut ids = Vec::new();
        for id in photo_ids {
            if ids.contains(&id) {
                continue;
            }
            if self.db.photo_by_id(id).ok().flatten().is_some() {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            self.message = Some("先に写真を選択してください".to_string());
            cx.notify();
            return;
        }
        let Some(profile) = self
            .profiles
            .get(self.profile_index)
            .or_else(|| self.profiles.first())
        else {
            self.message = Some("圧縮プロファイルがありません".to_string());
            cx.notify();
            return;
        };
        self.manual_compress = Some(ManualCompressionDialog {
            photo_ids: ids,
            profile_id: profile.id.clone(),
            output: ManualOutputTarget::Profile,
            folder: None,
            keep_original: true,
            hide_output: false,
        });
        cx.notify();
    }

    pub(crate) fn compress_selection_to_folder(&mut self, cx: &mut Context<Self>) {
        let ids = self.selected_action_ids();
        if ids.is_empty() {
            self.message = Some("先に写真を選択してください".to_string());
            cx.notify();
            return;
        }
        let mut picker = rfd::FileDialog::new();
        if let Some(dir) = self.settings.import_dir().to_str() {
            picker = picker.set_directory(dir);
        }
        let Some(folder) = picker.pick_folder() else {
            return;
        };
        self.open_manual_compression_with_target(
            ids,
            ManualOutputTarget::Folder,
            Some(folder.to_string_lossy().to_string()),
            cx,
        );
    }

    pub(crate) fn compress_selection_to_profile_folder(&mut self, cx: &mut Context<Self>) {
        let ids = self.selected_action_ids();
        if ids.is_empty() {
            self.message = Some("先に写真を選択してください".to_string());
            cx.notify();
            return;
        }
        self.open_manual_compression_with_target(ids, ManualOutputTarget::Profile, None, cx);
    }

    pub(crate) fn compress_selection_to_clipboard(&mut self, cx: &mut Context<Self>) {
        let ids = self.selected_action_ids();
        if ids.is_empty() {
            self.message = Some("先に写真を選択してください".to_string());
            cx.notify();
            return;
        }
        if ids.len() == 1
            && let Some(profile) = self
                .profiles
                .iter()
                .find(|profile| profile.clipboard)
                .cloned()
        {
            self.manual_compress = Some(ManualCompressionDialog {
                photo_ids: ids,
                profile_id: profile.id,
                output: ManualOutputTarget::Clipboard,
                folder: None,
                keep_original: true,
                hide_output: true,
            });
            self.confirm_manual_compression(cx);
            return;
        }
        self.open_manual_compression_with_target(ids, ManualOutputTarget::Clipboard, None, cx);
    }

    fn open_manual_compression_with_target(
        &mut self,
        photo_ids: Vec<i64>,
        output: ManualOutputTarget,
        folder: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut ids = Vec::new();
        for id in photo_ids {
            if !ids.contains(&id) && self.db.photo_by_id(id).ok().flatten().is_some() {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            self.message = Some("先に写真を選択してください".to_string());
            cx.notify();
            return;
        }
        let Some(profile) = self
            .profiles
            .get(self.profile_index)
            .or_else(|| self.profiles.first())
        else {
            self.message = Some("圧縮プロファイルがありません".to_string());
            cx.notify();
            return;
        };
        self.manual_compress = Some(ManualCompressionDialog {
            photo_ids: ids,
            profile_id: profile.id.clone(),
            output,
            folder,
            keep_original: true,
            hide_output: true,
        });
        cx.notify();
    }

    pub(crate) fn close_manual_compression(&mut self, cx: &mut Context<Self>) {
        self.manual_compress = None;
        cx.notify();
    }

    pub(crate) fn choose_manual_folder(&mut self, cx: &mut Context<Self>) {
        let current = self
            .manual_compress
            .as_ref()
            .and_then(|dialog| dialog.folder.clone());
        let mut picker = rfd::FileDialog::new();
        if let Some(dir) = current {
            picker = picker.set_directory(dir);
        }
        if let Some(dir) = picker.pick_folder()
            && let Some(dialog) = &mut self.manual_compress
        {
            dialog.folder = Some(dir.to_string_lossy().to_string());
            dialog.output = ManualOutputTarget::Folder;
        }
        cx.notify();
    }

    pub(crate) fn confirm_manual_compression(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.manual_compress.clone() else {
            return;
        };
        let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| profile.id == dialog.profile_id)
            .cloned()
        else {
            self.message = Some("選択したプロファイルが見つかりません".to_string());
            cx.notify();
            return;
        };
        if !manual_output_available(dialog.photo_ids.len(), dialog.output) {
            self.message = Some("クリップボード出力は1枚選択時のみ使えます".to_string());
            cx.notify();
            return;
        }

        if dialog.output == ManualOutputTarget::Clipboard {
            let photo_id = dialog.photo_ids[0];
            let Some(photo) = self.db.photo_by_id(photo_id).ok().flatten() else {
                self.manual_compress = None;
                self.message = Some("写真が見つかりません".to_string());
                cx.notify();
                return;
            };
            let source = PathBuf::from(photo.effective_path());
            self.manual_compress = None;
            self.message = Some("クリップボードへコピーしています…".to_string());
            cx.notify();
            cx.spawn(async move |this, cx| {
                let bytes = cx
                    .background_executor()
                    .spawn(async move { compressor::render_clipboard_png(&source, &profile) })
                    .await;
                this.update(cx, |this, cx| {
                    match bytes {
                        Ok(bytes) => {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_image(
                                &gpui_kit::Image::from_bytes(gpui_kit::ImageFormat::Png, bytes),
                            ));
                            this.message =
                                Some("圧縮画像をクリップボードへコピーしました".to_string());
                        }
                        Err(error) => {
                            this.message =
                                Some(format!("クリップボードへコピーできません: {error}"));
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }

        let output_dir = if dialog.output == ManualOutputTarget::Folder {
            let Some(folder) = dialog.folder.as_deref() else {
                self.message = Some("出力フォルダを選択してください".to_string());
                cx.notify();
                return;
            };
            Some(folder)
        } else {
            None
        };
        let policy = compression_policy(dialog.keep_original);
        let mut enqueued = 0usize;
        let mut failed = 0usize;
        for photo_id in &dialog.photo_ids {
            match self.db.enqueue_job_for_existing_profile_with_output_dir(
                *photo_id,
                &profile.id,
                policy,
                output_dir,
            ) {
                Ok(true) => enqueued += 1,
                Ok(false) => failed += 1,
                Err(error) => {
                    failed += 1;
                    tracing::warn!(%error, photo_id, "cannot enqueue manual compression");
                }
            }
        }
        self.manual_compress = None;
        if enqueued > 0 {
            let policy_note = if dialog.keep_original {
                "元画像を保持"
            } else {
                "成功後に元画像をゴミ箱へ移動"
            };
            let failure_note = if failed > 0 {
                format!("（{failed} 件は追加できませんでした）")
            } else {
                String::new()
            };
            self.message = Some(format!(
                "{enqueued} 件を圧縮キューに追加しました（{policy_note}）{failure_note}"
            ));
            self.reload_aux(cx);
            self.send_wake_jobs(cx);
        } else {
            self.message = Some("圧縮ジョブを追加できませんでした".to_string());
        }
        cx.notify();
    }

    pub(crate) fn edit_profile(
        &mut self,
        edit: impl FnOnce(&mut CompressionProfile),
        cx: &mut Context<Self>,
    ) {
        if let Some(profile) = self.profiles.get_mut(self.profile_index) {
            edit(profile);
            let profile = profile.clone();
            if let Err(error) = self.db.save_profile(&profile) {
                tracing::warn!(%error, profile = %profile.id, "cannot save compression profile");
                self.message = Some(format!("プロファイルを保存できません: {error}"));
            }
        }
        cx.notify();
    }

    pub(crate) fn move_profile(&mut self, index: usize, delta: isize, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.profiles.iter().map(|p| p.id.clone()).collect();
        let Some(order) = moved_profile_order(&ids, index, delta) else {
            return;
        };
        let target = step_index(index, ids.len(), delta);
        match self.db.reorder_profiles(&order) {
            Ok(()) => {
                self.profiles.swap(index, target);
                if self.profile_index == index {
                    self.profile_index = target;
                } else if self.profile_index == target {
                    self.profile_index = index;
                }
            }
            Err(error) => self.message = Some(format!("並び替えを保存できません: {error}")),
        }
        cx.notify();
    }

    pub(crate) fn duplicate_profile(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(self.profile_index) else {
            return;
        };
        let id = profile.id.clone();
        match self.db.duplicate_profile(&id) {
            Ok(copy) => {
                self.profiles = self.db.list_profiles().unwrap_or_default();
                self.profile_index = self
                    .profiles
                    .iter()
                    .position(|profile| profile.id == copy.id)
                    .unwrap_or(0);
                self.message = Some(format!("「{}」を複製しました（自動適用はOFF）", copy.name));
            }
            Err(error) => self.message = Some(format!("複製に失敗: {error}")),
        }
        cx.notify();
    }

    pub(crate) fn request_delete_profile(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .cloned()
        else {
            return;
        };
        if self.profiles.len() <= 1 {
            self.message = Some("最後のプロファイルは削除できません".to_string());
            cx.notify();
            return;
        }
        match self.db.active_job_count_for_profile(id) {
            Ok(0) => {
                let entity = cx.entity();
                let id = id.to_string();
                let name = profile.name.clone();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let entity = entity.clone();
                    let id = id.clone();
                    alert
                        .title("プロファイルを削除")
                        .description(format!(
                            "「{name}」を削除します。この操作は元に戻せません。"
                        ))
                        .confirm()
                        .ok_text("削除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("キャンセル")
                        .on_ok(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.delete_profile(&id, cx));
                            true
                        })
                });
            }
            Ok(count) => {
                self.message = Some(format!(
                    "「{}」は未完了のジョブが{}件あるため削除できません",
                    profile.name, count
                ));
                cx.notify();
            }
            Err(error) => {
                self.message = Some(format!("削除前の確認に失敗: {error}"));
                cx.notify();
            }
        }
    }

    pub(crate) fn delete_profile(&mut self, id: &str, cx: &mut Context<Self>) {
        let removed_index = self
            .profiles
            .iter()
            .position(|profile| profile.id == id)
            .unwrap_or(self.profile_index);
        let name = self
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| id.to_string());
        match self.db.delete_profile(id) {
            Ok(()) => {
                self.profiles = self.db.list_profiles().unwrap_or_default();
                self.profile_index = removed_index.min(self.profiles.len().saturating_sub(1));
                self.message = Some("プロファイルを削除しました".to_string());
            }
            Err(error) => {
                let message = match self.db.active_job_count_for_profile(id) {
                    Ok(count) if count > 0 => {
                        format!("「{name}」は未完了のジョブが{count}件あるため削除できません")
                    }
                    _ if self.profiles.len() <= 1 => {
                        "最後のプロファイルは削除できません".to_string()
                    }
                    _ => format!("プロファイルを削除できません: {error}"),
                };
                self.message = Some(message);
            }
        }
        cx.notify();
    }
    pub(crate) fn retry_job(&mut self, id: String, cx: &mut Context<Self>) {
        match self.db.retry_job(&id) {
            Ok(true) => {
                self.message = Some("再試行をキューに戻しました".to_string());
                self.reload_job_state();
                self.send_wake_jobs(cx);
            }
            Ok(false) => {
                self.message = Some(format!("再試行の上限（{MAX_JOB_RETRIES}回）に達しました"));
            }
            Err(error) => {
                tracing::warn!(%error, %id, "cannot retry compression job");
                self.message = Some(format!("再試行できません: {error}"));
            }
        }
        cx.notify();
    }

    pub(crate) fn retry_all_failed_jobs(&mut self, cx: &mut Context<Self>) {
        match self.db.retry_failed_jobs() {
            Ok(count) => {
                self.message = Some(format!("{count} 件を再試行に戻しました（上限到達は除外）"));
                self.reload_job_state();
                self.send_wake_jobs(cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot retry failed compression jobs");
                self.message = Some(format!("再試行できません: {error}"));
            }
        }
        cx.notify();
    }

    pub(crate) fn request_release_waiting_jobs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(description) = release_confirmation(
            self.settings.auto_compress_keep_original,
            self.waiting_job_count,
        ) else {
            self.release_waiting_jobs(cx);
            return;
        };
        let entity = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            let description = description.clone();
            alert
                .title("待機ジョブを圧縮")
                .description(description)
                .confirm()
                .ok_text("圧縮する")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("キャンセル")
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.release_waiting_jobs(cx));
                    true
                })
        });
    }

    fn release_waiting_jobs(&mut self, cx: &mut Context<Self>) {
        let policy = compression_policy(self.settings.auto_compress_keep_original);
        match self.db.release_waiting_jobs(policy) {
            Ok(0) => self.message = Some("未実行のジョブはありません".to_string()),
            Ok(count) => {
                self.message = Some(format!("{count} 件を圧縮キューに戻しました"));
                self.reload_job_state();
                self.send_wake_jobs(cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot release waiting compression jobs");
                self.message = Some(format!("待機ジョブを開始できません: {error}"));
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{ManualOutputTarget, manual_output_available};

    #[test]
    fn clipboard_output_is_limited_to_one_photo() {
        assert!(manual_output_available(1, ManualOutputTarget::Clipboard));
        assert!(!manual_output_available(2, ManualOutputTarget::Clipboard));
        assert!(manual_output_available(2, ManualOutputTarget::Folder));
        assert!(manual_output_available(2, ManualOutputTarget::Profile));
    }
}
