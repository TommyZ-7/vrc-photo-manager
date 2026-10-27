//! App-side daemon state: IPC status, monitor session, and daemon commands.
//!
//! Rendering stays in `ui::dashboard`; these methods only touch App fields,
//! the daemon command queue, and the IPC bridge.

use chrono::Utc;
use gpui_kit::*;
use vrc_photo_core::model::DaemonCommand;
use vrc_photo_ipc::{Request as IpcRequest, ScanPhase as IpcScanPhase, StatusSnapshot};

use crate::app::App;
use crate::{ScanProgressView, daemon_liveness};

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
                self.message = Some(if self.ipc_connected {
                    "再スキャンを要求しました。デーモンが処理します。".to_string()
                } else {
                    "再スキャンを要求しました。デーモン起動後に実行されます。".to_string()
                });
            }
            Err(error) => {
                self.message = Some(format!("再スキャンを要求できません: {error}"));
            }
        }
        cx.notify();
    }

    pub(crate) fn toggle_daemon_pause(&mut self, cx: &mut Context<Self>) {
        if !self.ipc_connected {
            self.message = Some("デーモン停止中です".to_string());
            cx.notify();
            return;
        }
        let paused = !self.daemon_paused;
        let success = if paused {
            "監視を一時停止しました"
        } else {
            "監視を再開しました"
        };
        self.send_ipc_request(
            IpcRequest::SetPaused { paused },
            Some(success.to_string()),
            None,
            cx,
        );
    }

    pub(crate) fn toggle_daemon_shutdown(&mut self, cx: &mut Context<Self>) {
        if !self.ipc_connected {
            self.message = Some("デーモン停止中です".to_string());
            cx.notify();
            return;
        }
        if !self.daemon_shutdown_armed {
            self.daemon_shutdown_armed = true;
            self.message = Some("もう一度「デーモンを終了」を押すと終了します".to_string());
            cx.notify();
            return;
        }
        self.daemon_shutdown_armed = false;
        self.send_ipc_request(
            IpcRequest::Shutdown,
            Some("デーモンを終了しました".to_string()),
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
            Ok(true) => {
                self.message = Some("再試行を要求しました。デーモンが処理します。".to_string())
            }
            Ok(false) => self.message = Some("すでに再試行を要求済みです。".to_string()),
            Err(error) => self.message = Some(format!("再試行を要求できません: {error}")),
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
