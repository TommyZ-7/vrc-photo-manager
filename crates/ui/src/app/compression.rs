//! App-side compression profile state: profile ordering, duplication, deletion,
//! and manual-compression queueing.
//!
//! Rendering stays in `ui::compression`; these methods only touch App fields and
//! the compression queue.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::*;
use vrc_photo_core::compressor;
use vrc_photo_core::model::{CompressionProfile, MAX_JOB_RETRIES, MAX_PROFILE_NAME_CHARS};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileNameError {
    Empty,
    TooLong,
}

impl ProfileNameError {
    pub(crate) fn message(self) -> String {
        match self {
            Self::Empty => "名前を入力してください".to_string(),
            Self::TooLong => {
                format!("名前は{MAX_PROFILE_NAME_CHARS}文字以内で入力してください")
            }
        }
    }
}

/// Trims a name typed in the editor and rejects values that must not be
/// persisted. Duplicate names are allowed; references use the profile id.
pub(crate) fn normalize_profile_name(raw: &str) -> Result<String, ProfileNameError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ProfileNameError::Empty);
    }
    if trimmed.chars().count() > MAX_PROFILE_NAME_CHARS {
        return Err(ProfileNameError::TooLong);
    }
    Ok(trimmed.to_string())
}

/// Returns the name to persist when the editor text is valid and differs from
/// the stored name. Invalid or unchanged text must leave the database alone.
pub(crate) fn profile_name_update(
    raw: &str,
    current: &str,
) -> Result<Option<String>, ProfileNameError> {
    let name = normalize_profile_name(raw)?;
    Ok((name != current).then_some(name))
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

    /// Deduplicates ids and drops rows that no longer exist with chunked
    /// queries instead of one `photo_by_id` per selected photo.
    fn existing_photo_ids(&self, photo_ids: Vec<i64>) -> Vec<i64> {
        let mut seen = HashSet::with_capacity(photo_ids.len());
        let unique: Vec<i64> = photo_ids
            .into_iter()
            .filter(|id| seen.insert(*id))
            .collect();
        self.db
            .photos_by_ids(&unique)
            .map(|photos| photos.into_iter().map(|photo| photo.id).collect())
            .unwrap_or_default()
    }

    /// Opens the compression modal for the current multi-selection. The IDs
    /// are read at click time so rendering never clones the selection.
    pub(crate) fn open_manual_compression_for_selection(&mut self, cx: &mut Context<Self>) {
        let ids = self.selected_action_ids();
        self.open_manual_compression(ids, cx);
    }

    /// Inspector shortcut: use the multi-selection when one exists, otherwise
    /// fall back to the photo shown in the inspector.
    pub(crate) fn open_manual_compression_for_photo(
        &mut self,
        photo_id: i64,
        cx: &mut Context<Self>,
    ) {
        let ids = if self.selected_ids.is_empty() {
            vec![photo_id]
        } else {
            self.selected_ids.to_vec()
        };
        self.open_manual_compression(ids, cx);
    }

    pub(crate) fn open_manual_compression(&mut self, photo_ids: Vec<i64>, cx: &mut Context<Self>) {
        let ids = self.existing_photo_ids(photo_ids);
        if ids.is_empty() {
            self.toast_error("先に写真を選択してください", cx);
            cx.notify();
            return;
        }
        let Some(profile) = self
            .profiles
            .get(self.profile_index)
            .or_else(|| self.profiles.first())
        else {
            self.toast_error("圧縮プロファイルがありません", cx);
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
            self.toast_error("先に写真を選択してください", cx);
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
            self.toast_error("先に写真を選択してください", cx);
            cx.notify();
            return;
        }
        self.open_manual_compression_with_target(ids, ManualOutputTarget::Profile, None, cx);
    }

    pub(crate) fn compress_selection_to_clipboard(&mut self, cx: &mut Context<Self>) {
        let ids = self.selected_action_ids();
        if ids.is_empty() {
            self.toast_error("先に写真を選択してください", cx);
            cx.notify();
            return;
        }
        if ids.len() != 1 {
            // A clipboard holds one image. Opening the normal modal would hide
            // the output selector and leave the confirm button disabled.
            self.toast_error("クリップボード圧縮は1枚選択時のみ使えます", cx);
            cx.notify();
            return;
        }
        if let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| profile.clipboard && profile.enabled)
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
        let ids = self.existing_photo_ids(photo_ids);
        if ids.is_empty() {
            self.toast_error("先に写真を選択してください", cx);
            cx.notify();
            return;
        }
        let Some(profile) = self
            .profiles
            .get(self.profile_index)
            .or_else(|| self.profiles.first())
        else {
            self.toast_error("圧縮プロファイルがありません", cx);
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
            self.toast_error("選択したプロファイルが見つかりません", cx);
            cx.notify();
            return;
        };
        if !manual_output_available(dialog.photo_ids.len(), dialog.output) {
            self.toast_error("クリップボード出力は1枚選択時のみ使えます", cx);
            cx.notify();
            return;
        }

        if dialog.output == ManualOutputTarget::Clipboard {
            let photo_id = dialog.photo_ids[0];
            let Some(photo) = self.db.photo_by_id(photo_id).ok().flatten() else {
                self.manual_compress = None;
                self.toast_error("写真が見つかりません", cx);
                cx.notify();
                return;
            };
            let source = PathBuf::from(photo.effective_path());
            self.manual_compress = None;
            self.toast_info("クリップボードへコピーしています…", cx);
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
                            this.toast_success("圧縮画像をクリップボードへコピーしました", cx);
                        }
                        Err(error) => {
                            this.toast_error(
                                format!("クリップボードへコピーできません: {error}"),
                                cx,
                            );
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
                self.toast_warning("出力フォルダを選択してください", cx);
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
            self.toast_success(
                format!("{enqueued} 件を圧縮キューに追加しました（{policy_note}）{failure_note}"),
                cx,
            );
            self.reload_aux(cx);
            self.send_wake_jobs(cx);
        } else {
            self.toast_error("圧縮ジョブを追加できませんでした", cx);
        }
        cx.notify();
    }

    pub(crate) fn edit_profile(
        &mut self,
        edit: impl FnOnce(&mut CompressionProfile),
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self
            .profiles
            .get(self.profile_index)
            .map(|profile| profile.id.clone())
        {
            self.edit_profile_by_id(&id, edit, cx);
        } else {
            cx.notify();
        }
    }

    /// Edits a profile by id so a deferred input event cannot land on whichever
    /// profile happens to be selected by the time it is delivered.
    pub(crate) fn edit_profile_by_id(
        &mut self,
        id: &str,
        edit: impl FnOnce(&mut CompressionProfile),
        cx: &mut Context<Self>,
    ) {
        if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) {
            edit(profile);
            let profile = profile.clone();
            if let Err(error) = self.db.save_profile(&profile) {
                tracing::warn!(%error, profile = %profile.id, "cannot save compression profile");
                self.toast_error(format!("プロファイルを保存できません: {error}"), cx);
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
            Err(error) => self.toast_error(format!("並び替えを保存できません: {error}"), cx),
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
                self.toast_success(
                    format!("「{}」を複製しました（自動適用はOFF）", copy.name),
                    cx,
                );
            }
            Err(error) => self.toast_error(format!("複製に失敗: {error}"), cx),
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
            self.toast_error("最後のプロファイルは削除できません", cx);
            cx.notify();
            return;
        }
        match self.db.active_job_count_for_profile(id) {
            Ok(0) => {
                let entity = cx.entity();
                let id = id.to_string();
                let name = profile.name.clone();
                let panel_deep = self.palette.panel_deep;
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let entity = entity.clone();
                    let id = id.clone();
                    alert
                        .title("プロファイルを削除")
                        .bg(rgb(panel_deep))
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
                self.toast_error(
                    format!(
                        "「{}」は未完了のジョブが{}件あるため削除できません",
                        profile.name, count
                    ),
                    cx,
                );
                cx.notify();
            }
            Err(error) => {
                self.toast_error(format!("削除前の確認に失敗: {error}"), cx);
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
                self.toast_success("プロファイルを削除しました", cx);
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
                self.toast_error(message, cx);
            }
        }
        cx.notify();
    }
    pub(crate) fn retry_job(&mut self, id: String, cx: &mut Context<Self>) {
        match self.db.retry_job(&id) {
            Ok(true) => {
                self.toast_success("再試行をキューに戻しました", cx);
                self.reload_job_state(cx);
                self.send_wake_jobs(cx);
            }
            Ok(false) => {
                self.toast_warning(
                    format!("再試行の上限（{MAX_JOB_RETRIES}回）に達しました"),
                    cx,
                );
            }
            Err(error) => {
                tracing::warn!(%error, %id, "cannot retry compression job");
                self.toast_error(format!("再試行できません: {error}"), cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn retry_all_failed_jobs(&mut self, cx: &mut Context<Self>) {
        match self.db.retry_failed_jobs() {
            Ok(count) => {
                self.toast_success(
                    format!("{count} 件を再試行に戻しました（上限到達は除外）"),
                    cx,
                );
                self.reload_job_state(cx);
                self.send_wake_jobs(cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot retry failed compression jobs");
                self.toast_error(format!("再試行できません: {error}"), cx);
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
        let panel_deep = self.palette.panel_deep;
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            let description = description.clone();
            alert
                .title("待機ジョブを圧縮")
                .bg(rgb(panel_deep))
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
            Ok(0) => self.toast_info("未実行のジョブはありません", cx),
            Ok(count) => {
                self.toast_success(format!("{count} 件を圧縮キューに戻しました"), cx);
                self.reload_job_state(cx);
                self.send_wake_jobs(cx);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot release waiting compression jobs");
                self.toast_error(format!("待機ジョブを開始できません: {error}"), cx);
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PROFILE_NAME_CHARS, ManualOutputTarget, ProfileNameError, manual_output_available,
        normalize_profile_name, profile_name_update,
    };

    #[test]
    fn clipboard_output_is_limited_to_one_photo() {
        assert!(manual_output_available(1, ManualOutputTarget::Clipboard));
        assert!(!manual_output_available(2, ManualOutputTarget::Clipboard));
        assert!(manual_output_available(2, ManualOutputTarget::Folder));
        assert!(manual_output_available(2, ManualOutputTarget::Profile));
    }

    #[test]
    fn profile_name_is_trimmed_before_saving() {
        assert_eq!(
            normalize_profile_name("  SNS共有用 \t").unwrap(),
            "SNS共有用"
        );
        assert_eq!(
            normalize_profile_name("アーカイブ用").unwrap(),
            "アーカイブ用"
        );
    }

    #[test]
    fn profile_name_rejects_empty_and_whitespace_only_values() {
        assert_eq!(normalize_profile_name(""), Err(ProfileNameError::Empty));
        assert_eq!(
            normalize_profile_name("   \t\n"),
            Err(ProfileNameError::Empty)
        );
    }

    #[test]
    fn profile_name_accepts_100_characters_and_rejects_101() {
        let at_limit = "あ".repeat(100);
        assert_eq!(normalize_profile_name(&at_limit).unwrap(), at_limit);

        let over_limit = "あ".repeat(101);
        assert_eq!(
            normalize_profile_name(&over_limit),
            Err(ProfileNameError::TooLong)
        );
    }

    #[test]
    fn profile_name_updates_only_changed_valid_values() {
        assert_eq!(
            profile_name_update("  SNS共有用  ", "アーカイブ用"),
            Ok(Some("SNS共有用".to_string()))
        );
        assert_eq!(
            profile_name_update(" アーカイブ用 ", "アーカイブ用"),
            Ok(None)
        );
        assert_eq!(
            profile_name_update("   ", "アーカイブ用"),
            Err(ProfileNameError::Empty)
        );
        assert_eq!(
            profile_name_update(&"あ".repeat(MAX_PROFILE_NAME_CHARS + 1), "アーカイブ用"),
            Err(ProfileNameError::TooLong)
        );
    }

    #[test]
    fn profile_name_errors_explain_how_to_fix_the_value() {
        assert!(ProfileNameError::Empty.message().contains("入力"));
        assert!(
            ProfileNameError::TooLong
                .message()
                .contains(&MAX_PROFILE_NAME_CHARS.to_string())
        );
    }
}
