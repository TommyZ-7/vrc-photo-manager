//! App-side daemon state: IPC status, monitor session, and daemon commands.
//!
//! Rendering stays in `ui::dashboard`; these methods only touch App fields,
//! the daemon command queue, and the IPC bridge.

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::Utc;
use gpui_kit::*;
use vrc_photo_core::model::{DaemonCommand, DaemonOperation};
use vrc_photo_i18n::{tr, tr_args};
use vrc_photo_ipc::{Request as IpcRequest, ScanPhase as IpcScanPhase, StatusSnapshot};

use crate::app::App;
use crate::app::lifecycle::ScanProgressView;

pub(crate) fn daemon_liveness(age_secs: Option<i64>, ipc_connected: bool) -> bool {
    ipc_connected || age_secs.is_some_and(|age| age < 20)
}

pub(crate) fn daemon_state_label(active: bool, paused: bool) -> String {
    if !active {
        tr!("daemon.state.stopped")
    } else if paused {
        tr!("daemon.state.paused")
    } else {
        tr!("daemon.state.running")
    }
}

pub(crate) fn ipc_status_label(connected: bool, error: Option<&str>) -> String {
    if connected {
        tr!("daemon.ipc.connected")
    } else if let Some(error) = error {
        tr_args!("daemon.ipc.disconnected_error", error = error)
    } else {
        tr!("daemon.ipc.disconnected")
    }
}

pub(crate) fn daemon_sync_label(
    age_secs: Option<i64>,
    supervisor: Option<&str>,
    ipc_connected: bool,
) -> String {
    match age_secs.filter(|age| *age < 20) {
        Some(age) if supervisor == Some("systemd") => {
            tr_args!("daemon.sync.age_systemd", age = age)
        }
        Some(age) => tr_args!("daemon.sync.age", age = age),
        None if ipc_connected => tr!("daemon.sync.ipc"),
        None => tr!("daemon.sync.stopped"),
    }
}

pub(crate) fn autostart_description(daemon_active: bool, supervisor: Option<&str>) -> String {
    if daemon_active && supervisor == Some("systemd") {
        tr!("daemon.autostart.systemd")
    } else {
        tr!("daemon.autostart.login")
    }
}

pub(crate) fn daemon_operation_label(operation: DaemonOperation) -> String {
    match operation {
        DaemonOperation::Scan => tr!("daemon.operation.scan"),
        DaemonOperation::Log => tr!("daemon.operation.log"),
        DaemonOperation::Link => tr!("daemon.operation.link"),
    }
}

pub(crate) fn format_timestamp(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|timestamp| {
            timestamp
                .with_timezone(&chrono::Local)
                .format("%m/%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| raw.to_string())
}

#[cfg(target_os = "windows")]
pub(crate) fn open_path(path: &Path) -> Result<(), String> {
    // explorer.exe reports a non-zero exit code even when it opens the folder,
    // so success is defined by the process starting.
    Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| tr_args!("daemon.error.explorer", error = error))
}

#[cfg(target_os = "macos")]
pub(crate) fn open_path(path: &Path) -> Result<(), String> {
    let status = Command::new("open")
        .arg(path)
        .status()
        .map_err(|error| tr_args!("daemon.error.open", error = error))?;
    if status.success() {
        Ok(())
    } else {
        Err(tr_args!("daemon.error.open_status", status = status))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn open_path(path: &Path) -> Result<(), String> {
    match open_status("xdg-open", &[], path) {
        Ok(()) => Ok(()),
        Err(first) => {
            open_status("gio", &["open"], path).map_err(|second| format!("{first} / {second}"))
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_status(program: &str, args: &[&str], path: &Path) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .map_err(|error| tr_args!("daemon.error.spawn", program = program, error = error))?;
    if status.success() {
        Ok(())
    } else {
        Err(tr_args!(
            "daemon.error.spawn_status",
            program = program,
            status = status
        ))
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
pub(crate) fn open_path(path: &Path) -> Result<(), String> {
    Err(tr_args!(
        "daemon.error.no_file_manager",
        path = path.display()
    ))
}

fn daemon_binary_path() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "vrc-photo-daemon.exe"
    } else {
        "vrc-photo-daemon"
    };
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|parent| parent.join(name))
}

pub(crate) fn spawn_daemon_process() -> Result<(), String> {
    let path = daemon_binary_path().ok_or_else(|| "daemon binary path not found".to_string())?;
    if !path.is_file() {
        return Err(tr_args!("daemon.error.not_found", path = path.display()));
    }
    Command::new(&path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(crate) fn run_background_supervisor() -> Result<(), String> {
    let path = daemon_binary_path().ok_or_else(|| "daemon binary path not found".to_string())?;
    if !path.is_file() {
        return Err(tr_args!("daemon.error.not_found", path = path.display()));
    }
    let mut child = Command::new(&path)
        .spawn()
        .map_err(|error| error.to_string())?;
    child.wait().map_err(|error| error.to_string())?;
    Ok(())
}

impl App {
    pub(crate) fn apply_daemon_status(&mut self, status: StatusSnapshot) {
        self.daemon_paused = status.paused;
        self.scan_progress = status.scanning.then(|| ScanProgressView {
            phase: status.scan_phase.unwrap_or(IpcScanPhase::Walking),
            processed: status.scan_processed,
            total: status.scan_total,
            indexed: status.scan_indexed,
            root: status.scan_root,
            roots: status.scan_roots,
        });
        self.daemon_active = daemon_liveness(self.daemon_age_secs, self.ipc_connected);
    }

    pub(crate) fn send_wake_commands(&self, cx: &mut Context<Self>) {
        self.wake_daemon(IpcRequest::WakeCommands, cx);
    }

    pub(crate) fn send_wake_jobs(&self, cx: &mut Context<Self>) {
        self.wake_daemon(IpcRequest::WakeJobs, cx);
    }

    pub(crate) fn request_daemon_rescan(&mut self, cx: &mut Context<Self>) {
        match self.db.request_daemon_command(DaemonCommand::Rescan) {
            Ok(_) => {
                self.send_wake_commands(cx);
                self.toast_info(
                    if self.ipc_connected {
                        tr!("daemon.rescan.requested")
                    } else {
                        tr!("daemon.rescan.queued")
                    },
                    cx,
                );
            }
            Err(error) => {
                self.toast_error(tr_args!("daemon.rescan.error", error = error), cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn toggle_daemon_pause(&mut self, cx: &mut Context<Self>) {
        if !self.ipc_connected {
            self.toast_warning(tr!("daemon.stopped"), cx);
            cx.notify();
            return;
        }
        let paused = !self.daemon_paused;
        let success = if paused {
            tr!("daemon.paused")
        } else {
            tr!("daemon.resumed")
        };
        self.send_ipc_request(IpcRequest::SetPaused { paused }, Some(success), None, cx);
    }

    pub(crate) fn toggle_daemon_shutdown(&mut self, cx: &mut Context<Self>) {
        if !self.ipc_connected {
            self.toast_warning(tr!("daemon.stopped"), cx);
            cx.notify();
            return;
        }
        if !self.daemon_shutdown_armed {
            self.daemon_shutdown_armed = true;
            self.toast_warning(tr!("daemon.quit.armed"), cx);
            cx.notify();
            return;
        }
        self.daemon_shutdown_armed = false;
        self.send_ipc_request(
            IpcRequest::Shutdown,
            Some(tr!("daemon.quit.done")),
            None,
            cx,
        );
    }

    pub(crate) fn request_daemon_command(
        &mut self,
        command: DaemonCommand,
        cx: &mut Context<Self>,
    ) {
        match self.db.request_daemon_command(command) {
            Ok(true) => self.toast_info(tr!("daemon.retry.requested"), cx),
            Ok(false) => self.toast_info(tr!("daemon.retry.already"), cx),
            Err(error) => self.toast_error(tr_args!("daemon.retry.error", error = error), cx),
        }
        self.send_wake_commands(cx);
        cx.notify();
    }

    pub(crate) fn refresh_monitor(&mut self) {
        let now = Utc::now().timestamp();
        self.daemon_age_secs = self
            .db
            .get_setting("daemon_heartbeat")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<i64>().ok())
            .map(|timestamp| (now - timestamp).max(0));
        self.daemon_active = daemon_liveness(self.daemon_age_secs, self.ipc_connected);
        self.daemon_supervisor = self.db.get_setting("daemon_supervisor").ok().flatten();
        self.monitor_world = None;
        self.monitor_instance = None;
        self.monitor_players.clear();
        if let Ok(Some(session)) = self.db.session_at(now) {
            if let Ok(Some(world)) = self.db.world_by_id(session.world_id) {
                self.monitor_world = world.alias.or(world.name);
            }
            self.monitor_instance = session.instance_id;
            if let Ok(players) = self.db.players_at(session.id, now) {
                self.monitor_players = players
                    .into_iter()
                    .map(|player| player.display_name)
                    .collect();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        autostart_description, daemon_liveness, daemon_operation_label, daemon_state_label,
        daemon_sync_label, format_timestamp, ipc_status_label,
    };
    use vrc_photo_core::model::DaemonOperation;

    #[test]
    fn daemon_operation_labels_cover_every_operation() {
        assert_eq!(
            daemon_operation_label(DaemonOperation::Scan),
            "写真スキャン"
        );
        assert_eq!(daemon_operation_label(DaemonOperation::Log), "ログ解析");
        assert_eq!(
            daemon_operation_label(DaemonOperation::Link),
            "写真の紐付け"
        );
    }

    #[test]
    fn timestamp_formats_rfc3339_and_falls_back_to_raw() {
        let raw = "2026-10-23T03:04:05+00:00";
        let expected = chrono::DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m/%d %H:%M")
            .to_string();
        assert_eq!(format_timestamp(raw), expected);
        assert_eq!(format_timestamp("not a timestamp"), "not a timestamp");
    }
    #[test]
    fn ipc_status_label_distinguishes_disconnected_and_mismatch() {
        assert_eq!(ipc_status_label(true, None), "IPC 接続中");
        assert!(ipc_status_label(false, Some("protocol mismatch")).contains("未接続"));
    }

    #[test]
    fn daemon_state_label_prefers_stopped_over_paused() {
        assert_eq!(daemon_state_label(false, true), "停止中");
        assert_eq!(daemon_state_label(true, true), "一時停止中");
        assert_eq!(daemon_state_label(true, false), "稼働中");
    }

    #[test]
    fn daemon_liveness_uses_heartbeat_or_ipc() {
        assert!(daemon_liveness(Some(3), false));
        assert!(daemon_liveness(None, true));
        assert!(!daemon_liveness(Some(30), false));
        assert!(!daemon_liveness(None, false));
    }

    #[test]
    fn daemon_sync_label_includes_systemd_supervisor() {
        assert_eq!(
            daemon_sync_label(Some(5), Some("systemd"), false),
            "最終同期 5秒前 · systemd"
        );
        assert_eq!(
            daemon_sync_label(Some(5), Some("app"), false),
            "最終同期 5秒前"
        );
        assert_eq!(
            daemon_sync_label(Some(30), Some("systemd"), false),
            "デーモン停止"
        );
        assert_eq!(daemon_sync_label(None, None, false), "デーモン停止");
        assert_eq!(daemon_sync_label(None, None, true), "IPC 接続中");
        assert_eq!(daemon_sync_label(Some(30), None, true), "IPC 接続中");
    }

    #[test]
    fn autostart_description_mentions_systemd_only_while_active() {
        assert_eq!(
            autostart_description(true, Some("systemd")),
            "systemd 管理で起動中。サービス利用時はOFF推奨"
        );
        assert_eq!(
            autostart_description(false, Some("systemd")),
            "OSログイン時にデーモンを起動"
        );
        assert_eq!(
            autostart_description(true, Some("app")),
            "OSログイン時にデーモンを起動"
        );
    }
}
