use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use chrono::{DateTime, Local, Utc};
use gpui_kit::component::WindowExt;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use semver::Version;
use vrc_photon_i18n::{tr, tr_args};
use vrc_photon_update::{
    ApplyJob, ApplyKind, AvailableUpdate, FeedConfig, InstallKind, IoOperation, LastErrorCode,
    LastResult, LastStatus, NativeString, UpdateError, UpdatePaths, UreqClient, check, download,
    start_apply,
};

use crate::app::App;
use crate::app::daemon::{open_path, restart_daemon_after_update, start_daemon};

#[cfg(test)]
mod tests {
    use super::*;
    use vrc_photon_update::{InstallKind, LastResult, LastStatus, PackageFamily};

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
    fn update_phase_errors_render_in_the_selected_language() {
        let text = UpdateText::key("update.error.network");
        assert!(text.message_in("ja").contains("更新"));
        assert!(text.message_in("en").contains("update"));
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
            code: None,
        };
        let notice = result_notice(&result, "0.4.0").unwrap();
        assert!(notice.message_in("ja").contains("v0.4.0"));
        assert!(notice.message_in("en").contains("v0.4.0"));
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
            code: None,
        };
        let notice = result_notice(&result, "0.3.0").unwrap();
        assert!(notice.message_in("ja").contains("v0.3.0"));
        assert!(notice.message_in("ja").contains("起動しませんでした"));
        assert!(notice.message_in("en").contains("v0.3.0"));
        assert!(notice.message_in("en").contains("起動しませんでした"));
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
            kind: UpdateNoticeKind::Success,
            version: "1.0.0".into(),
            reason: None,
            status: LastStatus::Success,
            code: None,
        };
        let rolled_back = UpdateNotice {
            kind: UpdateNoticeKind::RolledBack,
            version: "1.0.0".into(),
            reason: None,
            status: LastStatus::RolledBack,
            code: None,
        };
        let failed = UpdateNotice {
            kind: UpdateNoticeKind::Failed,
            version: "1.0.0".into(),
            reason: None,
            status: LastStatus::Failed,
            code: None,
        };
        let p = crate::ui::palette::resolve("charcoal", "sky");
        assert_eq!(success.color(&p), p.green);
        assert_eq!(rolled_back.color(&p), p.amber);
        assert_eq!(failed.color(&p), p.red);
    }

    #[test]
    fn auto_check_runs_while_an_update_is_available() {
        let update = AvailableUpdate {
            version: Version::new(0, 4, 0),
            notes_url: None,
            asset: None,
            public_keys: Vec::new(),
            signed_manifest: vrc_photon_update::SignedManifest {
                bytes: Vec::new(),
                signature: String::new(),
            },
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
            signed_manifest: vrc_photon_update::SignedManifest {
                bytes: Vec::new(),
                signature: String::new(),
            },
        };
        let failure: std::result::Result<Option<AvailableUpdate>, UpdateText> =
            Err(UpdateText::key("update.error.network"));

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

        let up_to_date: std::result::Result<Option<AvailableUpdate>, UpdateText> = Ok(None);
        assert!(matches!(
            resolve_checked_phase(false, &up_to_date, Some(update)),
            UpdatePhase::UpToDate
        ));
    }

    #[test]
    fn only_self_update_capable_installs_with_assets_can_apply() {
        assert!(!can_apply(
            &InstallKind::SystemPackage {
                family: PackageFamily::Deb,
                managed: true,
            },
            true,
            false,
            false,
            false
        ));
        assert!(can_apply(&appimage(), true, false, false, false));
        assert!(!can_apply(&appimage(), true, true, false, false));
        assert!(can_apply(&appimage(), true, true, true, false));
        assert!(!can_apply(&appimage(), false, false, false, false));
    }

    #[test]
    fn system_package_can_apply_only_when_ready() {
        let install = InstallKind::SystemPackage {
            family: PackageFamily::Pacman,
            managed: true,
        };
        assert!(can_apply(&install, true, false, false, true));
        assert!(!can_apply(&install, true, false, false, false));
        assert!(can_apply(&appimage(), true, false, false, false));
    }

    #[test]
    fn system_package_without_auto_apply_shows_the_package_command() {
        assert!(should_show_package_command(
            &InstallKind::SystemPackage {
                family: PackageFamily::Pacman,
                managed: true,
            },
            false,
            true,
        ));
        assert!(!should_show_package_command(
            &InstallKind::SystemPackage {
                family: PackageFamily::Pacman,
                managed: true,
            },
            true,
            true,
        ));
        assert!(should_show_package_command(
            &InstallKind::SystemPackage {
                family: PackageFamily::Pacman,
                managed: true,
            },
            true,
            false,
        ));
        assert!(should_show_package_command(
            &InstallKind::SystemPackage {
                family: PackageFamily::Pacman,
                managed: false,
            },
            false,
            true,
        ));
        assert!(!can_apply(
            &InstallKind::SystemPackage {
                family: PackageFamily::Pacman,
                managed: false,
            },
            true,
            false,
            false,
            false
        ));
    }

    #[test]
    fn result_notice_maps_auth_and_lock_codes() {
        let cancelled = LastResult {
            status: LastStatus::Failed,
            from: "0.9.2".to_string(),
            to: "0.10.0".to_string(),
            reason: None,
            code: Some(LastErrorCode::AuthCancelled),
        };
        let notice = result_notice(&cancelled, "0.9.2").unwrap();
        assert!(notice.message_in("ja").contains("キャンセル"));
        assert!(notice.message_in("en").contains("cancel"));

        let locked = LastResult {
            status: LastStatus::Failed,
            from: "0.9.2".to_string(),
            to: "0.10.0".to_string(),
            reason: None,
            code: Some(LastErrorCode::PackageManagerLocked),
        };
        let notice = result_notice(&locked, "0.9.2").unwrap();
        assert!(notice.message_in("ja").contains("ロック"));
    }

    #[test]
    fn failed_notice_is_shown_on_the_new_version() {
        let result = LastResult {
            status: LastStatus::Failed,
            from: "0.9.2".to_string(),
            to: "0.10.0".to_string(),
            reason: Some("health".to_string()),
            code: Some(LastErrorCode::HealthCheckFailed),
        };
        assert!(result_notice(&result, "0.10.0").is_some());
        assert!(result_notice(&result, "0.9.2").is_some());
        assert!(result_notice(&result, "0.8.0").is_none());
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
        use vrc_photon_update::IoOperation;

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
                let message = update_error_message(error).message();
                assert!(message.contains(*expected), "{message}");
                message
            })
            .collect();
        assert_eq!(messages.len(), cases.len());
    }

    #[test]
    fn keeps_existing_update_error_messages() {
        assert!(
            update_error_message(&UpdateError::Signature)
                .message()
                .contains("署名")
        );
        assert!(
            update_error_message(&UpdateError::Integrity("x".into()))
                .message()
                .contains("検証")
        );
        assert!(
            update_error_message(&UpdateError::Network("x".into()))
                .message()
                .contains("ネットワーク")
        );
    }

    #[test]
    fn labels_install_kinds_and_package_commands() {
        assert_eq!(install_label(&appimage()), "AppImage");
        assert_eq!(
            install_label(&InstallKind::SystemPackage {
                family: PackageFamily::Deb,
                managed: true,
            }),
            "deb"
        );
        assert_eq!(
            package_command(&InstallKind::SystemPackage {
                family: PackageFamily::Pacman,
                managed: true,
            }),
            Some("sudo pacman -Syu vrc-photon")
        );
        assert_eq!(package_command(&appimage()), None);
    }
}

use super::ipc_bridge;
use crate::ui::components::glass_panel;
use crate::ui::theme::Palette;
use gpui_kit::base::{Disableable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::prelude::*;
use gpui_kit::{Context, Div, Window, div, px, relative, rgb};

#[derive(Debug, Clone)]
pub(crate) struct UpdateText {
    key: &'static str,
    args: Vec<(&'static str, String)>,
}

impl UpdateText {
    fn key(key: &'static str) -> Self {
        Self {
            key,
            args: Vec::new(),
        }
    }

    fn arg(mut self, name: &'static str, value: impl ToString) -> Self {
        self.args.push((name, value.to_string()));
        self
    }

    fn message(&self) -> String {
        self.message_in(&vrc_photon_i18n::current_locale())
    }

    fn message_in(&self, locale: &str) -> String {
        vrc_photon_i18n::tr_args_in(locale, self.key, &self.args)
    }
}

enum UpdateMessage {
    Checked {
        manual: bool,
        result: std::result::Result<Option<AvailableUpdate>, UpdateText>,
    },
    DownloadProgress {
        received: u64,
        total: Option<u64>,
    },
    Downloaded {
        staged: PathBuf,
    },
    DownloadFailed(UpdateText),
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
    Error(UpdateText),
}

impl UpdatePhase {
    fn badge(&self, p: &Palette) -> (String, u32, u32) {
        match self {
            Self::Idle => (tr!("update.phase.idle"), p.muted, p.panel_2),
            Self::Checking => (tr!("update.phase.checking"), p.accent, p.accent_soft),
            Self::UpToDate => (tr!("update.phase.up_to_date"), p.green, p.green_soft),
            Self::Available(_) => (tr!("update.phase.available"), p.amber, p.amber_soft),
            Self::Downloading { .. } => (tr!("update.phase.downloading"), p.accent, p.accent_soft),
            Self::Ready { .. } => (tr!("update.phase.ready"), p.green, p.green_soft),
            Self::Applying => (tr!("update.phase.applying"), p.amber, p.amber_soft),
            Self::Error(_) => (tr!("update.phase.error"), p.red, p.red_soft),
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
    result: &std::result::Result<Option<AvailableUpdate>, UpdateText>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateNoticeKind {
    Success,
    RolledBack,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct UpdateNotice {
    kind: UpdateNoticeKind,
    version: String,
    reason: Option<String>,
    status: LastStatus,
    code: Option<LastErrorCode>,
}

impl UpdateNotice {
    pub(crate) fn message(&self) -> String {
        self.message_in(&vrc_photon_i18n::current_locale())
    }

    pub(crate) fn message_in(&self, locale: &str) -> String {
        match self.kind {
            UpdateNoticeKind::Success => vrc_photon_i18n::tr_args_in(
                locale,
                "update.notice.success",
                &[("version", self.version.clone())],
            ),
            UpdateNoticeKind::RolledBack => match &self.reason {
                Some(reason) => vrc_photon_i18n::tr_args_in(
                    locale,
                    "update.notice.rolled_back",
                    &[
                        ("version", self.version.clone()),
                        ("reason", reason.clone()),
                    ],
                ),
                None => vrc_photon_i18n::tr_args_in(
                    locale,
                    "update.notice.rolled_back_no_reason",
                    &[("version", self.version.clone())],
                ),
            },
            UpdateNoticeKind::Failed => match self.code {
                Some(LastErrorCode::AuthCancelled) => {
                    vrc_photon_i18n::tr_in(locale, "update.notice.auth_cancelled")
                }
                Some(LastErrorCode::AuthUnavailable) => {
                    vrc_photon_i18n::tr_in(locale, "update.notice.auth_unavailable")
                }
                Some(LastErrorCode::PackageManagerLocked) => {
                    vrc_photon_i18n::tr_in(locale, "update.notice.package_locked")
                }
                Some(LastErrorCode::PackageManagerFailed) => {
                    vrc_photon_i18n::tr_in(locale, "update.notice.package_failed")
                }
                Some(LastErrorCode::PackageVerificationFailed) => {
                    vrc_photon_i18n::tr_in(locale, "update.error.package_verify")
                }
                Some(LastErrorCode::PackageMetadataMismatch) => {
                    vrc_photon_i18n::tr_in(locale, "update.notice.package_mismatch")
                }
                Some(LastErrorCode::Downgrade) => {
                    vrc_photon_i18n::tr_in(locale, "update.notice.downgrade")
                }
                Some(LastErrorCode::HealthCheckFailed) => {
                    vrc_photon_i18n::tr_in(locale, "update.notice.health_failed")
                }
                Some(LastErrorCode::InstallOutcomeUnknown)
                | Some(LastErrorCode::HelperFailed)
                | None => match &self.reason {
                    Some(reason) => vrc_photon_i18n::tr_args_in(
                        locale,
                        "update.notice.failed",
                        &[("reason", reason.clone())],
                    ),
                    None => vrc_photon_i18n::tr_in(locale, "update.notice.failed_no_reason"),
                },
            },
        }
    }

    pub(crate) fn color(&self, p: &Palette) -> u32 {
        match self.status {
            LastStatus::Success => p.green,
            LastStatus::RolledBack => p.amber,
            LastStatus::Failed => p.red,
        }
    }
}

/// UI state for the signed application updater.
pub struct UpdateState {
    pub phase: UpdatePhase,
    pub install: InstallKind,
    /// Whether a system package can be downloaded and applied in-app.
    pub system_apply_available: bool,
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
            vrc_photon_update::install_kind()
        };
        let data_dir = vrc_photon_core::data_dir();
        let last_result = if demo || !database_ok {
            None
        } else {
            data_dir
                .as_deref()
                .and_then(|dir| LastResult::take(&UpdatePaths::new(dir)))
        };
        let auth_unavailable = last_result
            .as_ref()
            .is_some_and(|result| result.code == Some(LastErrorCode::AuthUnavailable));
        let system_apply_available = install.system_package_apply(
            vrc_photon_update::pkexec_available(),
            vrc_photon_update::system_helper_available(),
            vrc_photon_update::system_policy_available(),
        ) && !auth_unavailable;
        let notice = last_result
            .and_then(|result| result_notice(&result, vrc_photon_update::CURRENT_VERSION));
        let (tx, rx) = async_channel::unbounded();
        Self {
            phase: UpdatePhase::Idle,
            install,
            system_apply_available,
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
        .then(|| tr!("update.badge"))
    }

    pub(crate) fn notice(&self) -> Option<&UpdateNotice> {
        self.notice.as_ref()
    }

    fn write_health_marker(&self) -> std::result::Result<(), String> {
        if !self.database_ok || self.demo {
            return Err(tr!("update.error.database"));
        }
        let token = self
            .health_token
            .as_deref()
            .ok_or_else(|| tr!("update.error.token"))?;
        let data_dir = self
            .data_dir
            .as_deref()
            .ok_or_else(|| tr!("update.error.data_dir"))?;
        let paths = UpdatePaths::new(data_dir);
        paths.ensure().map_err(|error| error.to_string())?;
        vrc_photon_update::helper::write_health_marker(
            &paths,
            token,
            vrc_photon_update::CURRENT_VERSION,
            self.db_path.as_deref(),
        )
        .map_err(|error| error.to_string())
    }
}

fn update_error_message(error: &UpdateError) -> UpdateText {
    match error {
        UpdateError::Network(_) => UpdateText::key("update.error.network"),
        UpdateError::Signature => UpdateText::key("update.error.signature"),
        UpdateError::Integrity(_) => UpdateText::key("update.error.integrity"),
        UpdateError::InvalidManifest(_) => UpdateText::key("update.error.manifest"),
        UpdateError::UnsupportedPlatform => UpdateText::key("update.error.unsupported"),
        UpdateError::Cancelled => UpdateText::key("update.error.cancelled"),
        UpdateError::NotFound => UpdateText::key("update.error.not_found"),
        UpdateError::HelperMissing => UpdateText::key("update.error.helper_missing_reinstall"),
        UpdateError::Io { operation, .. } => match operation {
            IoOperation::Helper => UpdateText::key("update.error.helper_prepare"),
            IoOperation::Job => UpdateText::key("update.error.job_write_past"),
            IoOperation::Apply => UpdateText::key("update.error.appimage_replace_past"),
            IoOperation::Spawn => UpdateText::key("update.error.helper_spawn_past"),
            IoOperation::Download | IoOperation::Other => {
                UpdateText::key("update.error.file_save_past")
            }
        },
        UpdateError::InvalidJob(_) => UpdateText::key("update.error.invalid_job"),
        UpdateError::Other(message) => {
            UpdateText::key("update.error.other").arg("message", message)
        }
        UpdateError::SystemHelperMissing => UpdateText::key("update.error.system_helper_missing"),
        UpdateError::PackageManagerLocked => UpdateText::key("update.error.package_locked"),
        UpdateError::PackageMetadataMismatch(_) => UpdateText::key("update.error.package_mismatch"),
        UpdateError::PackageVerificationFailed(_) => UpdateText::key("update.error.package_verify"),
        UpdateError::Downgrade { .. } => UpdateText::key("update.error.downgrade"),
        UpdateError::PackageManagerFailed { .. } => UpdateText::key("update.error.package_failed"),
        UpdateError::NotPrivileged
        | UpdateError::StagingRejected(_)
        | UpdateError::PackageManagerUnavailable => {
            UpdateText::key("update.error.other").arg("message", error.to_string())
        }
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
                self.toast_info(tr!("update.toast.checking"), cx);
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
                let Ok(current) = Version::parse(vrc_photon_update::CURRENT_VERSION) else {
                    let _ = tx.send_blocking(UpdateMessage::Checked {
                        manual,
                        result: Err(UpdateText::key("update.error.version_parse")),
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
                _ => UpdatePhase::Error(
                    UpdateText::key("update.error.check_thread").arg("error", error),
                ),
            };
            self.toast_error(tr!("update.error.check_failed"), cx);
            cx.notify();
        }
    }

    pub(crate) fn start_update_download(&mut self, cx: &mut Context<Self>) {
        if !appimage_writable(&self.update.install) {
            self.update.phase =
                UpdatePhase::Error(UpdateText::key("update.error.appimage_not_writable"));
            self.toast_error(tr!("update.error.appimage_not_writable"), cx);
            cx.notify();
            return;
        }
        let UpdatePhase::Available(update) = &self.update.phase else {
            return;
        };
        let update = update.clone();
        let Some(asset) = update.asset.clone() else {
            self.update.phase = UpdatePhase::Error(UpdateText::key("update.error.unsupported"));
            cx.notify();
            return;
        };
        let Some(data_dir) = self.update.data_dir.clone() else {
            self.update.phase = UpdatePhase::Error(UpdateText::key("update.error.data_dir"));
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
        let paths = UpdatePaths::new(&data_dir);
        if let Err(error) = paths.ensure() {
            self.update.phase = UpdatePhase::Error(update_error_message(&error));
            cx.notify();
            return;
        }
        let dest = paths
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
            self.update.phase = UpdatePhase::Error(
                UpdateText::key("update.error.download_start").arg("error", error),
            );
            self.toast_error(tr!("update.error.download_failed"), cx);
            cx.notify();
        }
    }

    pub(crate) fn apply_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.update.phase, UpdatePhase::Ready { .. }) {
            return;
        }
        let entity = cx.entity();
        let panel_deep = self.palette.panel_deep;
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            alert
                .title(tr!("update.apply.title"))
                .bg(rgb(panel_deep))
                .description(tr!("update.apply.description"))
                .confirm()
                .ok_text(tr!("update.apply.ok"))
                .ok_variant(ButtonVariant::Primary)
                .cancel_text(tr!("common.cancel"))
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
            self.update.phase = UpdatePhase::Error(UpdateText::key("update.error.unsupported"));
            cx.notify();
            return;
        };
        if matches!(self.update.install, InstallKind::SystemPackage { .. })
            && !self.update.system_apply_available
        {
            self.toast_warning(tr!("update.error.not_supported"), cx);
            cx.notify();
            return;
        }
        if !appimage_writable(&self.update.install) {
            self.toast_error(tr!("update.error.appimage_not_writable"), cx);
            cx.notify();
            return;
        }
        if !can_apply(
            &self.update.install,
            true,
            self.daemon_active,
            self.ipc_connected,
            self.update.system_apply_available,
        ) {
            if self.daemon_active && !self.ipc_connected {
                self.toast_error(tr!("update.error.daemon_connected"), cx);
            } else {
                self.toast_warning(tr!("update.error.not_supported"), cx);
            }
            cx.notify();
            return;
        }
        if let Some(data_dir) = self.update.data_dir.as_deref()
            && vrc_photon_update::helper::apply_in_progress(&UpdatePaths::new(data_dir))
        {
            self.toast_warning(tr!("update.error.in_progress"), cx);
            cx.notify();
            return;
        }
        let Some(data_dir) = self.update.data_dir.clone() else {
            self.toast_error(tr!("update.error.data_dir"), cx);
            cx.notify();
            return;
        };
        let Ok(current_exe) = std::env::current_exe() else {
            self.toast_error(tr!("update.error.executable"), cx);
            cx.notify();
            return;
        };
        let working_dir = match std::env::current_dir() {
            Ok(dir) => dir,
            Err(error) => {
                let message = UpdateText::key("update.error.cwd").arg("error", error);
                self.update.phase = UpdatePhase::Error(message.clone());
                self.toast_error(message.message(), cx);
                cx.notify();
                return;
            }
        };
        let restart_args: Vec<NativeString> =
            std::env::args_os().skip(1).map(NativeString::new).collect();
        let (kind, target) = match &self.update.install {
            InstallKind::AppImage { path } => (ApplyKind::AppImage, path.clone()),
            InstallKind::WindowsCurrentUser { .. } => (ApplyKind::Windows, current_exe.clone()),
            InstallKind::SystemPackage { family, .. } => (
                ApplyKind::SystemPackage { family: *family },
                current_exe.clone(),
            ),
            _ => {
                self.toast_warning(tr!("update.error.not_supported"), cx);
                cx.notify();
                return;
            }
        };
        let (signed_manifest, manifest_signature) = match &kind {
            ApplyKind::SystemPackage { .. } => {
                let bytes = match String::from_utf8(update.signed_manifest.bytes.clone()) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::error!(%error, "signed manifest is not valid UTF-8");
                        let message = UpdateText::key("update.error.manifest");
                        self.update.phase = UpdatePhase::Error(message.clone());
                        self.toast_error(message.message(), cx);
                        cx.notify();
                        return;
                    }
                };
                (Some(bytes), Some(update.signed_manifest.signature.clone()))
            }
            _ => (None, None),
        };
        let job = ApplyJob {
            schema: vrc_photon_update::APPLY_SCHEMA,
            kind,
            from_version: vrc_photon_update::CURRENT_VERSION.to_string(),
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
            signed_manifest,
            manifest_signature,
        };

        // The supervisor is not loaded into App until the first monitor
        // refresh, so read the recorded value from the database.
        let supervisor = self
            .daemon_supervisor
            .clone()
            .or_else(|| self.db.get_setting("daemon_supervisor").ok().flatten());
        let endpoint = self.ipc_endpoint.clone();
        let shutdown = self.ipc_connected;
        let apply_data_dir = job.data_dir.clone();
        cx.spawn(async move |this, cx| {
            if shutdown
                && let Some(endpoint) = endpoint
                && let Err(error) = cx
                    .background_executor()
                    .spawn(async move {
                        ipc_bridge::request(endpoint, vrc_photon_ipc::Request::Shutdown)
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
                        this.toast_error(message.message(), cx);
                        if this.daemon_active {
                            let _ = start_daemon(supervisor.as_deref());
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
            self.toast_error(tr_args!("update.error.health_record", error = error), cx);
            tracing::error!(%error, "cannot write update health marker");
            cx.notify();
            return;
        }
        // A restarted UI has not refreshed the monitor yet, so the recorded
        // supervisor is read from the database when the field is empty.
        let supervisor = self
            .daemon_supervisor
            .clone()
            .or_else(|| self.db.get_setting("daemon_supervisor").ok().flatten());
        if !self.demo
            && let Err(error) = restart_daemon_after_update(supervisor.as_deref())
        {
            self.toast_error(tr_args!("update.error.daemon_start", error = error), cx);
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
                                tr_args!("update.toast.available", version = update.version),
                                cx,
                            );
                        } else {
                            self.toast_info(
                                tr_args!("update.toast.available_short", version = update.version),
                                cx,
                            );
                        }
                    }
                    Ok(None) => {
                        if manual {
                            self.toast_success(tr!("update.latest"), cx);
                        }
                    }
                    Err(message) => {
                        if manual {
                            self.toast_error(message.message(), cx);
                        } else {
                            tracing::warn!(message = %message.message(), "automatic update check failed");
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
                    self.toast_success(tr!("update.toast.downloaded"), cx);
                    cx.notify();
                }
            }
            UpdateMessage::DownloadFailed(message) => {
                self.update.phase = UpdatePhase::Error(message.clone());
                self.toast_error(message.message(), cx);
                cx.notify();
            }
        }
    }

    pub(crate) fn update_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let phase = self.update.phase.clone();
        let install = self.update.install.clone();
        let last_checked = last_checked_label(self.settings.update_last_checked_at);
        let demo = self.demo;
        let busy = self.update.busy();
        let has_asset = match &phase {
            UpdatePhase::Available(update)
            | UpdatePhase::Downloading { update, .. }
            | UpdatePhase::Ready { update, .. } => update.asset.is_some(),
            _ => false,
        };
        let (status, fg, bg) = phase.badge(&p);

        let mut card = glass_panel(&self.palette)
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
                            .text_color(rgb(p.text))
                            .child(tr!("update.title")),
                    )
                    .child(self.badge(status, fg, bg)),
            )
            .child(self.meta_row(
                tr!("update.current_version"),
                format!("v{}", vrc_photon_update::CURRENT_VERSION),
            ))
            .child(self.meta_row(
                tr!("update.install_kind"),
                install_label(&install).to_string(),
            ))
            .child(self.meta_row(tr!("update.last_checked"), last_checked));

        if demo {
            return card.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(tr!("update.demo_unavailable")),
            );
        }

        if let Some(notice) = self.update.notice() {
            card = card.child(
                div()
                    .p(px(10.))
                    .rounded(px(8.))
                    .bg(rgb(p.panel_2))
                    .text_size(px(11.))
                    .text_color(rgb(notice.color(&p)))
                    .child(notice.message()),
            );
        }

        card = card.child(self.setting_switch(
            tr!("update.auto_check"),
            tr!("update.auto_check_hint"),
            self.settings.auto_update_check,
            false,
            cx,
            |settings, value| settings.auto_update_check = value,
        ));

        card = card.child(
            h_flex().flex_wrap().gap(px(8.)).child(
                Button::new("update-check")
                    .label(tr!("update.check_now"))
                    .small()
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_update_check(true, cx);
                    })),
            ),
        );

        card = card.child(self.update_phase_view(&phase, cx));

        if should_show_package_command(&install, self.update.system_apply_available, has_asset) {
            if let Some(command) = package_command(&install) {
                card = card.child(
                    v_flex()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(p.muted))
                                .child(tr!("update.package_manager")),
                        )
                        .child(
                            div()
                                .font_family("monospace")
                                .text_size(px(10.))
                                .text_color(rgb(p.text))
                                .child(command),
                        ),
                );
            } else {
                card = card.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child(tr!("update.package_manager_hint")),
                );
            }
        } else if matches!(install, InstallKind::SystemPackage { .. }) {
            card = card.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted_2))
                    .child(tr!("update.system_update_hint")),
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
                    .text_color(rgb(p.muted_2))
                    .child(tr!("update.not_automatic")),
            );
        }

        card
    }

    fn update_phase_view(&mut self, phase: &UpdatePhase, cx: &mut Context<Self>) -> Div {
        let p = self.palette;
        match phase {
            UpdatePhase::Idle => div(),
            UpdatePhase::Checking => div()
                .text_size(px(11.))
                .text_color(rgb(p.muted))
                .child(tr!("update.checking")),
            UpdatePhase::UpToDate => div()
                .text_size(px(11.))
                .text_color(rgb(p.green))
                .child(tr!("update.latest")),
            UpdatePhase::Available(update) => self.update_available_view(update, cx),
            UpdatePhase::Downloading {
                received, total, ..
            } => update_downloading_view(&p, *received, *total),
            UpdatePhase::Ready { update, .. } => self.update_ready_view(update, cx),
            UpdatePhase::Applying => div()
                .text_size(px(11.))
                .text_color(rgb(p.amber))
                .child(tr!("update.applying")),
            UpdatePhase::Error(message) => div()
                .text_size(px(11.))
                .text_color(rgb(p.red))
                .child(message.message()),
        }
    }

    fn update_available_view(&mut self, update: &AvailableUpdate, cx: &mut Context<Self>) -> Div {
        let p = self.palette;
        let install = self.update.install.clone();
        let can_download = update.asset.is_some()
            && match &install {
                InstallKind::AppImage { .. } => appimage_writable(&install),
                InstallKind::SystemPackage { .. } => self.update.system_apply_available,
                InstallKind::WindowsCurrentUser { .. } => true,
                _ => false,
            };
        let show_not_writable = update.asset.is_some()
            && !can_download
            && matches!(install, InstallKind::AppImage { .. });
        let show_fallback = matches!(install, InstallKind::SystemPackage { .. }) && !can_download;
        v_flex()
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(rgb(p.text))
                    .child(tr_args!("update.available", version = update.version)),
            )
            .when_some(update.notes_url.clone(), |this, url| {
                this.child(
                    Button::new("update-notes")
                        .label(tr!("update.release_notes"))
                        .small()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Err(error) = open_path(Path::new(&url)) {
                                this.toast_error(
                                    tr_args!("update.error.release_notes", error = error),
                                    cx,
                                );
                                cx.notify();
                            }
                        })),
                )
            })
            .when(can_download, |this| {
                this.child(
                    Button::new("update-download")
                        .label(tr!("update.download"))
                        .small()
                        .primary()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.start_update_download(cx);
                        })),
                )
            })
            .when(show_not_writable, |this| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(p.amber))
                        .child(tr!("update.appimage_not_writable_short")),
                )
            })
            .when(show_fallback, |this| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(p.muted_2))
                        .child(tr!("update.package_manager_fallback")),
                )
            })
    }

    fn update_ready_view(&mut self, update: &AvailableUpdate, cx: &mut Context<Self>) -> Div {
        let p = self.palette;
        let apply_ok = can_apply(
            &self.update.install,
            update.asset.is_some(),
            self.daemon_active,
            self.ipc_connected,
            self.update.system_apply_available,
        ) && appimage_writable(&self.update.install);
        v_flex()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.green))
                    .child(tr!("update.downloaded")),
            )
            .child(
                Button::new("update-apply")
                    .label(tr!("update.apply.ok"))
                    .small()
                    .primary()
                    .disabled(!apply_ok)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.apply_update(window, cx);
                    })),
            )
            .when(!apply_ok, |this| {
                this.child(div().text_size(px(10.)).text_color(rgb(p.amber)).child(
                    if self.daemon_active && !self.ipc_connected {
                        tr!("update.error.daemon_connected")
                    } else {
                        tr!("update.error.not_supported")
                    },
                ))
            })
    }
}

fn update_downloading_view(palette: &Palette, received: u64, total: Option<u64>) -> Div {
    let p = palette;
    let ratio = total
        .map(|total| (received as f32 / total as f32).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    v_flex()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(p.muted))
                .child(tr_args!(
                    "update.downloading",
                    percent = format!("{:.0}", ratio * 100.0),
                    received = format_bytes(received),
                    total = total.map(format_bytes).unwrap_or_else(|| "?".to_string())
                )),
        )
        .child(
            div()
                .h(px(6.))
                .w_full()
                .rounded_full()
                .bg(rgb(p.panel_2))
                .child(
                    div()
                        .h_full()
                        .w(relative(ratio))
                        .rounded_full()
                        .bg(rgb(p.accent)),
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
        return tr!("update.last_checked.never");
    };
    let Some(timestamp) = DateTime::from_timestamp(checked_at, 0) else {
        return tr!("update.last_checked.unknown");
    };
    timestamp
        .with_timezone(&Local)
        .format("%Y/%m/%d %H:%M")
        .to_string()
}

fn result_notice(result: &LastResult, current_version: &str) -> Option<UpdateNotice> {
    match result.status {
        LastStatus::Success if result.to == current_version => Some(UpdateNotice {
            kind: UpdateNoticeKind::Success,
            version: result.to.clone(),
            reason: None,
            status: LastStatus::Success,
            code: result.code,
        }),
        LastStatus::RolledBack if result.from == current_version => Some(UpdateNotice {
            kind: UpdateNoticeKind::RolledBack,
            version: result.from.clone(),
            reason: result.reason.clone(),
            status: LastStatus::RolledBack,
            code: result.code,
        }),
        LastStatus::Failed if result.from == current_version || result.to == current_version => {
            Some(UpdateNotice {
                kind: UpdateNoticeKind::Failed,
                version: result.from.clone(),
                reason: result.reason.clone(),
                status: LastStatus::Failed,
                code: result.code,
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
    system_apply_available: bool,
) -> bool {
    let automatic = install.automatic_apply()
        || (matches!(install, InstallKind::SystemPackage { .. }) && system_apply_available);
    automatic && has_asset && (!daemon_active || ipc_connected)
}

fn should_show_package_command(
    install: &InstallKind,
    system_apply_available: bool,
    has_asset: bool,
) -> bool {
    matches!(install, InstallKind::SystemPackage { .. }) && (!system_apply_available || !has_asset)
}

fn appimage_writable(install: &InstallKind) -> bool {
    let InstallKind::AppImage { path } = install else {
        return true;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    let probe = parent.join(format!(".vrc-photon-write-test-{}", std::process::id()));
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

fn install_label(install: &InstallKind) -> String {
    match install {
        InstallKind::AppImage { .. } => "AppImage".to_string(),
        InstallKind::WindowsCurrentUser { .. } | InstallKind::WindowsPerMachine { .. } => {
            "Windows (NSIS)".to_string()
        }
        InstallKind::WindowsOther => tr!("update.install.windows_manual"),
        InstallKind::SystemPackage { family, .. } => match family {
            vrc_photon_update::PackageFamily::Deb => "deb".to_string(),
            vrc_photon_update::PackageFamily::Pacman => "pacman".to_string(),
            vrc_photon_update::PackageFamily::Unknown => tr!("update.install.linux_package"),
        },
        InstallKind::Development => tr!("update.install.development"),
    }
}

fn package_command(install: &InstallKind) -> Option<&'static str> {
    match install {
        InstallKind::SystemPackage { family, .. } => family.update_command(),
        _ => None,
    }
}
