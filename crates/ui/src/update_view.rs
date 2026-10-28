use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use chrono::{DateTime, Local, Utc};
use gpui_kit::component::WindowExt;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use semver::Version;
use vrc_photo_update::{
    ApplyJob, ApplyKind, AvailableUpdate, FeedConfig, InstallKind, IoOperation, LastResult,
    LastStatus, NativeString, UpdateError, UpdatePaths, UreqClient, check, download, start_apply,
};

use crate::app::App;
use crate::app::daemon::{open_path, spawn_daemon_process};

#[cfg(test)]
mod tests {
    use super::*;
    use vrc_photo_update::{InstallKind, LastResult, LastStatus, PackageFamily};

    fn appimage() -> InstallKind {
        InstallKind::AppImage {
            path: "/tmp/VRC.AppImage".into(),
        }
    }

    #[test]
    fn formats_byte_sizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MB");
    }

    #[test]
    fn labels_the_never_checked_state() {
        assert_eq!(last_checked_label(None), "未確認");
    }

    #[test]
    fn describes_a_successful_result_only_for_the_new_version() {
        let result = LastResult {
            status: LastStatus::Success,
            from: "0.3.0".to_string(),
            to: "0.4.0".to_string(),
            reason: None,
        };
        let notice = result_notice(&result, "0.4.0").unwrap();
        assert_eq!(notice.message, "v0.4.0 に更新しました");
        assert_eq!(notice.status, LastStatus::Success);
        assert!(result_notice(&result, "0.3.0").is_none());
    }

    #[test]
    fn describes_a_rollback_with_its_reason() {
        let result = LastResult {
            status: LastStatus::RolledBack,
            from: "0.3.0".to_string(),
            to: "0.4.0".to_string(),
            reason: Some("起動しませんでした".to_string()),
        };
        let notice = result_notice(&result, "0.3.0").unwrap();
        assert!(notice.message.contains("v0.3.0 に戻しました"));
        assert!(notice.message.contains("起動しませんでした"));
        assert_eq!(notice.status, LastStatus::RolledBack);
        assert!(result_notice(&result, "0.4.0").is_none());
    }

    #[test]
    fn labels_only_the_time_for_a_checked_state() {
        let label = last_checked_label(Some(1_700_000_000));
        assert!(!label.contains("最終確認"));
        assert!(label.contains(':'));
    }

    #[test]
    fn colors_rollback_and_failure_differently_from_success() {
        let success = UpdateNotice {
            message: "ok".into(),
            status: LastStatus::Success,
        };
        let rolled_back = UpdateNotice {
            message: "back".into(),
            status: LastStatus::RolledBack,
        };
        let failed = UpdateNotice {
            message: "failed".into(),
            status: LastStatus::Failed,
        };
        assert_eq!(success.color(), GREEN);
        assert_eq!(rolled_back.color(), AMBER);
        assert_eq!(failed.color(), RED);
    }

    #[test]
    fn auto_check_runs_while_an_update_is_available() {
        let update = AvailableUpdate {
            version: Version::new(0, 4, 0),
            notes_url: None,
            asset: None,
            public_keys: Vec::new(),
        };
        assert!(phase_allows_auto_check(&UpdatePhase::Available(update)));
        assert!(!phase_allows_auto_check(&UpdatePhase::Applying));
    }

    #[test]
    fn automatic_check_failure_restores_an_available_update() {
        let update = AvailableUpdate {
            version: Version::new(0, 4, 0),
            notes_url: None,
            asset: None,
            public_keys: Vec::new(),
        };
        let failure: std::result::Result<Option<AvailableUpdate>, String> =
            Err("offline".to_string());

        let restored = resolve_checked_phase(false, &failure, Some(update.clone()));
        assert!(matches!(
            restored,
            UpdatePhase::Available(ref available) if available.version == update.version
        ));
        assert!(matches!(
            resolve_checked_phase(true, &failure, Some(update.clone())),
            UpdatePhase::Error(_)
        ));
        assert!(matches!(
            resolve_checked_phase(false, &failure, None),
            UpdatePhase::Idle
        ));

        let up_to_date: std::result::Result<Option<AvailableUpdate>, String> = Ok(None);
        assert!(matches!(
            resolve_checked_phase(false, &up_to_date, Some(update)),
            UpdatePhase::UpToDate
        ));
    }

    #[test]
    fn only_self_update_capable_installs_with_assets_can_apply() {
        assert!(!can_apply(
            &InstallKind::SystemPackage {
                family: PackageFamily::Deb
            },
            true,
            false,
            false
        ));
        assert!(can_apply(&appimage(), true, false, false));
        assert!(!can_apply(&appimage(), true, true, false));
        assert!(can_apply(&appimage(), true, true, true));
        assert!(!can_apply(&appimage(), false, false, false));
    }

    #[test]
    fn detects_a_writable_appimage_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("VRC.AppImage");
        std::fs::write(&target, b"appimage").unwrap();
        let install = InstallKind::AppImage { path: target };
        assert!(appimage_writable(&install));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
            assert!(!appimage_writable(&install));
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn explains_update_io_failures_by_operation() {
        use vrc_photo_update::IoOperation;

        let cases = [
            (UpdateError::HelperMissing, "更新ヘルパーが見つかりません"),
            (
                UpdateError::io_at(
                    IoOperation::Job,
                    "/tmp/job.json",
                    std::io::ErrorKind::PermissionDenied.into(),
                ),
                "更新ジョブを書き込めません",
            ),
            (
                UpdateError::io_at(
                    IoOperation::Apply,
                    "/tmp/app.AppImage",
                    std::io::ErrorKind::PermissionDenied.into(),
                ),
                "AppImageを置き換えられません",
            ),
            (
                UpdateError::io_at(
                    IoOperation::Spawn,
                    "/tmp/helper",
                    std::io::ErrorKind::NotFound.into(),
                ),
                "更新ヘルパーを起動できません",
            ),
            (
                UpdateError::io_at(
                    IoOperation::Download,
                    "/tmp/artifact",
                    std::io::ErrorKind::NotFound.into(),
                ),
                "更新ファイルを保存できません",
            ),
        ];
        let messages: std::collections::HashSet<_> = cases
            .iter()
            .map(|(error, expected)| {
                let message = update_error_message(error);
                assert!(message.contains(*expected), "{message}");
                message
            })
            .collect();
        assert_eq!(messages.len(), cases.len());
    }

    #[test]
    fn keeps_existing_update_error_messages() {
        assert!(update_error_message(&UpdateError::Signature).contains("署名"));
        assert!(update_error_message(&UpdateError::Integrity("x".into())).contains("検証"));
        assert!(update_error_message(&UpdateError::Network("x".into())).contains("ネットワーク"));
    }

    #[test]
    fn labels_install_kinds_and_package_commands() {
        assert_eq!(install_label(&appimage()), "AppImage");
        assert_eq!(
            install_label(&InstallKind::SystemPackage {
                family: PackageFamily::Deb
            }),
            "deb"
        );
        assert_eq!(
            package_command(&InstallKind::SystemPackage {
                family: PackageFamily::Pacman
            }),
            Some("sudo pacman -Syu vrc-photo-manager")
        );
        assert_eq!(package_command(&appimage()), None);
    }
}

use super::ipc_bridge;
use crate::ui::components::glass_panel;
use crate::ui::theme::{
    ACCENT_SOFT, AMBER, BLUE, GREEN, MUTED, MUTED_2, PANEL_2, PANEL_DEEP, RED, TEXT,
};
use gpui_kit::base::{Disableable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::prelude::*;
use gpui_kit::{Context, Div, Window, div, px, relative, rgb};

enum UpdateMessage {
    Checked {
        manual: bool,
        result: std::result::Result<Option<AvailableUpdate>, String>,
    },
    DownloadProgress {
        received: u64,
        total: Option<u64>,
    },
    Downloaded {
        staged: PathBuf,
    },
    DownloadFailed(String),
}

#[derive(Debug, Clone)]
pub enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    Available(AvailableUpdate),
    Downloading {
        update: AvailableUpdate,
        received: u64,
        total: Option<u64>,
    },
    Ready {
        update: AvailableUpdate,
        staged: PathBuf,
    },
    Applying,
    Error(String),
}

impl UpdatePhase {
    fn badge(&self) -> (&'static str, u32, u32) {
        match self {
            Self::Idle => ("待機", MUTED, PANEL_2),
            Self::Checking => ("確認中", BLUE, ACCENT_SOFT),
            Self::UpToDate => ("最新", GREEN, 0x112a22),
            Self::Available(_) => ("更新あり", AMBER, 0x2a2412),
            Self::Downloading { .. } => ("ダウンロード中", BLUE, ACCENT_SOFT),
            Self::Ready { .. } => ("準備完了", GREEN, 0x112a22),
            Self::Applying => ("適用中", AMBER, 0x2a2412),
            Self::Error(_) => ("エラー", RED, 0x2a1414),
        }
    }
}

fn phase_allows_auto_check(phase: &UpdatePhase) -> bool {
    matches!(
        phase,
        UpdatePhase::Idle
            | UpdatePhase::UpToDate
            | UpdatePhase::Available(_)
            | UpdatePhase::Error(_)
    )
}

fn resolve_checked_phase(
    manual: bool,
    result: &std::result::Result<Option<AvailableUpdate>, String>,
    resume: Option<AvailableUpdate>,
) -> UpdatePhase {
    match result {
        Ok(Some(update)) => UpdatePhase::Available(update.clone()),
        Ok(None) => UpdatePhase::UpToDate,
        Err(message) if manual => UpdatePhase::Error(message.clone()),
        Err(_) => resume
            .map(UpdatePhase::Available)
            .unwrap_or(UpdatePhase::Idle),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UpdateNotice {
    pub(crate) message: String,
    status: LastStatus,
}

impl UpdateNotice {
    pub(crate) fn color(&self) -> u32 {
        match self.status {
            LastStatus::Success => GREEN,
            LastStatus::RolledBack => AMBER,
            LastStatus::Failed => RED,
        }
    }
}

/// UI state for the signed application updater.
pub struct UpdateState {
    pub phase: UpdatePhase,
    pub install: InstallKind,
    notice: Option<UpdateNotice>,
    resume_available: Option<AvailableUpdate>,
    tx: Sender<UpdateMessage>,
    rx: Option<Receiver<UpdateMessage>>,
    health_token: Option<String>,
    database_ok: bool,
    demo: bool,
    data_dir: Option<PathBuf>,
    db_path: Option<PathBuf>,
    in_flight: bool,
    cancel: Arc<AtomicBool>,
}

impl UpdateState {
    pub fn new(
        health_token: Option<String>,
        database_ok: bool,
        demo: bool,
        db_path: Option<PathBuf>,
    ) -> Self {
        let install = if demo {
            InstallKind::Development
        } else {
            vrc_photo_update::install_kind()
        };
        let data_dir = vrc_photo_core::data_dir();
        let notice = if demo || !database_ok {
            None
        } else {
            data_dir.as_deref().and_then(|dir| {
                let result = LastResult::take(&UpdatePaths::new(dir))?;
                result_notice(&result, vrc_photo_update::CURRENT_VERSION)
            })
        };
        let (tx, rx) = async_channel::unbounded();
        Self {
            phase: UpdatePhase::Idle,
            install,
            notice,
            resume_available: None,
            tx,
            rx: Some(rx),
            health_token,
            database_ok,
            demo,
            data_dir,
            db_path,
            in_flight: false,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn busy(&self) -> bool {
        self.in_flight
            || matches!(
                self.phase,
                UpdatePhase::Checking | UpdatePhase::Downloading { .. } | UpdatePhase::Applying
            )
    }

    fn take_rx(&mut self) -> Option<Receiver<UpdateMessage>> {
        self.rx.take()
    }

    fn start_channel(&mut self, cx: &mut Context<App>) {
        let Some(rx) = self.take_rx() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            while let Ok(message) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.handle_update_message(message, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn nav_badge(&self) -> Option<String> {
        matches!(
            self.phase,
            UpdatePhase::Available(_) | UpdatePhase::Downloading { .. } | UpdatePhase::Ready { .. }
        )
        .then(|| "更新".to_string())
    }

    pub(crate) fn notice(&self) -> Option<&UpdateNotice> {
        self.notice.as_ref()
    }

    fn write_health_marker(&self) -> std::result::Result<(), String> {
        if !self.database_ok || self.demo {
            return Err("データベースを開けなかったため、更新を確定できません".to_string());
        }
        let token = self
            .health_token
            .as_deref()
            .ok_or_else(|| "更新用トークンがありません".to_string())?;
        let data_dir = self
            .data_dir
            .as_deref()
            .ok_or_else(|| "データディレクトリを特定できません".to_string())?;
        let paths = UpdatePaths::new(data_dir);
        paths.ensure().map_err(|error| error.to_string())?;
        vrc_photo_update::helper::write_health_marker(
            &paths,
            token,
            vrc_photo_update::CURRENT_VERSION,
            self.db_path.as_deref(),
        )
        .map_err(|error| error.to_string())
    }
}

fn update_error_message(error: &UpdateError) -> String {
    match error {
        UpdateError::Network(_) => {
            "更新を確認できませんでした（ネットワークまたはサーバーの問題）".to_string()
        }
        UpdateError::Signature => "署名の検証に失敗したため、更新を中止しました".to_string(),
        UpdateError::Integrity(_) => {
            "更新ファイルの検証に失敗したため、更新を中止しました".to_string()
        }
        UpdateError::InvalidManifest(_) => "更新情報を検証できませんでした".to_string(),
        UpdateError::UnsupportedPlatform => {
            "このプラットフォーム向けの更新ファイルがありません".to_string()
        }
        UpdateError::Cancelled => "更新をキャンセルしました".to_string(),
        UpdateError::NotFound => "更新情報が見つかりません".to_string(),
        UpdateError::HelperMissing => {
            "更新ヘルパーが見つかりません。アプリを再インストールしてください".to_string()
        }
        UpdateError::Io { operation, .. } => match operation {
            IoOperation::Helper => "更新ヘルパーを準備できませんでした".to_string(),
            IoOperation::Job => "更新ジョブを書き込めませんでした".to_string(),
            IoOperation::Apply => "AppImageを置き換えられませんでした".to_string(),
            IoOperation::Spawn => "更新ヘルパーを起動できませんでした".to_string(),
            IoOperation::Download | IoOperation::Other => {
                "更新ファイルを保存できませんでした".to_string()
            }
        },
        UpdateError::InvalidJob(_) => "更新の適用情報が不正です".to_string(),
        UpdateError::Other(message) => format!("更新処理に失敗しました: {message}"),
    }
}

impl App {
    pub(crate) fn start_update_timers(&mut self, cx: &mut Context<Self>) {
        if self.demo {
            return;
        }
        self.update.start_channel(cx);
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(15))
                .await;
            if this
                .update(cx, |this, cx| this.maybe_auto_update_check(cx))
                .is_err()
            {
                return;
            }
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(60 * 60))
                    .await;
                if this
                    .update(cx, |this, cx| this.maybe_auto_update_check(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn maybe_auto_update_check(&mut self, cx: &mut Context<Self>) {
        if !self.settings.auto_update_check
            || !self.update.install.automatic_check()
            || self.update.busy()
            || !phase_allows_auto_check(&self.update.phase)
        {
            return;
        }
        let now = Utc::now().timestamp();
        if self
            .settings
            .update_last_checked_at
            .is_some_and(|last| now - last < 86_400)
        {
            return;
        }
        self.start_update_check(false, cx);
    }

    pub(crate) fn start_update_check(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.update.busy() {
            if manual {
                self.toast_info("更新を確認しています", cx);
                cx.notify();
            }
            return;
        }
        self.update.resume_available = match &self.update.phase {
            UpdatePhase::Available(update) => Some(update.clone()),
            _ => None,
        };
        self.update.in_flight = true;
        self.update.phase = UpdatePhase::Checking;
        cx.notify();

        let tx = self.update.tx.clone();
        let install = self.update.install.clone();
        let spawned = std::thread::Builder::new()
            .name("update-check".to_string())
            .spawn(move || {
                let client = UreqClient::new(cfg!(debug_assertions));
                let feed = FeedConfig::from_env();
                let Ok(current) = Version::parse(vrc_photo_update::CURRENT_VERSION) else {
                    let _ = tx.send_blocking(UpdateMessage::Checked {
                        manual,
                        result: Err("現在のバージョンを解析できません".to_string()),
                    });
                    return;
                };
                let result = check(&client, &feed, &current, &install)
                    .map_err(|error| update_error_message(&error));
                let _ = tx.send_blocking(UpdateMessage::Checked { manual, result });
            });
        if let Err(error) = spawned {
            self.update.in_flight = false;
            let resume = self.update.resume_available.take();
            self.update.phase = match (manual, resume) {
                (false, Some(update)) => UpdatePhase::Available(update),
                _ => UpdatePhase::Error(format!("更新確認スレッドを開始できません: {error}")),
            };
            self.toast_error("更新を確認できませんでした", cx);
            cx.notify();
        }
    }

    pub(crate) fn start_update_download(&mut self, cx: &mut Context<Self>) {
        if !appimage_writable(&self.update.install) {
            self.update.phase = UpdatePhase::Error(
                "AppImageの場所に書き込めないため更新できません。書き込み可能な場所へ移動してください"
                    .to_string(),
            );
            self.toast_error(
                "AppImageの場所に書き込めないため更新できません。書き込み可能な場所へ移動してください",
                cx,
            );
            cx.notify();
            return;
        }
        let UpdatePhase::Available(update) = &self.update.phase else {
            return;
        };
        let update = update.clone();
        let Some(asset) = update.asset.clone() else {
            self.update.phase = UpdatePhase::Error(
                "このプラットフォーム向けの更新ファイルがありません".to_string(),
            );
            cx.notify();
            return;
        };
        let Some(data_dir) = self.update.data_dir.clone() else {
            self.update.phase =
                UpdatePhase::Error("データディレクトリを特定できません".to_string());
            cx.notify();
            return;
        };
        let file_name = match asset.file_name() {
            Ok(name) => name,
            Err(error) => {
                self.update.phase = UpdatePhase::Error(update_error_message(&error));
                cx.notify();
                return;
            }
        };
        let dest = UpdatePaths::new(&data_dir)
            .downloads_dir()
            .join(update.version.to_string())
            .join(file_name);
        self.update.cancel.store(false, Ordering::Relaxed);
        self.update.phase = UpdatePhase::Downloading {
            update: update.clone(),
            received: 0,
            total: Some(asset.size),
        };
        cx.notify();

        let tx = self.update.tx.clone();
        let cancel = self.update.cancel.clone();
        let update_for_download = update;
        let spawned = std::thread::Builder::new()
            .name("update-download".to_string())
            .spawn(move || {
                let client = UreqClient::new(cfg!(debug_assertions));
                let mut last_reported = 0u64;
                let result = download(
                    &client,
                    &update_for_download,
                    &dest,
                    &mut |received, total| {
                        if received.saturating_sub(last_reported) >= 256 * 1024
                            || Some(received) == total
                        {
                            last_reported = received;
                            let _ = tx
                                .send_blocking(UpdateMessage::DownloadProgress { received, total });
                        }
                    },
                    &cancel,
                );
                match result {
                    Ok(staged) => {
                        let _ = tx.send_blocking(UpdateMessage::Downloaded { staged });
                    }
                    Err(error) => {
                        let _ = tx.send_blocking(UpdateMessage::DownloadFailed(
                            update_error_message(&error),
                        ));
                    }
                }
            });
        if let Err(error) = spawned {
            self.update.phase =
                UpdatePhase::Error(format!("ダウンロードを開始できません: {error}"));
            self.toast_error("更新ファイルをダウンロードできませんでした", cx);
            cx.notify();
        }
    }

    pub(crate) fn apply_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.update.phase, UpdatePhase::Ready { .. }) {
            return;
        }
        let entity = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            alert
                .title("更新を適用")
                .bg(rgb(PANEL_DEEP))
                .description("ダウンロード済みの更新を適用して再起動します。")
                .confirm()
                .ok_text("更新して再起動")
                .ok_variant(ButtonVariant::Primary)
                .cancel_text("キャンセル")
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.apply_update_confirmed(cx));
                    true
                })
        });
    }

    fn apply_update_confirmed(&mut self, cx: &mut Context<Self>) {
        let UpdatePhase::Ready { update, staged } = &self.update.phase else {
            return;
        };
        let update = update.clone();
        let staged = staged.clone();
        let Some(asset) = update.asset.clone() else {
            self.update.phase = UpdatePhase::Error(
                "このプラットフォーム向けの更新ファイルがありません".to_string(),
            );
            cx.notify();
            return;
        };
        if !appimage_writable(&self.update.install) {
            self.toast_error(
                "AppImageの場所に書き込めないため更新できません。書き込み可能な場所へ移動してください",
                cx,
            );
            cx.notify();
            return;
        }
        if !can_apply(
            &self.update.install,
            true,
            self.daemon_active,
            self.ipc_connected,
        ) {
            if self.daemon_active && !self.ipc_connected {
                self.toast_error(
                    "デーモンに接続できないため更新できません。デーモンを停止してから再試行してください",
                    cx,
                );
            } else {
                self.toast_warning("このインストール形式では自動更新を適用できません", cx);
            }
            cx.notify();
            return;
        }
        if let Some(data_dir) = self.update.data_dir.as_deref()
            && vrc_photo_update::helper::apply_in_progress(&UpdatePaths::new(data_dir))
        {
            self.toast_warning("別の更新処理が進行中です", cx);
            cx.notify();
            return;
        }
        let Some(data_dir) = self.update.data_dir.clone() else {
            self.toast_error("データディレクトリを特定できません", cx);
            cx.notify();
            return;
        };
        let Ok(current_exe) = std::env::current_exe() else {
            self.toast_error("現在の実行ファイルを特定できません", cx);
            cx.notify();
            return;
        };
        let working_dir = match std::env::current_dir() {
            Ok(dir) => dir,
            Err(error) => {
                let message = format!("現在の作業ディレクトリを特定できません: {error}");
                self.update.phase = UpdatePhase::Error(message.clone());
                self.toast_error(message, cx);
                cx.notify();
                return;
            }
        };
        let restart_args: Vec<NativeString> =
            std::env::args_os().skip(1).map(NativeString::new).collect();
        let (kind, target) = match &self.update.install {
            InstallKind::AppImage { path } => (ApplyKind::AppImage, path.clone()),
            InstallKind::WindowsCurrentUser { .. } => (ApplyKind::Windows, current_exe.clone()),
            _ => {
                self.toast_warning("このインストール形式では自動更新を適用できません", cx);
                cx.notify();
                return;
            }
        };
        let job = ApplyJob {
            schema: vrc_photo_update::APPLY_SCHEMA,
            kind,
            from_version: vrc_photo_update::CURRENT_VERSION.to_string(),
            to_version: update.version.to_string(),
            target,
            staged,
            signature: asset.signature,
            sha256: asset.sha256,
            ui_pid: std::process::id(),
            daemon_pid: self.daemon_pid,
            restart_args,
            working_dir,
            database: self
                .update
                .db_path
                .as_ref()
                .map(|path| NativeString::new(path.as_os_str())),
            health_token: ApplyJob::new_health_token(),
            data_dir,
            created_at: Utc::now().to_rfc3339(),
        };

        let endpoint = self.ipc_endpoint.clone();
        let shutdown = self.ipc_connected;
        let apply_data_dir = job.data_dir.clone();
        cx.spawn(async move |this, cx| {
            if shutdown
                && let Some(endpoint) = endpoint
                && let Err(error) = cx
                    .background_executor()
                    .spawn(async move {
                        ipc_bridge::request(endpoint, vrc_photo_ipc::Request::Shutdown)
                    })
                    .await
            {
                tracing::warn!(%error, "cannot stop the daemon for the update");
            }
            this.update(cx, |this, cx| {
                match start_apply(&job, &current_exe, &apply_data_dir) {
                    Ok(()) => {
                        this.update.phase = UpdatePhase::Applying;
                        cx.notify();
                        cx.quit();
                    }
                    Err(error) => {
                        let message = update_error_message(&error);
                        this.update.phase = UpdatePhase::Error(message.clone());
                        this.toast_error(message, cx);
                        if this.daemon_active {
                            let _ = spawn_daemon_process();
                        }
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn complete_update_startup(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.update.health_token.is_none() {
            return;
        }
        if let Err(error) = self.update.write_health_marker() {
            self.toast_error(format!("更新の起動確認を記録できません: {error}"), cx);
            tracing::error!(%error, "cannot write update health marker");
            cx.notify();
            return;
        }
        if !self.demo
            && let Err(error) = spawn_daemon_process()
        {
            self.toast_error(format!("デーモンを起動できません: {error}"), cx);
        }
        cx.notify();
    }

    fn handle_update_message(&mut self, message: UpdateMessage, cx: &mut Context<Self>) {
        match message {
            UpdateMessage::Checked { manual, result } => {
                self.update.in_flight = false;
                let resume = self.update.resume_available.take();
                self.settings.update_last_checked_at = Some(Utc::now().timestamp());
                if let Err(error) = self.settings.save(&self.db) {
                    tracing::warn!(%error, "cannot save update check time");
                }
                match &result {
                    Ok(Some(update)) => {
                        if !manual {
                            self.toast_info(
                                format!(
                                    "アップデート v{} があります（設定 → アップデート）",
                                    update.version
                                ),
                                cx,
                            );
                        } else {
                            self.toast_info(format!("v{} が利用可能です", update.version), cx);
                        }
                    }
                    Ok(None) => {
                        if manual {
                            self.toast_success("最新のバージョンです", cx);
                        }
                    }
                    Err(message) => {
                        if manual {
                            self.toast_error(message.clone(), cx);
                        } else {
                            tracing::warn!(%message, "automatic update check failed");
                        }
                    }
                }
                self.update.phase = resolve_checked_phase(manual, &result, resume);
                cx.notify();
            }
            UpdateMessage::DownloadProgress { received, total } => {
                if let UpdatePhase::Downloading {
                    received: current,
                    total: current_total,
                    ..
                } = &mut self.update.phase
                {
                    *current = received;
                    *current_total = total;
                    cx.notify();
                }
            }
            UpdateMessage::Downloaded { staged } => {
                if let UpdatePhase::Downloading { update, .. } = &self.update.phase {
                    self.update.phase = UpdatePhase::Ready {
                        update: update.clone(),
                        staged,
                    };
                    self.toast_success(
                        "ダウンロードと署名検証が完了しました。再起動して更新できます",
                        cx,
                    );
                    cx.notify();
                }
            }
            UpdateMessage::DownloadFailed(message) => {
                self.update.phase = UpdatePhase::Error(message.clone());
                self.toast_error(message, cx);
                cx.notify();
            }
        }
    }

    pub(crate) fn update_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let phase = self.update.phase.clone();
        let install = self.update.install.clone();
        let last_checked = last_checked_label(self.settings.update_last_checked_at);
        let demo = self.demo;
        let busy = self.update.busy();
        let (status, fg, bg) = phase.badge();

        let mut card = glass_panel()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .p(px(14.))
            .gap(px(12.))
            .rounded(px(17.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .text_color(rgb(TEXT))
                            .child("アップデート"),
                    )
                    .child(self.badge(status, fg, bg)),
            )
            .child(self.meta_row(
                "現在のバージョン",
                format!("v{}", vrc_photo_update::CURRENT_VERSION),
            ))
            .child(self.meta_row("インストール形式", install_label(&install).to_string()))
            .child(self.meta_row("最終確認", last_checked));

        if demo {
            return card.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("デモではアップデートを利用できません"),
            );
        }

        if let Some(notice) = self.update.notice() {
            card = card.child(
                div()
                    .p(px(10.))
                    .rounded(px(8.))
                    .bg(rgb(PANEL_2))
                    .text_size(px(11.))
                    .text_color(rgb(notice.color()))
                    .child(notice.message.clone()),
            );
        }

        card = card.child(self.setting_switch(
            "アップデートを自動確認",
            "起動中に24時間ごとに確認します",
            self.settings.auto_update_check,
            false,
            cx,
            |settings, value| settings.auto_update_check = value,
        ));

        card = card.child(
            h_flex().flex_wrap().gap(px(8.)).child(
                Button::new("update-check")
                    .label("今すぐ確認")
                    .small()
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_update_check(true, cx);
                    })),
            ),
        );

        card = card.child(self.update_phase_view(&phase, cx));

        if let Some(command) = package_command(&install) {
            card = card.child(
                v_flex()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child("このインストール形式はパッケージマネージャで更新します"),
                    )
                    .child(
                        div()
                            .font_family("monospace")
                            .text_size(px(10.))
                            .text_color(rgb(TEXT))
                            .child(command),
                    ),
            );
        } else if matches!(install, InstallKind::SystemPackage { .. }) {
            card = card.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("このディストリビューションのパッケージマネージャで更新してください"),
            );
        } else if matches!(
            install,
            InstallKind::Development
                | InstallKind::WindowsOther
                | InstallKind::WindowsPerMachine { .. }
        ) {
            card = card.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED_2))
                    .child("このインストール形式は自動更新の対象外です"),
            );
        }

        card
    }

    fn update_phase_view(&mut self, phase: &UpdatePhase, cx: &mut Context<Self>) -> Div {
        match phase {
            UpdatePhase::Idle => div(),
            UpdatePhase::Checking => div()
                .text_size(px(11.))
                .text_color(rgb(MUTED))
                .child("更新を確認しています…"),
            UpdatePhase::UpToDate => div()
                .text_size(px(11.))
                .text_color(rgb(GREEN))
                .child("最新のバージョンです"),
            UpdatePhase::Available(update) => self.update_available_view(update, cx),
            UpdatePhase::Downloading {
                received, total, ..
            } => update_downloading_view(*received, *total),
            UpdatePhase::Ready { update, .. } => self.update_ready_view(update, cx),
            UpdatePhase::Applying => div()
                .text_size(px(11.))
                .text_color(rgb(AMBER))
                .child("更新を適用しています。アプリを再起動します…"),
            UpdatePhase::Error(message) => div()
                .text_size(px(11.))
                .text_color(rgb(RED))
                .child(message.clone()),
        }
    }

    fn update_available_view(&mut self, update: &AvailableUpdate, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(TEXT))
                    .child(format!("v{} が利用可能です", update.version)),
            )
            .when_some(update.notes_url.clone(), |this, url| {
                this.child(
                    Button::new("update-notes")
                        .label("リリースノート")
                        .small()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Err(error) = open_path(Path::new(&url)) {
                                this.toast_error(
                                    format!("リリースノートを開けません: {error}"),
                                    cx,
                                );
                                cx.notify();
                            }
                        })),
                )
            })
            .when(
                update.asset.is_some() && appimage_writable(&self.update.install),
                |this| {
                    this.child(
                        Button::new("update-download")
                            .label("ダウンロード")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_update_download(cx);
                            })),
                    )
                },
            )
            .when(
                update.asset.is_some() && !appimage_writable(&self.update.install),
                |this| {
                    this.child(div().text_size(px(10.)).text_color(rgb(AMBER)).child(
                        "AppImageの場所に書き込めません。書き込み可能な場所へ移動してください",
                    ))
                },
            )
    }

    fn update_ready_view(&mut self, update: &AvailableUpdate, cx: &mut Context<Self>) -> Div {
        let apply_ok = can_apply(
            &self.update.install,
            update.asset.is_some(),
            self.daemon_active,
            self.ipc_connected,
        ) && appimage_writable(&self.update.install);
        v_flex()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(GREEN))
                    .child("ダウンロードと署名検証が完了しました"),
            )
            .child(
                Button::new("update-apply")
                    .label("更新して再起動")
                    .small()
                    .primary()
                    .disabled(!apply_ok)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.apply_update(window, cx);
                    })),
            )
            .when(!apply_ok, |this| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(AMBER))
                        .child(if self.daemon_active && !self.ipc_connected {
                            "デーモンに接続できないため更新できません。デーモンを停止してから再試行してください"
                        } else {
                            "このインストール形式では自動更新を適用できません"
                        }),
                )
            })
    }
}

fn update_downloading_view(received: u64, total: Option<u64>) -> Div {
    let ratio = total
        .map(|total| (received as f32 / total as f32).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    v_flex()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(MUTED))
                .child(format!(
                    "ダウンロード中 {:.0}% · {} / {}",
                    ratio * 100.0,
                    format_bytes(received),
                    total.map(format_bytes).unwrap_or_else(|| "?".to_string())
                )),
        )
        .child(
            div()
                .h(px(6.))
                .w_full()
                .rounded_full()
                .bg(rgb(PANEL_2))
                .child(
                    div()
                        .h_full()
                        .w(relative(ratio))
                        .rounded_full()
                        .bg(rgb(BLUE)),
                ),
        )
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < MB {
        format!("{:.1} KB", value / KB)
    } else if value < GB {
        format!("{:.1} MB", value / MB)
    } else {
        format!("{:.1} GB", value / GB)
    }
}

fn last_checked_label(checked_at: Option<i64>) -> String {
    let Some(checked_at) = checked_at else {
        return "未確認".to_string();
    };
    let Some(timestamp) = DateTime::from_timestamp(checked_at, 0) else {
        return "不明".to_string();
    };
    timestamp
        .with_timezone(&Local)
        .format("%Y/%m/%d %H:%M")
        .to_string()
}

fn result_notice(result: &LastResult, current_version: &str) -> Option<UpdateNotice> {
    match result.status {
        LastStatus::Success if result.to == current_version => Some(UpdateNotice {
            message: format!("v{} に更新しました", result.to),
            status: LastStatus::Success,
        }),
        LastStatus::RolledBack if result.from == current_version => {
            let reason = result
                .reason
                .as_deref()
                .map(|reason| format!("（{reason}）"))
                .unwrap_or_default();
            Some(UpdateNotice {
                message: format!("更新に失敗したため v{} に戻しました{reason}", result.from),
                status: LastStatus::RolledBack,
            })
        }
        LastStatus::Failed if result.from == current_version => {
            let reason = result
                .reason
                .as_deref()
                .map(|reason| format!("（{reason}）"))
                .unwrap_or_default();
            Some(UpdateNotice {
                message: format!("更新に失敗しました{reason}"),
                status: LastStatus::Failed,
            })
        }
        _ => None,
    }
}

fn can_apply(
    install: &InstallKind,
    has_asset: bool,
    daemon_active: bool,
    ipc_connected: bool,
) -> bool {
    install.automatic_apply() && has_asset && (!daemon_active || ipc_connected)
}

fn appimage_writable(install: &InstallKind) -> bool {
    let InstallKind::AppImage { path } = install else {
        return true;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    let probe = parent.join(format!(".vrc-photo-write-test-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn install_label(install: &InstallKind) -> &'static str {
    match install {
        InstallKind::AppImage { .. } => "AppImage",
        InstallKind::WindowsCurrentUser { .. } | InstallKind::WindowsPerMachine { .. } => {
            "Windows (NSIS)"
        }
        InstallKind::WindowsOther => "Windows (手動更新)",
        InstallKind::SystemPackage { family } => match family {
            vrc_photo_update::PackageFamily::Deb => "deb",
            vrc_photo_update::PackageFamily::Pacman => "pacman",
            vrc_photo_update::PackageFamily::Unknown => "Linux パッケージ",
        },
        InstallKind::Development => "開発ビルド",
    }
}

fn package_command(install: &InstallKind) -> Option<&'static str> {
    match install {
        InstallKind::SystemPackage { family } => family.update_command(),
        _ => None,
    }
}
