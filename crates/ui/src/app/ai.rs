//! AI tagging UI state, actions, and daemon event handling.
//!
//! The daemon owns plugin installation, model download, and the tagging
//! worker. This module mirrors that state into the UI, sends the user's
//! requests over IPC, and turns progress events into card and inspector
//! updates. Rendering lives in `ui::settings`, `ui::integrations`, and
//! `ui::library`.

use std::collections::HashMap;
use std::time::Duration;

use gpui_kit::*;
use vrc_photon_ai_protocol::{
    ModelState, PluginInstallStatus, Probe, TaggingJobStatus, TaggingStage,
};
use vrc_photon_core::ai::{
    AiTagDefinition, PhotoAi, PhotoAiTagView, PluginInstallation, PluginState, TaggingJob,
};
use vrc_photon_core::db::Db;
use vrc_photon_core::model::JobStatusCounts;
use vrc_photon_core::settings::AppSettings;
use vrc_photon_i18n::{MISSING, tr, tr_args};
use vrc_photon_ipc::{
    ErrorCode, Event as IpcEvent, Request as IpcRequest, Response as IpcResponse,
};

use crate::app::App;
use crate::app::file_picker::{self, DialogOptions};
use crate::ipc_bridge;

/// Plugin id of the bundled AI tagging sidecar package.
pub(crate) use vrc_photon_core::ai::AI_PLUGIN_ID;

/// Delay before an event burst triggers a state or inspector refresh.
const AI_REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Which operation produced a transient AI error. Keeping the origin lets the
/// settings card show install/model failures and the runtime card show tagging
/// failures without relabelling one as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AiErrorKind {
    Install,
    Model,
    Job,
    /// Probe, variant switch, or plugin uninstall.
    Operation,
}

impl AiErrorKind {
    fn index(self) -> usize {
        match self {
            Self::Install => 0,
            Self::Model => 1,
            Self::Job => 2,
            Self::Operation => 3,
        }
    }
}

/// The latest AI operation failure: a stable code plus the daemon's raw detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiErrorInfo {
    pub(crate) kind: AiErrorKind,
    pub(crate) code: String,
    pub(crate) detail: Option<String>,
}

impl AiErrorInfo {
    pub(crate) fn new(kind: AiErrorKind, code: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            kind,
            code: code.into(),
            detail,
        }
    }
}

/// A plugin uninstall target. `None`/`None` removes the whole plugin (the
/// existing behaviour); a variant/version pair removes just that package.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UninstallTarget {
    pub(crate) variant: Option<String>,
    pub(crate) version: Option<String>,
}

impl UninstallTarget {
    pub(crate) fn plugin_wide() -> Self {
        Self::default()
    }

    pub(crate) fn for_install(install: &PluginInstallation) -> Self {
        Self {
            variant: Some(install.variant.clone()),
            version: Some(install.version.clone()),
        }
    }

    pub(crate) fn is_plugin_wide(&self) -> bool {
        self.variant.is_none() && self.version.is_none()
    }
}

/// Maps a modal target to the IPC fields. A variant-only removal never deletes
/// the plugin-wide AI tags, even if the modal checkbox was somehow checked.
pub(crate) fn uninstall_request_args(
    target: &UninstallTarget,
    delete_ai_tags: bool,
) -> (Option<String>, Option<String>, bool) {
    if target.is_plugin_wide() {
        (None, None, delete_ai_tags)
    } else {
        (target.variant.clone(), target.version.clone(), false)
    }
}

/// Everything the AI tagging cards render. Populated from the database and
/// kept current by daemon events.
#[derive(Default, Clone)]
pub(crate) struct AiUiState {
    pub(crate) installations: Vec<PluginInstallation>,
    pub(crate) active: Option<PluginInstallation>,
    pub(crate) state: Option<PluginState>,
    pub(crate) probe: Option<Probe>,
    pub(crate) tag_definitions: Vec<AiTagDefinition>,
    pub(crate) download: Option<ModelDownloadView>,
    pub(crate) install_busy: bool,
    /// A Recheck or variant switch is waiting for the next state event.
    pub(crate) probe_pending: bool,
    pub(crate) job_counts: JobStatusCounts,
    pub(crate) active_jobs: Vec<TaggingJob>,
    /// File names for the displayed jobs, keyed by photo id.
    pub(crate) active_job_names: HashMap<i64, String>,
    pub(crate) license_modal_open: bool,
    pub(crate) uninstall_modal_open: bool,
    pub(crate) uninstall_target: Option<UninstallTarget>,
    pub(crate) uninstall_delete_tags: bool,
    /// Latest failure per origin, so an install warning and a tagging failure
    /// can stay visible at the same time.
    errors: [Option<AiErrorInfo>; 4],
}

/// Progress of the model download, kept separately from the persisted state
/// so a progress event can update the bar without a database round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelDownloadView {
    pub(crate) bytes: u64,
    pub(crate) total: Option<u64>,
    pub(crate) state: ModelState,
}

impl ModelDownloadView {
    fn from_state(state: &PluginState) -> Option<Self> {
        match state.model_state {
            ModelState::Downloading | ModelState::Verifying => Some(Self {
                bytes: u64::try_from(state.model_bytes_downloaded.max(0)).unwrap_or(0),
                total: state
                    .model_bytes_total
                    .map(|total| u64::try_from(total.max(0)).unwrap_or(0)),
                state: state.model_state,
            }),
            _ => None,
        }
    }
}

/// A complete read of the AI tagging tables.
#[derive(Default)]
pub(crate) struct AiSnapshot {
    pub(crate) installations: Vec<PluginInstallation>,
    pub(crate) active: Option<PluginInstallation>,
    pub(crate) state: Option<PluginState>,
    pub(crate) probe: Option<Probe>,
    pub(crate) tag_definitions: Vec<AiTagDefinition>,
    pub(crate) job_counts: JobStatusCounts,
    pub(crate) active_jobs: Vec<TaggingJob>,
    pub(crate) active_job_names: HashMap<i64, String>,
}

/// The subset the three-second live poller collects on its background thread.
#[derive(Default)]
pub(crate) struct AiLiveSnapshot {
    pub(crate) job_counts: JobStatusCounts,
    pub(crate) active_jobs: Vec<TaggingJob>,
    pub(crate) active_job_names: HashMap<i64, String>,
    pub(crate) state: Option<PluginState>,
    pub(crate) probe: Option<Probe>,
}

impl AiUiState {
    /// True when an active package, a ready model, and a stored probe exist.
    pub(crate) fn ready(&self) -> bool {
        self.active.is_some()
            && self
                .state
                .as_ref()
                .is_some_and(|state| state.model_state == ModelState::Ready)
            && self.probe.is_some()
    }

    pub(crate) fn apply_snapshot(&mut self, snapshot: AiSnapshot) {
        self.installations = snapshot.installations;
        self.active = snapshot.active;
        self.state = snapshot.state;
        self.probe = snapshot.probe;
        self.tag_definitions = snapshot.tag_definitions;
        self.download = self.state.as_ref().and_then(ModelDownloadView::from_state);
        self.job_counts = snapshot.job_counts;
        self.active_jobs = snapshot.active_jobs;
        self.active_job_names = snapshot.active_job_names;
    }

    /// Applies the three-second poller snapshot. Returns whether anything the
    /// cards display changed.
    pub(crate) fn apply_live_snapshot(&mut self, snapshot: AiLiveSnapshot) -> bool {
        let changed = self.job_counts != snapshot.job_counts
            || self.active_jobs != snapshot.active_jobs
            || self.active_job_names != snapshot.active_job_names
            || self.state != snapshot.state
            || self.probe != snapshot.probe;
        if changed {
            self.job_counts = snapshot.job_counts;
            self.active_jobs = snapshot.active_jobs;
            self.active_job_names = snapshot.active_job_names;
            self.state = snapshot.state;
            self.probe = snapshot.probe;
            self.download = self.state.as_ref().and_then(ModelDownloadView::from_state);
        }
        changed
    }

    /// Drops transient connection-scoped state after the IPC link closes: a
    /// disconnected daemon cannot finish an install, a probe, or a download,
    /// and leaving those flags set would wedge the cards.
    pub(crate) fn reset_connection(&mut self) {
        self.install_busy = false;
        self.probe_pending = false;
        self.download = None;
    }

    /// Stores a new error unless it is an exact repeat of the visible one.
    /// Returns true when the error is new and should be reported.
    fn set_error(&mut self, error: AiErrorInfo) -> bool {
        let slot = &mut self.errors[error.kind.index()];
        if slot.as_ref() == Some(&error) {
            return false;
        }
        *slot = Some(error);
        true
    }

    fn clear_error(&mut self, kind: AiErrorKind) {
        self.errors[kind.index()] = None;
    }

    pub(crate) fn error_of(&self, kind: AiErrorKind) -> Option<&AiErrorInfo> {
        self.errors[kind.index()].as_ref()
    }

    /// Marks a Recheck/variant switch as awaiting its state event.
    pub(crate) fn begin_probe(&mut self) {
        self.probe_pending = true;
    }

    /// Resolves a pending probe after a state refresh. Returns true when the
    /// plugin still has no probe, which is the daemon's failure signal.
    pub(crate) fn finish_probe(&mut self) -> bool {
        let was_pending = std::mem::take(&mut self.probe_pending);
        was_pending && self.probe.is_none()
    }

    /// Applies one `TaggingProgress` event before the debounced database
    /// refresh lands. Counts follow the job's previous status so a progress
    /// update for a processing job does not double count it.
    pub(crate) fn apply_tagging_progress(
        &mut self,
        job_id: &str,
        photo_id: i64,
        status: TaggingJobStatus,
        progress: f64,
        stage: Option<TaggingStage>,
        error: Option<&str>,
    ) -> bool {
        let terminal = matches!(
            status,
            TaggingJobStatus::Completed | TaggingJobStatus::Failed | TaggingJobStatus::Cancelled
        );
        let index = self.active_jobs.iter().position(|job| job.id == job_id);
        let previous = index.map(|index| self.active_jobs[index].status);
        if let Some(previous) = previous {
            if previous != status {
                adjust_job_count(&mut self.job_counts, previous, -1);
                if status == TaggingJobStatus::Failed {
                    // A job that reached a terminal failure still counts as a
                    // failure; only cancelled/completed leave the totals.
                    adjust_job_count(&mut self.job_counts, TaggingJobStatus::Failed, 1);
                } else if !terminal {
                    adjust_job_count(&mut self.job_counts, status, 1);
                }
            }
        } else if !terminal {
            adjust_job_count(&mut self.job_counts, status, 1);
        }

        match index {
            Some(index) if terminal => {
                self.active_jobs.remove(index);
            }
            Some(index) => {
                let job = &mut self.active_jobs[index];
                job.status = status;
                job.progress = progress;
                job.stage = stage;
                job.error = error.map(str::to_string);
            }
            None if !terminal => {
                self.active_jobs.insert(
                    0,
                    TaggingJob {
                        id: job_id.to_string(),
                        photo_id,
                        plugin_id: AI_PLUGIN_ID.to_string(),
                        variant: None,
                        status,
                        progress,
                        stage,
                        error: error.map(str::to_string),
                        retry_count: 0,
                        created_at: chrono::Utc::now().to_rfc3339(),
                        started_at: None,
                        finished_at: None,
                    },
                );
                self.active_jobs.truncate(20);
            }
            None => {}
        }

        let new_failure = if status == TaggingJobStatus::Failed
            && let Some(error) = error
        {
            self.set_error(AiErrorInfo::new(AiErrorKind::Job, error, None))
        } else {
            if status == TaggingJobStatus::Completed {
                // A successful tag clears the previous job failure. Install/model
                // failures are owned by their own flows and stay put.
                self.clear_error(AiErrorKind::Job);
            }
            false
        };

        // The card shows the first three rows, so a newly pending row must not
        // push a processing job out of view.
        self.active_jobs.sort_by_key(|job| match job.status {
            TaggingJobStatus::Processing => 0,
            TaggingJobStatus::Pending => 1,
            TaggingJobStatus::Waiting => 2,
            TaggingJobStatus::Failed
            | TaggingJobStatus::Completed
            | TaggingJobStatus::Cancelled => 3,
        });
        self.active_jobs.truncate(20);
        new_failure
    }

    /// Applies an install event. Returns the failure message when it warrants
    /// a toast; repeated failures of the same operation are only reported once.
    pub(crate) fn apply_install_progress(
        &mut self,
        status: PluginInstallStatus,
        error: Option<&str>,
    ) -> Option<String> {
        match status {
            PluginInstallStatus::Running => {
                self.install_busy = true;
                self.clear_error(AiErrorKind::Install);
                None
            }
            PluginInstallStatus::Installed => {
                self.install_busy = false;
                self.clear_error(AiErrorKind::Install);
                None
            }
            PluginInstallStatus::Failed => {
                self.install_busy = false;
                let error = error.unwrap_or("install_failed").to_string();
                let info = AiErrorInfo::new(AiErrorKind::Install, error.clone(), None);
                if self.set_error(info) {
                    Some(error)
                } else {
                    None
                }
            }
        }
    }

    /// Applies a model download event. Returns the failure message to toast
    /// once; progress ticks and repeated identical errors stay silent.
    pub(crate) fn apply_model_download(
        &mut self,
        status: ModelState,
        bytes: u64,
        total: Option<u64>,
        error: Option<&str>,
    ) -> Option<String> {
        match status {
            ModelState::Downloading | ModelState::Verifying => {
                self.clear_error(AiErrorKind::Model);
                self.download = Some(ModelDownloadView {
                    bytes,
                    total,
                    state: status,
                });
                self.update_model_state(status, bytes, total, None);
                None
            }
            ModelState::Missing => {
                self.download = None;
                self.update_model_state(status, bytes, total, None);
                None
            }
            ModelState::Ready => {
                self.download = None;
                self.clear_error(AiErrorKind::Model);
                self.update_model_state(status, bytes, total, None);
                None
            }
            ModelState::Error => {
                self.download = None;
                let error = error.unwrap_or("model_download_failed").to_string();
                self.update_model_state(status, bytes, total, Some(&error));
                let info = AiErrorInfo::new(AiErrorKind::Model, error.clone(), None);
                if self.set_error(info) {
                    Some(error)
                } else {
                    None
                }
            }
        }
    }

    fn update_model_state(
        &mut self,
        status: ModelState,
        bytes: u64,
        total: Option<u64>,
        error: Option<&str>,
    ) {
        if let Some(state) = &mut self.state {
            state.model_state = status;
            state.model_bytes_downloaded = i64::try_from(bytes).unwrap_or(i64::MAX);
            state.model_bytes_total = total.map(|total| i64::try_from(total).unwrap_or(i64::MAX));
            state.model_error = error.map(str::to_string);
        }
    }
}

/// The card state derived from the plugin installation, model, and probe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AiPluginCardState {
    #[default]
    NotInstalled,
    Installing,
    ModelMissing,
    Downloading,
    Verifying,
    Ready,
    NeedsProbe,
    Error,
}

/// Pure state machine for the settings plugin card. A ready model without a
/// stored probe is not ready for tagging, so it gets its own state instead of
/// claiming success.
pub(crate) fn ai_plugin_card_state(ai: &AiUiState) -> AiPluginCardState {
    if ai.install_busy {
        return AiPluginCardState::Installing;
    }
    if ai.active.is_none() {
        return AiPluginCardState::NotInstalled;
    }
    if let Some(download) = &ai.download {
        return if download.state == ModelState::Verifying {
            AiPluginCardState::Verifying
        } else {
            AiPluginCardState::Downloading
        };
    }
    match ai.state.as_ref().map(|state| state.model_state) {
        Some(ModelState::Downloading) => AiPluginCardState::Downloading,
        Some(ModelState::Verifying) => AiPluginCardState::Verifying,
        Some(ModelState::Ready) if ai.ready() => AiPluginCardState::Ready,
        Some(ModelState::Ready) => AiPluginCardState::NeedsProbe,
        Some(ModelState::Error) => AiPluginCardState::Error,
        Some(ModelState::Missing) | None => AiPluginCardState::ModelMissing,
    }
}

/// The dashboard card's visible state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AiDashboardState {
    NotReady,
    Idle,
    Active,
    Failed,
}

/// Presentation model for the dashboard AI card.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AiDashboardView {
    pub(crate) visible: bool,
    pub(crate) state: AiDashboardState,
    pub(crate) jobs: Vec<TaggingJob>,
    pub(crate) more: usize,
}

/// Builds the dashboard view. The card is hidden for an uninstalled plugin and
/// for a ready plugin that the user has not enabled; a not-ready plugin stays
/// visible so the card can point at setup.
pub(crate) fn ai_dashboard_view(ai: &AiUiState, enabled: bool) -> AiDashboardView {
    let installed = ai.active.is_some() || !ai.installations.is_empty();
    let ready = ai.ready();
    let visible = installed && (!ready || enabled);
    if !visible {
        return AiDashboardView {
            visible: false,
            state: AiDashboardState::NotReady,
            jobs: Vec::new(),
            more: 0,
        };
    }

    let state = if !ready {
        AiDashboardState::NotReady
    } else if ai.job_counts.failed > 0 {
        AiDashboardState::Failed
    } else if ai.job_counts.waiting + ai.job_counts.pending + ai.job_counts.processing > 0 {
        AiDashboardState::Active
    } else {
        AiDashboardState::Idle
    };

    let mut jobs: Vec<TaggingJob> = ai
        .active_jobs
        .iter()
        .filter(|job| {
            matches!(
                job.status,
                TaggingJobStatus::Processing
                    | TaggingJobStatus::Pending
                    | TaggingJobStatus::Waiting
            )
        })
        .cloned()
        .collect();
    jobs.sort_by_key(|job| match job.status {
        TaggingJobStatus::Processing => 0,
        TaggingJobStatus::Pending => 1,
        TaggingJobStatus::Waiting => 2,
        TaggingJobStatus::Failed | TaggingJobStatus::Completed | TaggingJobStatus::Cancelled => 3,
    });
    let active_total =
        (ai.job_counts.waiting + ai.job_counts.pending + ai.job_counts.processing).max(0) as usize;
    let displayed = jobs.len().min(3);
    let more = active_total.saturating_sub(displayed);
    jobs.truncate(3);

    AiDashboardView {
        visible,
        state,
        jobs,
        more,
    }
}

/// The primary action a plugin card offers for the current state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AiPluginAction {
    #[default]
    InstallZip,
    DownloadModel,
    ImportModel,
    Recheck,
    DeleteModel,
    Uninstall,
    None,
}

/// Pure presentation model for the settings plugin card. Keeping the state and
/// primary action here makes the state machine testable without a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AiPluginCardView {
    pub(crate) state: AiPluginCardState,
    pub(crate) primary_action: AiPluginAction,
}

impl From<&AiUiState> for AiPluginCardView {
    fn from(ai: &AiUiState) -> Self {
        let state = ai_plugin_card_state(ai);
        let primary_action = match state {
            AiPluginCardState::Installing => AiPluginAction::None,
            AiPluginCardState::NotInstalled => AiPluginAction::InstallZip,
            AiPluginCardState::ModelMissing | AiPluginCardState::Error => {
                AiPluginAction::DownloadModel
            }
            AiPluginCardState::Downloading | AiPluginCardState::Verifying => AiPluginAction::None,
            AiPluginCardState::Ready | AiPluginCardState::NeedsProbe => AiPluginAction::Recheck,
        };
        Self {
            state,
            primary_action,
        }
    }
}

/// Whether an installation row is the active package. The version is part of
/// the row identity, so two versions of one variant cannot both look active.
pub(crate) fn is_active_installation(install: &PluginInstallation) -> bool {
    install.active
}

/// Stable element id for a variant row. Includes the version because a variant
/// can have several installed versions at once.
pub(crate) fn installation_row_id(install: &PluginInstallation) -> String {
    format!("ai-variant-{}-{}", install.variant, install.version)
}

/// License and model metadata read from an installed package manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AiManifestView {
    pub(crate) license: Option<String>,
    pub(crate) license_url: Option<String>,
    pub(crate) license_file: Option<String>,
    pub(crate) model_name: Option<String>,
    pub(crate) model_size: Option<u64>,
    pub(crate) revision: Option<String>,
}

fn manifest_text(value: Option<&serde_json::Value>, key: &str) -> Option<String> {
    value
        .and_then(|value| value.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty())
}

/// Reads the neutral license and model details the settings card displays.
/// Malformed or older manifests degrade to empty values instead of hiding the
/// card.
pub(crate) fn ai_manifest_view(manifest_json: &str) -> AiManifestView {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(manifest_json) else {
        return AiManifestView::default();
    };
    let model = value.get("model");
    let model_size = model
        .and_then(|model| model.get("files"))
        .and_then(serde_json::Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|file| file.get("size").and_then(serde_json::Value::as_u64))
                .sum::<u64>()
        })
        .filter(|size| *size > 0);
    let license_file = value
        .get("files")
        .and_then(serde_json::Value::as_array)
        .and_then(|files| {
            files
                .iter()
                .filter_map(|file| file.get("path").and_then(serde_json::Value::as_str))
                .find(|path| path.to_ascii_lowercase().contains("license"))
                .map(str::to_string)
        });
    AiManifestView {
        license: manifest_text(model, "license"),
        license_url: manifest_text(model, "license_url"),
        license_file,
        model_name: match (
            manifest_text(model, "id"),
            manifest_text(model, "quantization"),
        ) {
            (Some(id), Some(quantization)) => Some(format!("{id} {quantization}")),
            (Some(id), None) => Some(id),
            (None, Some(quantization)) => Some(quantization),
            (None, None) => None,
        },
        model_size,
        revision: manifest_text(model, "revision"),
    }
}

/// Display name for the backend encoded in a platform variant such as
/// `linux-vulkan`.
pub(crate) fn variant_backend_label(variant: &str) -> String {
    let kind = variant
        .rsplit_once('-')
        .map(|(_, backend)| backend)
        .unwrap_or(variant);
    match kind.to_ascii_lowercase().as_str() {
        "vulkan" => "Vulkan".to_string(),
        "cpu" => "CPU".to_string(),
        "cuda" => "CUDA".to_string(),
        "roc" | "rocm" | "hip" => "ROCm".to_string(),
        "" => "—".to_string(),
        _ => kind.to_string(),
    }
}

/// Catalog name for the active language, falling back to the Japanese name
/// when an English name is not available.
pub(crate) fn ai_tag_display_name(definition: &AiTagDefinition, language: &str) -> String {
    if language.starts_with("en") {
        definition
            .name_en
            .clone()
            .unwrap_or_else(|| definition.name.clone())
    } else {
        definition.name.clone()
    }
}

fn adjust_job_count(counts: &mut JobStatusCounts, status: TaggingJobStatus, delta: i64) {
    let slot = match status {
        TaggingJobStatus::Waiting => &mut counts.waiting,
        TaggingJobStatus::Pending => &mut counts.pending,
        TaggingJobStatus::Processing => &mut counts.processing,
        TaggingJobStatus::Failed => &mut counts.failed,
        TaggingJobStatus::Completed | TaggingJobStatus::Cancelled => return,
    };
    *slot = (*slot + delta).max(0);
}

fn probe_from_state(state: &PluginState) -> Option<Probe> {
    let raw = state.probe_json.as_deref()?;
    match serde_json::from_str::<Probe>(raw) {
        Ok(probe) => Some(probe),
        Err(error) => {
            tracing::warn!(%error, "cannot parse the stored AI plugin probe");
            None
        }
    }
}

fn active_job_names(db: &Db, jobs: &[TaggingJob]) -> HashMap<i64, String> {
    let mut names = HashMap::new();
    for photo_id in jobs.iter().map(|job| job.photo_id) {
        if names.contains_key(&photo_id) {
            continue;
        }
        match db.photo_by_id(photo_id) {
            Ok(Some(photo)) => {
                names.insert(photo_id, photo.file_name);
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, photo_id, "cannot load a job photo name"),
        }
    }
    names
}

pub(crate) fn collect_ai_snapshot(db: &Db) -> AiSnapshot {
    let state = db.plugin_state(AI_PLUGIN_ID).unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot load AI plugin state");
        None
    });
    let active_jobs = db.active_tagging_jobs_for_ui(20).unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot load active AI tagging jobs");
        Vec::new()
    });
    let active_job_names = active_job_names(db, &active_jobs);
    AiSnapshot {
        installations: db
            .list_plugin_installations(AI_PLUGIN_ID)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot list AI plugin installations");
                Vec::new()
            }),
        active: db
            .active_plugin_installation(AI_PLUGIN_ID)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot load the active AI plugin installation");
                None
            }),
        probe: state.as_ref().and_then(probe_from_state),
        state,
        tag_definitions: db.ai_tag_definitions(AI_PLUGIN_ID).unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot load AI tag definitions");
            Vec::new()
        }),
        job_counts: db.tagging_job_counts(AI_PLUGIN_ID).unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot count AI tagging jobs");
            JobStatusCounts::default()
        }),
        active_jobs,
        active_job_names,
    }
}

pub(crate) fn collect_ai_live_snapshot(db: &Db) -> AiLiveSnapshot {
    let state = db.plugin_state(AI_PLUGIN_ID).unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot load AI plugin state");
        None
    });
    let active_jobs = db.active_tagging_jobs_for_ui(20).unwrap_or_default();
    let active_job_names = active_job_names(db, &active_jobs);
    AiLiveSnapshot {
        probe: state.as_ref().and_then(probe_from_state),
        state,
        job_counts: db.tagging_job_counts(AI_PLUGIN_ID).unwrap_or_default(),
        active_jobs,
        active_job_names,
    }
}

/// Fraction of the model download completed. A missing or zero total has no
/// meaningful progress and renders as an empty bar.
pub(crate) fn model_progress_ratio(bytes: u64, total: Option<u64>) -> f32 {
    match total {
        Some(total) if total > 0 => (bytes as f32 / total as f32).clamp(0.0, 1.0),
        _ => 0.0,
    }
}

fn normalize_backend(kind: &str) -> String {
    let kind = kind.trim().to_ascii_lowercase();
    if matches!(kind.as_str(), "rocm" | "hip") {
        "roc".to_string()
    } else {
        kind
    }
}

/// Backend choices for the runtime card: `auto` plus the kinds the probe
/// advertises. A saved value the current build cannot run selects `auto`.
pub(crate) fn backend_options(probe: &Probe, selected: &str) -> Vec<(String, bool)> {
    let mut ids: Vec<String> = Vec::new();
    for backend in &probe.backends {
        let kind = normalize_backend(&backend.kind);
        if kind.is_empty() || ids.iter().any(|id| id == &kind) {
            continue;
        }
        ids.push(kind);
    }
    let selected_known = ids.iter().any(|id| id == selected);
    let mut options = vec![("auto".to_string(), !selected_known)];
    for id in ids {
        let active = id == selected;
        options.push((id, active));
    }
    options
}

/// Ratio for a tagging job progress bar. The stored progress is a fraction in
/// `0..=1`; non-finite and out-of-range values are clamped so a bad sidecar
/// event cannot overdraw the bar.
pub(crate) fn download_progress_bar_ratio(progress: f64) -> f32 {
    if !progress.is_finite() {
        return 0.0;
    }
    progress.clamp(0.0, 1.0) as f32
}

/// File name for a runtime job row, with the translated photo id as a
/// fallback when the row disappears from the catalog between snapshots.
pub(crate) fn active_job_photo_label(
    file_name: Option<&str>,
    photo_id: i64,
    language: &str,
) -> String {
    match file_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_string(),
        None => format!(
            "{} #{}",
            vrc_photon_i18n::tr_in(language, "integrations.ai.jobs.photo"),
            photo_id
        ),
    }
}

/// Backend chips are selectable only once the plugin is ready and AI tagging
/// is enabled, matching the mockup's "有効化後に選択" state.
pub(crate) fn backend_selection_is_disabled_when_not_ready(ready: bool, enabled: bool) -> bool {
    !ready || !enabled
}

/// Human-readable people count from the sidecar interval, formatted for `language`.
pub(crate) fn people_label_in(language: &str, min: Option<u32>, max: Option<u32>) -> String {
    match (min, max) {
        (Some(0), Some(0)) => vrc_photon_i18n::tr_in(language, "photos.ai.people.zero"),
        (Some(1), Some(1)) => vrc_photon_i18n::tr_in(language, "photos.ai.people.one"),
        (Some(min), Some(max)) if min == max => vrc_photon_i18n::tr_args_in(
            language,
            "photos.ai.people.exact",
            &[("count", min.to_string())],
        ),
        (Some(min), Some(max)) => vrc_photon_i18n::tr_args_in(
            language,
            "photos.ai.people.range",
            &[("min", min.to_string()), ("max", max.to_string())],
        ),
        (Some(min), None) => vrc_photon_i18n::tr_args_in(
            language,
            "photos.ai.people.at_least",
            &[("min", min.to_string())],
        ),
        (None, Some(1)) => vrc_photon_i18n::tr_in(language, "photos.ai.people.at_most_one"),
        (None, Some(max)) => vrc_photon_i18n::tr_args_in(
            language,
            "photos.ai.people.at_most",
            &[("max", max.to_string())],
        ),
        (None, None) => vrc_photon_i18n::tr_in(language, "photos.ai.people.unknown"),
    }
}

/// Human-readable people count for the active locale. Kept for the label
/// tests; the inspector always passes an explicit language.
#[cfg(test)]
pub(crate) fn people_label(min: Option<u32>, max: Option<u32>) -> String {
    people_label_in(&vrc_photon_i18n::current_locale(), min, max)
}

/// Human-readable place branch from the classifier, translated for the active
/// locale.
pub(crate) fn place_label(branch: Option<&str>) -> String {
    match branch {
        Some(branch) => match branch.trim().to_ascii_lowercase().as_str() {
            "indoor" => tr!("photos.ai.place.indoor"),
            "outdoor" => tr!("photos.ai.place.outdoor"),
            _ => tr!("photos.ai.place.other"),
        },
        None => "—".to_string(),
    }
}

/// Place label with the classifier's confidence for that branch, when the
/// stored probability JSON contains it.
pub(crate) fn place_label_with_confidence(
    branch: Option<&str>,
    probabilities_json: Option<&str>,
) -> String {
    let label = place_label(branch);
    let Some(branch) = branch else {
        return label;
    };
    let Some(probability) = place_probability(branch, probabilities_json) else {
        return label;
    };
    format!("{label} {:.1}%", probability * 100.0)
}

fn place_probability(branch: &str, probabilities_json: Option<&str>) -> Option<f32> {
    let key = branch.trim().to_ascii_lowercase();
    let value: serde_json::Value = serde_json::from_str(probabilities_json?).ok()?;
    let probability = value.get(key.as_str())?.as_f64()? as f32;
    probability.is_finite().then(|| probability.clamp(0.0, 1.0))
}

/// Formats the stored RFC3339 tagging timestamp for the inspector in the
/// user's local timezone. A value the catalog cannot parse is shown unchanged
/// rather than dropped.
pub(crate) fn format_tagged_at(tagged_at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(tagged_at)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| tagged_at.to_string())
}

/// Title and hint for the AI tag row when no applied tags are shown. A stored
/// result with zero applied tags is different from never having tagged the
/// photo.
pub(crate) fn ai_empty_state_labels(has_ai: bool) -> (String, String) {
    if has_ai {
        (
            tr!("photos.ai.none_applied"),
            tr!("photos.ai.none_applied_hint"),
        )
    } else {
        (tr!("photos.ai.empty"), tr!("photos.ai.empty_hint"))
    }
}

/// The presentation of one AI tag chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiTagView {
    pub(crate) label: String,
    pub(crate) tooltip: String,
    pub(crate) probability_label: String,
}

/// Chooses the catalog name for the active language and formats the
/// probability tooltip. AI tags are read-only history.
/// Test-only the UI always calls `ai_tag_view_with_context`.
#[cfg(test)]
pub(crate) fn ai_tag_view(tag: &PhotoAiTagView, language: &str) -> AiTagView {
    ai_tag_view_with_context(tag, language, None, None)
}

/// Like [`ai_tag_view`], but includes the producing model and the formatted
/// tagging time in the tooltip when both are available.
pub(crate) fn ai_tag_view_with_context(
    tag: &PhotoAiTagView,
    language: &str,
    model: Option<&str>,
    time: Option<&str>,
) -> AiTagView {
    let label = if language.starts_with("en") {
        tag.name_en.clone().unwrap_or_else(|| tag.name.clone())
    } else {
        tag.name.clone()
    };
    let probability = format!("{:.3}", tag.probability);
    let tooltip = match (model, time) {
        (Some(model), Some(time)) => vrc_photon_i18n::tr_args_in(
            language,
            "photos.ai.probability_tooltip",
            &[
                ("probability", probability),
                ("model", model.to_string()),
                ("time", time.to_string()),
            ],
        ),
        _ => vrc_photon_i18n::tr_args_in(
            language,
            "photos.ai.probability",
            &[("probability", probability)],
        ),
    };
    AiTagView {
        label,
        tooltip,
        probability_label: format!("{:.1}%", tag.probability * 100.0),
    }
}

/// Formats every stored tag for the active language. Test-only helper.
#[cfg(test)]
pub(crate) fn ai_tag_views(tags: &[PhotoAiTagView], language: &str) -> Vec<AiTagView> {
    tags.iter().map(|tag| ai_tag_view(tag, language)).collect()
}

/// Formats every stored tag with the model and tagging time in its tooltip.
pub(crate) fn ai_tag_views_with_context(
    tags: &[PhotoAiTagView],
    language: &str,
    model: Option<&str>,
    time: Option<&str>,
) -> Vec<AiTagView> {
    tags.iter()
        .map(|tag| ai_tag_view_with_context(tag, language, model, time))
        .collect()
}

/// Loads the stored AI result and the applied tag chips for one photo.
pub(crate) fn load_photo_ai(db: &Db, photo_id: i64) -> (Option<PhotoAi>, Vec<PhotoAiTagView>) {
    let ai = db.photo_ai(photo_id).unwrap_or_else(|error| {
        tracing::warn!(%error, photo_id, "cannot load photo AI metadata");
        None
    });
    let tags = db
        .photo_ai_tags_with_names(photo_id)
        .unwrap_or_else(|error| {
            tracing::warn!(%error, photo_id, "cannot load photo AI tags");
            Vec::new()
        })
        .into_iter()
        .filter(|tag| tag.applied)
        .collect();
    (ai, tags)
}

/// Why a manual retag did or did not enqueue jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManualRetagOutcome {
    Disabled,
    NotReady,
    Queued(usize),
}

impl ManualRetagOutcome {
    /// True when the daemon's tagging worker should be woken. An empty enqueue
    /// means every photo already has an active job, so there is nothing to run.
    pub(crate) fn wakes(self) -> bool {
        matches!(self, Self::Queued(count) if count > 0)
    }
}

/// Validates the AI tagging switch and readiness before enqueueing pending
/// retag jobs. Keeping the decision pure lets the guards be tested without a
/// window.
pub(crate) fn enqueue_manual_retag(
    db: &Db,
    enabled: bool,
    ready: bool,
    photo_ids: &[i64],
) -> vrc_photon_core::error::Result<ManualRetagOutcome> {
    if !enabled {
        return Ok(ManualRetagOutcome::Disabled);
    }
    if !ready {
        return Ok(ManualRetagOutcome::NotReady);
    }
    db.enqueue_tagging_jobs(photo_ids, AI_PLUGIN_ID, TaggingJobStatus::Pending)
        .map(ManualRetagOutcome::Queued)
}

/// Whether the license modal must be shown before starting a model download.
/// Consent is persisted, so it is only ever requested once.
pub(crate) fn needs_license_consent(consent_at: Option<i64>) -> bool {
    consent_at.is_none()
}

/// Translated label for a daemon or tagging error code. Unknown codes fall
/// back to the raw code instead of the missing-key placeholder.
pub(crate) fn ai_error_label_in(language: &str, code: &str) -> String {
    let key = format!("tagging.error.{code}");
    let label = vrc_photon_i18n::tr_in(language, &key);
    if label == MISSING {
        code.to_string()
    } else {
        label
    }
}

/// Generic translated summary for an AI failure. The specific code label and
/// the daemon's raw detail are shown as secondary lines so a raw English
/// message never becomes the headline.
pub(crate) fn ai_error_summary_in(language: &str, error: &AiErrorInfo) -> String {
    let key = match error.kind {
        AiErrorKind::Install => "settings.ai.install_failed",
        AiErrorKind::Model => "settings.ai.model_failed",
        AiErrorKind::Job => "integrations.ai.jobs.failure_generic",
        AiErrorKind::Operation => "settings.ai.operation_failed",
    };
    vrc_photon_i18n::tr_in(language, key)
}

/// Translated code label plus the daemon's raw detail, when they add anything
/// beyond the code itself.
pub(crate) fn ai_error_detail_in(language: &str, error: &AiErrorInfo) -> Option<String> {
    let label = ai_error_label_in(language, &error.code);
    let known = label != error.code;
    let detail = error
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty() && *detail != error.code);
    match (known, detail) {
        (true, Some(detail)) if detail != label => Some(format!("{label}: {detail}")),
        (true, _) => Some(label),
        (false, Some(detail)) => Some(detail.to_string()),
        (false, None) => (!error.code.is_empty()).then(|| error.code.clone()),
    }
}

/// One toast line: translated summary, then the best available detail.
pub(crate) fn ai_error_toast_in(language: &str, error: &AiErrorInfo) -> String {
    let summary = ai_error_summary_in(language, error);
    match ai_error_detail_in(language, error) {
        Some(detail) => format!("{summary}: {detail}"),
        None => summary,
    }
}

/// Failures worth a toast even when they arrive through a job progress event.
/// Image-by-image failures stay in the cards; sidecar and resource failures
/// need the user's attention.
pub(crate) fn ai_job_error_is_notifiable(code: &str) -> bool {
    matches!(
        code,
        "sidecar_exited"
            | "sidecar_timeout"
            | "sidecar_protocol"
            | "protocol_error"
            | "model_load_failed"
            | "backend_unavailable"
            | "out_of_memory"
    )
}

/// A successful model download is announced once when it transitions to Ready.
pub(crate) fn model_ready_toast_pending(previous_download: bool, status: ModelState) -> bool {
    previous_download && status == ModelState::Ready
}

/// The terminal state event after a Recheck/variant switch carries no probe.
/// That is a failed plugin operation, not an install failure.
pub(crate) fn probe_failure_info() -> AiErrorInfo {
    AiErrorInfo::new(AiErrorKind::Operation, "plugin_probe_failed", None)
}

fn request_error_kind(request: &IpcRequest) -> AiErrorKind {
    match request {
        IpcRequest::InstallPlugin { .. } => AiErrorKind::Install,
        IpcRequest::StartModelDownload { .. }
        | IpcRequest::CancelModelDownload { .. }
        | IpcRequest::ImportModelFiles { .. }
        | IpcRequest::DeleteModel { .. } => AiErrorKind::Model,
        IpcRequest::ProbePlugin { .. }
        | IpcRequest::SetActiveVariant { .. }
        | IpcRequest::UninstallPlugin { .. } => AiErrorKind::Operation,
        _ => AiErrorKind::Job,
    }
}

/// Error codes are stable snake_case identifiers. Raw daemon messages often
/// contain spaces or prose, so they must not be repeated in the error-code row.
pub(crate) fn ai_error_code_is_stable(code: &str) -> bool {
    !code.is_empty()
        && code
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn error_code_str(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidRequest => "invalid_request",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Busy => "busy",
        ErrorCode::NotReady => "not_ready",
        ErrorCode::ConsentRequired => "consent_required",
    }
}

/// The outcome of an AI IPC request. Unlike the older `send_ipc_request`,
/// `Response::Error` is a failure here: the daemon answers validation errors
/// that way instead of closing the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AiResponseOutcome {
    Ok,
    Error { code: &'static str, message: String },
}

pub(crate) fn ai_response_outcome(response: &IpcResponse) -> AiResponseOutcome {
    match response {
        IpcResponse::Error { code, message } => AiResponseOutcome::Error {
            code: error_code_str(*code),
            message: message.clone(),
        },
        _ => AiResponseOutcome::Ok,
    }
}

/// Whether an install event belongs to the AI plugin. Async install failures
/// are published before the ZIP yields a plugin id, so the daemon reports them
/// with an empty id; the UI installs only the AI plugin, so those failures
/// belong to this flow. Events for other plugin ids stay ignored.
pub(crate) fn install_event_applies_to_ai(plugin_id: &str, status: PluginInstallStatus) -> bool {
    plugin_id == AI_PLUGIN_ID || (status == PluginInstallStatus::Failed && plugin_id.is_empty())
}

/// Applies an install event that belongs to the AI plugin.
pub(crate) fn apply_install_event(
    ai: &mut AiUiState,
    plugin_id: &str,
    status: PluginInstallStatus,
    error: Option<&str>,
) -> Option<String> {
    if !install_event_applies_to_ai(plugin_id, status) {
        // The UI only starts AI installs, but a daemon-side event for another
        // plugin must not leave the card on "Installing…" forever. A terminal
        // event ends whatever install flow is active; its error still belongs
        // to the other plugin and is not reported as an AI failure.
        if ai.install_busy
            && matches!(
                status,
                PluginInstallStatus::Installed | PluginInstallStatus::Failed
            )
        {
            ai.install_busy = false;
        }
        return None;
    }
    ai.apply_install_progress(status, error)
}

/// Library refresh chosen for one tagging event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaggingLiveRefresh {
    /// Progress only; the debounced AI-card read is enough.
    None,
    /// Terminal event: schedule the coalesced background live snapshot.
    Background,
}

impl App {
    pub(crate) fn refresh_ai_state(&mut self, cx: &mut Context<Self>) {
        let snapshot = collect_ai_snapshot(&self.db);
        self.ai.apply_snapshot(snapshot);
        cx.notify();
    }

    /// Sends an AI request and treats `Response::Error` as a failed operation.
    pub(crate) fn send_ai_request(
        &mut self,
        request: IpcRequest,
        success: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let clears_install_busy = matches!(request, IpcRequest::InstallPlugin { .. });
        let clears_probe_pending = matches!(
            request,
            IpcRequest::ProbePlugin { .. } | IpcRequest::SetActiveVariant { .. }
        );
        let error_kind = request_error_kind(&request);
        let refresh_selected = matches!(
            request,
            IpcRequest::UninstallPlugin {
                delete_ai_tags: true,
                ..
            }
        );
        if !self.ipc_connected {
            if clears_install_busy {
                self.ai.install_busy = false;
            }
            if clears_probe_pending {
                self.ai.probe_pending = false;
            }
            self.toast_error(tr!("daemon.ipc.disconnected"), cx);
            cx.notify();
            return;
        }
        let Some(endpoint) = self.ipc_endpoint.clone() else {
            if clears_install_busy {
                self.ai.install_busy = false;
            }
            if clears_probe_pending {
                self.ai.probe_pending = false;
            }
            self.toast_error(tr!("daemon.ipc.resolve_error"), cx);
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ipc_bridge::request(endpoint, request) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(response) => match ai_response_outcome(&response) {
                        AiResponseOutcome::Ok => {
                            if let Some(message) = success {
                                this.toast_success(message, cx);
                            }
                            if refresh_selected {
                                // Deleting AI data removes the inspector's
                                // result too; reload it so stale tags cannot
                                // survive the uninstall.
                                this.refresh_selected_ai(cx);
                            }
                        }
                        AiResponseOutcome::Error { code, message } => {
                            if clears_install_busy {
                                this.ai.install_busy = false;
                            }
                            if clears_probe_pending {
                                this.ai.probe_pending = false;
                            }
                            let info = AiErrorInfo::new(error_kind, code, Some(message.clone()));
                            this.ai.set_error(info.clone());
                            if code == "consent_required" {
                                this.ai.license_modal_open = true;
                                this.ai_modal_focus_pending = true;
                            }
                            this.toast_error(
                                ai_error_toast_in(&vrc_photon_i18n::current_locale(), &info),
                                cx,
                            );
                        }
                    },
                    Err(error) => {
                        if clears_install_busy {
                            this.ai.install_busy = false;
                        }
                        if clears_probe_pending {
                            this.ai.probe_pending = false;
                        }
                        let detail = ipc_bridge::error_message(&error);
                        let info = AiErrorInfo::new(error_kind, "internal", Some(detail));
                        this.ai.set_error(info.clone());
                        this.toast_error(
                            ai_error_toast_in(&vrc_photon_i18n::current_locale(), &info),
                            cx,
                        );
                    }
                }
                this.refresh_ai_state(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn install_ai_plugin(&mut self, window: &Window, cx: &mut Context<Self>) {
        let options = DialogOptions {
            filters: vec![("ZIP".to_string(), vec!["zip".to_string()])],
            ..DialogOptions::default()
        };
        let dialog = options.dialog(Some(window));
        cx.spawn(async move |this, cx| {
            let Some(path) = file_picker::pick_file(dialog).await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.ai.install_busy = true;
                this.ai.clear_error(AiErrorKind::Install);
                this.send_ai_request(
                    IpcRequest::InstallPlugin {
                        zip_path: path.to_string_lossy().into_owned(),
                    },
                    None,
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn import_ai_model_files(&mut self, window: &Window, cx: &mut Context<Self>) {
        let options = DialogOptions {
            filters: vec![("Model".to_string(), vec!["gguf".to_string()])],
            ..DialogOptions::default()
        };
        let dialog = options.dialog(Some(window));
        cx.spawn(async move |this, cx| {
            let Some(paths) = file_picker::pick_files(dialog).await else {
                return;
            };
            if paths.is_empty() {
                return;
            }
            this.update(cx, |this, cx| {
                this.ai.clear_error(AiErrorKind::Model);
                let paths = paths
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                this.send_ai_request(
                    IpcRequest::ImportModelFiles {
                        plugin_id: AI_PLUGIN_ID.to_string(),
                        paths,
                    },
                    None,
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    /// Starts a model download. The first request opens the license modal;
    /// the persisted consent makes later downloads start immediately.
    pub(crate) fn request_ai_model_download(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ai.active.is_none() {
            self.toast_warning(tr!("integrations.ai.not_ready"), cx);
            cx.notify();
            return;
        }
        if needs_license_consent(self.settings.ai_model_license_consent_at) {
            self.open_ai_license_modal(window, cx);
            return;
        }
        self.start_ai_model_download(cx);
    }

    pub(crate) fn open_ai_license_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai.license_modal_open {
            return;
        }
        self.ai_modal_return_focus = window.focused(cx);
        self.ai.license_modal_open = true;
        self.ai_modal_focus_pending = true;
        cx.notify();
    }

    pub(crate) fn accept_ai_model_license(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.settings.ai_model_license_consent_at;
        self.settings.ai_model_license_consent_at = Some(chrono::Utc::now().timestamp());
        if let Err(error) = self.settings.save(&self.db) {
            self.settings.ai_model_license_consent_at = previous;
            self.toast_error(tr_args!("settings.save_failed", error = error), cx);
            cx.notify();
            return;
        }
        self.close_ai_license_modal(window, cx);
        self.start_ai_model_download(cx);
    }

    pub(crate) fn close_ai_license_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ai.license_modal_open {
            return;
        }
        self.ai.license_modal_open = false;
        self.ai_modal_focus_pending = false;
        self.restore_ai_modal_focus(window, cx);
        cx.notify();
    }

    fn restore_ai_modal_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self
            .ai_modal_return_focus
            .take()
            .unwrap_or_else(|| self.photo_grid_focus.clone());
        window.focus(&focus, cx);
    }

    pub(crate) fn open_ai_uninstall_modal(
        &mut self,
        target: UninstallTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ai.uninstall_modal_open {
            return;
        }
        self.ai_modal_return_focus = window.focused(cx);
        self.ai.uninstall_target = Some(target);
        self.ai.uninstall_delete_tags = false;
        self.ai.uninstall_modal_open = true;
        self.ai_modal_focus_pending = true;
        cx.notify();
    }

    pub(crate) fn close_ai_uninstall_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ai.uninstall_modal_open {
            return;
        }
        self.ai.uninstall_modal_open = false;
        self.ai.uninstall_target = None;
        self.ai.uninstall_delete_tags = false;
        self.ai_modal_focus_pending = false;
        self.restore_ai_modal_focus(window, cx);
        cx.notify();
    }

    fn start_ai_model_download(&mut self, cx: &mut Context<Self>) {
        self.send_ai_request(
            IpcRequest::StartModelDownload {
                plugin_id: AI_PLUGIN_ID.to_string(),
            },
            None,
            cx,
        );
    }

    pub(crate) fn cancel_ai_model_download(&mut self, cx: &mut Context<Self>) {
        self.send_ai_request(
            IpcRequest::CancelModelDownload {
                plugin_id: AI_PLUGIN_ID.to_string(),
            },
            None,
            cx,
        );
    }

    pub(crate) fn probe_ai_plugin(&mut self, cx: &mut Context<Self>) {
        self.ai.begin_probe();
        self.send_ai_request(
            IpcRequest::ProbePlugin {
                plugin_id: AI_PLUGIN_ID.to_string(),
            },
            None,
            cx,
        );
    }

    pub(crate) fn set_ai_active_variant(
        &mut self,
        variant: &str,
        version: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.ai.begin_probe();
        self.send_ai_request(
            IpcRequest::SetActiveVariant {
                plugin_id: AI_PLUGIN_ID.to_string(),
                variant: variant.to_string(),
                version: version.map(str::to_string),
            },
            None,
            cx,
        );
    }

    pub(crate) fn delete_ai_model(&mut self, cx: &mut Context<Self>) {
        self.send_ai_request(
            IpcRequest::DeleteModel {
                plugin_id: AI_PLUGIN_ID.to_string(),
            },
            None,
            cx,
        );
    }

    pub(crate) fn uninstall_ai_plugin(
        &mut self,
        target: &UninstallTarget,
        delete_ai_tags: bool,
        cx: &mut Context<Self>,
    ) {
        let (variant, version, delete_ai_tags) = uninstall_request_args(target, delete_ai_tags);
        self.send_ai_request(
            IpcRequest::UninstallPlugin {
                plugin_id: AI_PLUGIN_ID.to_string(),
                variant,
                version,
                delete_ai_tags,
            },
            None,
            cx,
        );
    }

    pub(crate) fn save_ai_runtime_settings(
        &mut self,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut AppSettings),
    ) {
        let previous = self.settings.clone();
        apply(&mut self.settings);
        if let Err(error) = self.settings.save(&self.db) {
            self.settings = previous;
            tracing::warn!(%error, "cannot save AI tagging settings");
            self.toast_error(tr_args!("settings.save_failed", error = error), cx);
            cx.notify();
            return;
        }

        let backlog_started = !previous.ai_tagging_backlog && self.settings.ai_tagging_backlog;
        if backlog_started {
            match self.db.enqueue_tagging_backlog(AI_PLUGIN_ID) {
                Ok(inserted) => {
                    if inserted > 0 {
                        self.toast_info(
                            tr_args!("integrations.ai.backlog.started", count = inserted),
                            cx,
                        );
                    }
                }
                Err(error) => {
                    self.toast_error(tr_args!("integrations.ai.backlog.error", error = error), cx)
                }
            }
            self.wake_daemon(IpcRequest::WakeTagging, cx);
        }
        self.refresh_ai_state(cx);
        cx.notify();
    }

    /// Enqueues ready tagging jobs for the given photos. The plugin must be
    /// enabled and have an active installation, a ready model, and a probe.
    pub(crate) fn enqueue_ai_tagging(&mut self, photo_ids: &[i64], cx: &mut Context<Self>) {
        match enqueue_manual_retag(
            &self.db,
            self.settings.ai_tagging_enabled,
            self.ai.ready(),
            photo_ids,
        ) {
            Ok(ManualRetagOutcome::Disabled) => {
                self.toast_warning(tr!("integrations.ai.disabled"), cx);
                cx.notify();
                return;
            }
            Ok(ManualRetagOutcome::NotReady) => {
                self.toast_warning(tr!("integrations.ai.not_ready"), cx);
                cx.notify();
                return;
            }
            Ok(outcome @ ManualRetagOutcome::Queued(count)) => {
                if count == 0 {
                    self.toast_info(tr!("integrations.ai.enqueue.none"), cx);
                } else {
                    self.toast_success(
                        tr_args!("integrations.ai.enqueue.queued", count = count),
                        cx,
                    );
                }
                if outcome.wakes() {
                    self.wake_daemon(IpcRequest::WakeTagging, cx);
                }
            }
            Err(error) => {
                self.toast_error(tr_args!("integrations.ai.enqueue.error", error = error), cx)
            }
        }
        self.refresh_ai_state(cx);
        cx.notify();
    }

    pub(crate) fn cancel_ai_job(&mut self, job_id: &str, cx: &mut Context<Self>) {
        self.send_ai_request(
            IpcRequest::CancelTagging {
                job_id: job_id.to_string(),
            },
            None,
            cx,
        );
    }

    /// Moves every waiting backlog job to the ready queue and wakes the
    /// daemon's tagging worker.
    pub(crate) fn start_ai_backlog(&mut self, cx: &mut Context<Self>) {
        match self.db.release_waiting_tagging_jobs(AI_PLUGIN_ID) {
            Ok(0) => self.toast_info(tr!("integrations.ai.backlog.empty"), cx),
            Ok(count) => {
                self.toast_success(
                    tr_args!("integrations.ai.backlog.started", count = count),
                    cx,
                );
                self.wake_daemon(IpcRequest::WakeTagging, cx);
            }
            Err(error) => {
                self.toast_error(tr_args!("integrations.ai.backlog.error", error = error), cx)
            }
        }
        self.refresh_ai_state(cx);
        cx.notify();
    }

    /// Which library refresh a tagging event requires. Terminal events can
    /// move the library revision (applied tags) and the job queue, so they
    /// schedule the coalesced background pass. Progress events stay on the
    /// debounced AI-card read.
    fn tagging_live_refresh(terminal: bool) -> TaggingLiveRefresh {
        if terminal {
            TaggingLiveRefresh::Background
        } else {
            TaggingLiveRefresh::None
        }
    }

    /// Handles the four AI daemon events. Returns true when the event belongs
    /// to AI tagging.
    pub(crate) fn handle_ai_event(&mut self, event: IpcEvent, cx: &mut Context<Self>) -> bool {
        match event {
            IpcEvent::TaggingProgress {
                job_id,
                photo_id,
                status,
                progress,
                stage,
                error,
            } => {
                let new_failure = self.ai.apply_tagging_progress(
                    &job_id,
                    photo_id,
                    status,
                    progress,
                    stage,
                    error.as_deref(),
                );
                if new_failure
                    && let Some(code) = error.as_deref()
                    && ai_job_error_is_notifiable(code)
                {
                    let info = AiErrorInfo::new(AiErrorKind::Job, code, None);
                    self.toast_error(
                        ai_error_toast_in(&vrc_photon_i18n::current_locale(), &info),
                        cx,
                    );
                }
                let terminal = matches!(
                    status,
                    TaggingJobStatus::Completed
                        | TaggingJobStatus::Failed
                        | TaggingJobStatus::Cancelled
                );
                // Progress events can arrive several times per photo. Coalesce
                // the database read and inspector refresh; a terminal job
                // schedules the background live snapshot instead of running
                // the ~20-query refresh on the UI thread.
                self.schedule_ai_refresh(terminal, cx);
                if Self::tagging_live_refresh(terminal) == TaggingLiveRefresh::Background {
                    self.refresh_live_async(cx);
                }
                true
            }
            IpcEvent::PluginInstallProgress {
                plugin_id,
                status,
                error,
                ..
            } => {
                let applies = install_event_applies_to_ai(&plugin_id, status);
                let was_busy = self.ai.install_busy;
                if let Some(code) =
                    apply_install_event(&mut self.ai, &plugin_id, status, error.as_deref())
                {
                    let info = AiErrorInfo::new(AiErrorKind::Install, code, None);
                    self.toast_error(
                        ai_error_toast_in(&vrc_photon_i18n::current_locale(), &info),
                        cx,
                    );
                }
                if applies || was_busy {
                    self.refresh_ai_state(cx);
                }
                applies || was_busy
            }
            IpcEvent::PluginStateChanged { plugin_id } => {
                if plugin_id != AI_PLUGIN_ID {
                    return false;
                }
                self.refresh_ai_state(cx);
                // Uninstall with delete-tags and a completed retag finish in
                // the worker after the request ack; the terminal state event is
                // the authoritative point to reload the inspector.
                self.refresh_selected_ai(cx);
                if self.ai.finish_probe() {
                    let info = probe_failure_info();
                    self.toast_error(
                        ai_error_toast_in(&vrc_photon_i18n::current_locale(), &info),
                        cx,
                    );
                }
                true
            }
            IpcEvent::ModelDownloadProgress {
                plugin_id,
                status,
                bytes,
                total,
                error,
            } => {
                if plugin_id != AI_PLUGIN_ID {
                    return false;
                }
                let was_downloading = self.ai.download.is_some()
                    || self.ai.state.as_ref().is_some_and(|state| {
                        matches!(
                            state.model_state,
                            ModelState::Downloading | ModelState::Verifying
                        )
                    });
                if let Some(code) =
                    self.ai
                        .apply_model_download(status, bytes, total, error.as_deref())
                {
                    let info = AiErrorInfo::new(AiErrorKind::Model, code, None);
                    self.toast_error(
                        ai_error_toast_in(&vrc_photon_i18n::current_locale(), &info),
                        cx,
                    );
                }
                if model_ready_toast_pending(was_downloading, status) {
                    self.toast_success(tr!("notifications.ai.model_ready"), cx);
                }
                self.refresh_ai_state(cx);
                true
            }
            _ => false,
        }
    }

    /// Coalesces an event burst into one state read. `selected` additionally
    /// refreshes the inspector's AI tags for the selected photo.
    fn schedule_ai_refresh(&mut self, selected: bool, cx: &mut Context<Self>) {
        self.ai_selected_refresh_pending |= selected;
        if self.ai_refresh_scheduled {
            return;
        }
        self.ai_refresh_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AI_REFRESH_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                this.ai_refresh_scheduled = false;
                this.refresh_ai_state(cx);
                if std::mem::take(&mut this.ai_selected_refresh_pending) {
                    this.refresh_selected_ai(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Reloads the selected photo's metadata after its AI result changed.
    /// `load_selected_meta` also loads the AI rows, so this stays the single
    /// refresh path for the inspector.
    pub(crate) fn refresh_selected_ai(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected {
            self.load_selected_meta(id);
            self.refresh_stale_preview(cx);
        }
        cx.notify();
    }

    /// Loads the selected photo's AI result and applied tag views.
    pub(crate) fn load_selected_ai(&mut self, photo_id: i64) {
        let (ai, tags) = load_photo_ai(&self.db, photo_id);
        self.selected_ai = ai;
        self.selected_ai_tags = tags;
        self.ai_tag_focused = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AI_PLUGIN_ID, AiDashboardState, AiErrorInfo, AiErrorKind, AiPluginAction,
        AiPluginCardState, AiPluginCardView, AiResponseOutcome, AiUiState, ManualRetagOutcome,
        ModelDownloadView, TaggingLiveRefresh, UninstallTarget, active_job_photo_label,
        ai_dashboard_view, ai_empty_state_labels, ai_error_code_is_stable, ai_error_detail_in,
        ai_error_label_in, ai_error_summary_in, ai_error_toast_in, ai_job_error_is_notifiable,
        ai_manifest_view, ai_plugin_card_state, ai_response_outcome, ai_tag_view,
        ai_tag_view_with_context, ai_tag_views, apply_install_event, backend_options,
        backend_selection_is_disabled_when_not_ready, collect_ai_snapshot,
        download_progress_bar_ratio, enqueue_manual_retag, format_tagged_at,
        install_event_applies_to_ai, installation_row_id, is_active_installation, load_photo_ai,
        model_progress_ratio, model_ready_toast_pending, needs_license_consent, people_label,
        people_label_in, place_label, place_label_with_confidence, probe_failure_info,
        request_error_kind, uninstall_request_args, variant_backend_label,
    };
    use vrc_photon_ai_protocol::{
        BackendInfo, Catalog, ModelState, PluginInstallStatus, Probe, TagDefinition,
        TaggingJobStatus,
    };
    use vrc_photon_core::ai::{
        NewPhotoAi, PhotoAiTagView, PluginInstallation, PluginState, TaggingJob,
    };
    use vrc_photon_core::db::Db;
    use vrc_photon_core::model::{JobStatusCounts, NewPhoto};
    use vrc_photon_ipc::{ErrorCode, Request, Response};

    fn backend(kind: &str) -> BackendInfo {
        BackendInfo {
            kind: kind.to_string(),
            device: "test".to_string(),
        }
    }

    fn sample_probe() -> Probe {
        Probe {
            protocol_version: 1,
            sidecar_version: "1.0.0".to_string(),
            plugin_id: AI_PLUGIN_ID.to_string(),
            variant: "linux-vulkan".to_string(),
            catalog_version: "2026-11-03.1".to_string(),
            backends: vec![backend("vulkan"), backend("cpu")],
        }
    }

    fn sample_install() -> PluginInstallation {
        PluginInstallation {
            plugin_id: AI_PLUGIN_ID.to_string(),
            variant: "linux-vulkan".to_string(),
            version: "1.0.0".to_string(),
            protocol_version: 1,
            path: "/plugins/vrc-photon-autotag/linux-vulkan/1.0.0".to_string(),
            manifest_json: "{}".to_string(),
            catalog_version: Some("2026-11-03.1".to_string()),
            active: true,
            installed_at: "2026-11-04T00:00:00Z".to_string(),
            updated_at: "2026-11-04T00:00:00Z".to_string(),
        }
    }

    fn sample_state(model_state: ModelState) -> PluginState {
        PluginState {
            plugin_id: AI_PLUGIN_ID.to_string(),
            model_state,
            model_revision: Some("rev".to_string()),
            model_bytes_downloaded: 0,
            model_bytes_total: None,
            model_error: None,
            probe_json: None,
            updated_at: "2026-11-04T00:00:00Z".to_string(),
        }
    }

    fn installed_missing_model() -> AiUiState {
        let install = sample_install();
        AiUiState {
            installations: vec![install.clone()],
            active: Some(install),
            state: Some(sample_state(ModelState::Missing)),
            ..AiUiState::default()
        }
    }

    fn ready_state() -> AiUiState {
        let install = sample_install();
        AiUiState {
            installations: vec![install.clone()],
            active: Some(install),
            state: Some(sample_state(ModelState::Ready)),
            probe: Some(sample_probe()),
            ..AiUiState::default()
        }
    }

    fn sample_job(id: &str, status: TaggingJobStatus) -> TaggingJob {
        TaggingJob {
            id: id.to_string(),
            photo_id: 7,
            plugin_id: AI_PLUGIN_ID.to_string(),
            variant: None,
            status,
            progress: 0.0,
            stage: None,
            error: None,
            retry_count: 0,
            created_at: "2026-11-04T00:00:00Z".to_string(),
            started_at: None,
            finished_at: None,
        }
    }

    fn seed_photo(db: &Db) -> i64 {
        let root = db.add_root("/photos", "reference").unwrap();
        db.upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: "a.png".to_string(),
            abs_path: "/photos/a.png".to_string(),
            file_name: "a.png".to_string(),
            captured_at_utc: 1_700_000_000,
            captured_at_local: "2023-11-14 22:13:20.000".to_string(),
            tz_offset_sec: 0,
            width: 1920,
            height: 1080,
            file_size: 1234,
            mtime: 1_700_000_000,
            blake3: None,
        })
        .unwrap()
    }

    fn test_catalog() -> Catalog {
        Catalog {
            version: "2026-11-03.1".to_string(),
            outdoor: vec![
                TagDefinition {
                    key: "sea".to_string(),
                    name: "海".to_string(),
                    name_en: Some("Sea".to_string()),
                    description: "海辺".to_string(),
                    default_threshold: 0.5,
                },
                TagDefinition {
                    key: "park".to_string(),
                    name: "公園".to_string(),
                    name_en: None,
                    description: "公園".to_string(),
                    default_threshold: 0.5,
                },
            ],
            indoor: Vec::new(),
        }
    }

    fn test_ai_result(tags: &[(&str, f32)]) -> NewPhotoAi {
        NewPhotoAi {
            variant: "linux-cpu".to_string(),
            sidecar_version: "1.0.0".to_string(),
            model_version: "d1-2B-Q8_0".to_string(),
            model_revision: "rev-1".to_string(),
            catalog_version: "2026-11-03.1".to_string(),
            source: "original".to_string(),
            presence_threshold: 0.9,
            character_present: true,
            presence_probability: 0.99,
            people_min: Some(2),
            people_max: Some(3),
            people_median: Some(3),
            place_branch: Some("outdoor".to_string()),
            place_probabilities_json: Some(
                r#"{"indoor":0.1,"outdoor":0.8,"other":0.1}"#.to_string(),
            ),
            tags: tags
                .iter()
                .map(|(key, probability)| ((*key).to_string(), *probability))
                .collect(),
        }
    }

    #[test]
    fn model_progress_ratio_handles_unknown_total() {
        assert_eq!(model_progress_ratio(0, None), 0.0);
        assert_eq!(model_progress_ratio(50, Some(100)), 0.5);
        assert_eq!(model_progress_ratio(200, Some(100)), 1.0);
        assert_eq!(model_progress_ratio(10, Some(0)), 0.0);
    }

    #[test]
    fn plugin_card_state_selects_the_right_primary_action() {
        let view = AiPluginCardView::from(&AiUiState::default());
        assert_eq!(view.state, AiPluginCardState::NotInstalled);
        assert_eq!(view.primary_action, AiPluginAction::InstallZip);

        let view = AiPluginCardView::from(&installed_missing_model());
        assert_eq!(view.state, AiPluginCardState::ModelMissing);
        assert_eq!(view.primary_action, AiPluginAction::DownloadModel);

        let view = AiPluginCardView::from(&ready_state());
        assert_eq!(view.state, AiPluginCardState::Ready);
        assert_eq!(view.primary_action, AiPluginAction::Recheck);
    }

    #[test]
    fn plugin_card_requires_a_probe_before_reporting_ready() {
        let mut ai = ready_state();
        ai.probe = None;

        assert_eq!(ai_plugin_card_state(&ai), AiPluginCardState::NeedsProbe);
        let view = AiPluginCardView::from(&ai);
        assert_eq!(view.state, AiPluginCardState::NeedsProbe);
        assert_eq!(view.primary_action, AiPluginAction::Recheck);
    }

    #[test]
    fn only_one_version_of_a_variant_is_marked_active() {
        let mut older = sample_install();
        older.version = "1.0.0".to_string();
        older.active = true;
        let mut newer = sample_install();
        newer.version = "2.0.0".to_string();
        newer.active = false;
        let installations = [older, newer];

        let active: Vec<_> = installations
            .iter()
            .filter(|install| is_active_installation(install))
            .collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].version, "1.0.0");
        assert_ne!(
            installation_row_id(&installations[0]),
            installation_row_id(&installations[1])
        );
    }

    #[test]
    fn plugin_card_has_no_primary_action_while_downloading() {
        let mut ai = installed_missing_model();
        ai.download = Some(ModelDownloadView {
            bytes: 5,
            total: Some(10),
            state: ModelState::Downloading,
        });
        assert_eq!(
            AiPluginCardView::from(&ai).primary_action,
            AiPluginAction::None
        );
    }

    #[test]
    fn plugin_card_error_state_retries_the_download() {
        let mut ai = installed_missing_model();
        ai.state = Some(sample_state(ModelState::Error));
        assert_eq!(
            AiPluginCardView::from(&ai).primary_action,
            AiPluginAction::DownloadModel
        );
    }

    #[test]
    fn manifest_view_reads_license_and_model_size() {
        let json = serde_json::json!({
            "files": [{"path": "LICENSE-LFM.txt"}],
            "model": {
                "id": "d1-2B",
                "quantization": "Q8_0",
                "revision": "5ebcce3",
                "license": "LFM Open License v1.0",
                "license_url": "https://example.com/license",
                "files": [{"size": 5}, {"size": 6}]
            }
        });
        let view = ai_manifest_view(&json.to_string());
        assert_eq!(view.license.as_deref(), Some("LFM Open License v1.0"));
        assert_eq!(
            view.license_url.as_deref(),
            Some("https://example.com/license")
        );
        assert_eq!(view.license_file.as_deref(), Some("LICENSE-LFM.txt"));
        assert_eq!(view.model_name.as_deref(), Some("d1-2B Q8_0"));
        assert_eq!(view.model_size, Some(11));
        assert_eq!(view.revision.as_deref(), Some("5ebcce3"));
    }

    #[test]
    fn manifest_view_tolerates_missing_or_malformed_json() {
        let view = ai_manifest_view("{}");
        assert_eq!(view, super::AiManifestView::default());
        let view = ai_manifest_view("not json");
        assert_eq!(view, super::AiManifestView::default());
    }

    #[test]
    fn variant_backend_labels_are_readable() {
        assert_eq!(variant_backend_label("linux-vulkan"), "Vulkan");
        assert_eq!(variant_backend_label("windows-cuda"), "CUDA");
        assert_eq!(variant_backend_label("linux-cpu"), "CPU");
        assert_eq!(variant_backend_label("linux-rocm"), "ROCm");
        assert_eq!(variant_backend_label("generic"), "generic");
    }

    #[test]
    fn backend_options_only_include_probe_backends_and_auto() {
        let probe = Probe {
            backends: vec![backend("vulkan"), backend("cpu")],
            ..sample_probe()
        };
        let options = backend_options(&probe, "rocm");
        assert_eq!(options[0], ("auto".into(), true));
        assert!(options.iter().any(|(id, _)| id == "vulkan"));
        assert!(!options.iter().any(|(id, _)| id == "rocm"));

        let options = backend_options(&probe, "vulkan");
        assert_eq!(options[0], ("auto".into(), false));
        assert!(
            options
                .iter()
                .any(|(id, selected)| id == "vulkan" && *selected)
        );
    }

    #[test]
    fn ai_ready_requires_install_model_and_probe() {
        let mut ai = AiUiState::default();
        assert!(!ai.ready());

        ai.active = Some(sample_install());
        assert!(!ai.ready());

        ai.state = Some(sample_state(ModelState::Ready));
        assert!(!ai.ready());

        ai.probe = Some(sample_probe());
        assert!(ai.ready());

        ai.state = Some(sample_state(ModelState::Missing));
        assert!(!ai.ready());
    }

    #[test]
    fn handle_tagging_progress_updates_counts() {
        let mut ai = AiUiState {
            job_counts: JobStatusCounts {
                waiting: 1,
                ..JobStatusCounts::default()
            },
            ..AiUiState::default()
        };
        ai.active_jobs
            .push(sample_job("j1", TaggingJobStatus::Waiting));

        ai.apply_tagging_progress("j1", 7, TaggingJobStatus::Pending, 0.0, None, None);
        assert_eq!(ai.job_counts.waiting, 0);
        assert_eq!(ai.job_counts.pending, 1);

        ai.apply_tagging_progress(
            "j1",
            7,
            TaggingJobStatus::Processing,
            0.4,
            Some(vrc_photon_ai_protocol::TaggingStage::Presence),
            None,
        );
        assert_eq!(ai.job_counts.pending, 0);
        assert_eq!(ai.job_counts.processing, 1);
        assert_eq!(ai.active_jobs[0].progress, 0.4);

        ai.apply_tagging_progress(
            "j1",
            7,
            TaggingJobStatus::Completed,
            1.0,
            Some(vrc_photon_ai_protocol::TaggingStage::Write),
            None,
        );
        assert_eq!(ai.job_counts.processing, 0);
        assert!(ai.active_jobs.is_empty());
    }

    #[test]
    fn processing_to_failed_increments_failed_count() {
        let mut ai = AiUiState {
            job_counts: JobStatusCounts {
                processing: 1,
                ..JobStatusCounts::default()
            },
            ..AiUiState::default()
        };
        ai.active_jobs
            .push(sample_job("j1", TaggingJobStatus::Processing));

        ai.apply_tagging_progress(
            "j1",
            7,
            TaggingJobStatus::Failed,
            0.5,
            Some(vrc_photon_ai_protocol::TaggingStage::Presence),
            Some("sidecar_exited"),
        );

        assert_eq!(ai.job_counts.processing, 0);
        assert_eq!(ai.job_counts.failed, 1);
        assert!(ai.active_jobs.is_empty());
        let error = ai.error_of(AiErrorKind::Job).expect("job error");
        assert_eq!(error.code, "sidecar_exited");
    }

    #[test]
    fn empty_plugin_id_install_failure_clears_busy_and_reports() {
        assert!(install_event_applies_to_ai("", PluginInstallStatus::Failed));
        assert!(!install_event_applies_to_ai(
            "",
            PluginInstallStatus::Running
        ));
        assert!(!install_event_applies_to_ai(
            "another-plugin",
            PluginInstallStatus::Failed
        ));

        let mut ai = AiUiState {
            install_busy: true,
            ..AiUiState::default()
        };
        // A failure before the ZIP yields a plugin id arrives with an empty
        // id; it must still clear the busy flag and report the error.
        let toast = apply_install_event(
            &mut ai,
            "",
            PluginInstallStatus::Failed,
            Some("signature verification failed"),
        );
        assert_eq!(toast.as_deref(), Some("signature verification failed"));
        assert!(!ai.install_busy);
        let error = ai.error_of(AiErrorKind::Install).expect("install error");
        assert_eq!(error.code, "signature verification failed");

        let mut ai = AiUiState {
            install_busy: true,
            ..AiUiState::default()
        };
        assert!(
            apply_install_event(&mut ai, "", PluginInstallStatus::Running, None).is_none(),
            "a running event without an id is not attributable to the AI plugin"
        );
        assert!(ai.install_busy);
        // A daemon that accepts other plugin ids may publish their terminal
        // event while the AI install flow is still marked busy. The event is
        // not an AI failure, but it must not wedge the card forever.
        assert!(
            apply_install_event(
                &mut ai,
                "another-plugin",
                PluginInstallStatus::Installed,
                None
            )
            .is_none()
        );
        assert!(!ai.install_busy);
        assert!(
            [
                AiErrorKind::Install,
                AiErrorKind::Model,
                AiErrorKind::Job,
                AiErrorKind::Operation,
            ]
            .into_iter()
            .all(|kind| ai.error_of(kind).is_none())
        );
    }

    #[test]
    fn disconnect_resets_transient_ai_connection_state() {
        let mut ai = AiUiState {
            install_busy: true,
            probe_pending: true,
            download: Some(ModelDownloadView {
                bytes: 1,
                total: Some(2),
                state: ModelState::Downloading,
            }),
            ..AiUiState::default()
        };
        ai.reset_connection();
        assert!(!ai.install_busy);
        assert!(!ai.probe_pending);
        assert!(ai.download.is_none());
    }

    #[test]
    fn pending_probe_resolves_with_the_terminal_state() {
        // The first event after a switch may still have no probe; only the
        // terminal event decides success or failure.
        let mut ai = AiUiState::default();
        ai.begin_probe();
        assert!(ai.finish_probe(), "an absent terminal probe is a failure");
        assert!(!ai.finish_probe(), "the pending flag was consumed");

        let mut ai = AiUiState::default();
        ai.begin_probe();
        ai.probe = Some(sample_probe());
        assert!(!ai.finish_probe(), "a stored terminal probe is a success");
        assert!(!ai.finish_probe(), "the pending flag was consumed");
    }

    #[test]
    fn probe_failure_uses_the_operation_label() {
        let error = probe_failure_info();
        assert_eq!(error.kind, AiErrorKind::Operation);

        let toast = ai_error_toast_in("en", &error);
        assert_eq!(
            toast,
            "The AI tagging operation failed: The plugin probe failed"
        );
        assert!(
            !toast.contains("install"),
            "a failed probe must not be labelled as an install failure: {toast}"
        );
    }

    #[test]
    fn variant_uninstall_maps_to_variant_fields_and_keeps_tags() {
        let install = sample_install();
        let target = UninstallTarget::for_install(&install);
        assert_eq!(
            uninstall_request_args(&target, true),
            (
                Some("linux-vulkan".to_string()),
                Some("1.0.0".to_string()),
                false
            )
        );

        let plugin_wide = UninstallTarget::plugin_wide();
        assert_eq!(
            uninstall_request_args(&plugin_wide, true),
            (None, None, true)
        );
    }

    #[test]
    fn runtime_job_rows_prefer_file_names_with_an_id_fallback() {
        assert_eq!(
            active_job_photo_label(Some("screenshot.png"), 7, "ja"),
            "screenshot.png"
        );
        assert_eq!(active_job_photo_label(Some("  "), 7, "ja"), "写真 #7");
        assert_eq!(active_job_photo_label(None, 7, "en"), "Photo #7");
    }

    #[test]
    fn repeated_install_and_model_failures_toast_once() {
        let mut ai = AiUiState::default();

        assert_eq!(
            ai.apply_install_progress(PluginInstallStatus::Failed, Some("install_failed")),
            Some("install_failed".to_string())
        );
        assert_eq!(
            ai.apply_install_progress(PluginInstallStatus::Failed, Some("install_failed")),
            None
        );
        assert!(!ai.install_busy);

        ai.apply_install_progress(PluginInstallStatus::Running, None);
        assert!(ai.install_busy);
        assert_eq!(
            ai.apply_install_progress(PluginInstallStatus::Failed, Some("install_failed")),
            Some("install_failed".to_string())
        );

        assert_eq!(
            ai.apply_model_download(
                ModelState::Error,
                10,
                Some(20),
                Some("model_download_failed")
            ),
            Some("model_download_failed".to_string())
        );
        assert_eq!(
            ai.apply_model_download(
                ModelState::Error,
                10,
                Some(20),
                Some("model_download_failed")
            ),
            None
        );
        assert_eq!(
            ai.apply_model_download(ModelState::Downloading, 10, Some(20), None),
            None
        );
        assert!(ai.download.is_some());
    }

    #[test]
    fn install_and_job_errors_keep_separate_origins() {
        let mut ai = AiUiState::default();
        ai.apply_install_progress(PluginInstallStatus::Failed, Some("install_failed"));
        ai.apply_tagging_progress(
            "j1",
            7,
            TaggingJobStatus::Failed,
            0.5,
            None,
            Some("sidecar_exited"),
        );

        assert_eq!(
            ai.error_of(AiErrorKind::Install)
                .map(|error| error.code.as_str()),
            Some("install_failed")
        );
        assert_eq!(
            ai.error_of(AiErrorKind::Job)
                .map(|error| error.code.as_str()),
            Some("sidecar_exited")
        );
        assert!(ai.error_of(AiErrorKind::Model).is_none());
    }

    #[test]
    fn license_required_only_once() {
        assert!(needs_license_consent(None));
        let consent = Some(1_700_000_000);
        assert!(!needs_license_consent(consent));
        assert!(!needs_license_consent(consent));
    }

    #[test]
    fn error_label_maps_known_codes() {
        assert_eq!(
            ai_error_label_in("ja", "model_download_failed"),
            "モデルのダウンロードに失敗しました"
        );
        assert_eq!(
            ai_error_label_in("ja", "consent_required"),
            "モデルライセンスへの同意が必要です"
        );
        assert_eq!(
            ai_error_label_in("ja", "interrupted"),
            "処理が中断されました"
        );
        assert_eq!(
            ai_error_label_in("ja", "protocol_error"),
            "推論プラグインとの通信が不正です"
        );
        assert_eq!(
            ai_error_label_in("ja", "model_load_failed"),
            "モデルを読み込めませんでした"
        );
        assert_eq!(ai_error_label_in("ja", "unknown_code"), "unknown_code");

        let busy = AiErrorInfo::new(AiErrorKind::Install, "busy", Some("raw detail".into()));
        assert_eq!(
            ai_error_summary_in("ja", &busy),
            "プラグインを導入できませんでした"
        );
        assert_eq!(
            ai_error_detail_in("ja", &busy).as_deref(),
            Some("別のAIタグ付け操作が実行中です: raw detail")
        );
        assert_eq!(
            ai_error_toast_in("ja", &busy),
            "プラグインを導入できませんでした: 別のAIタグ付け操作が実行中です: raw detail"
        );

        let unknown = AiErrorInfo::new(
            AiErrorKind::Model,
            "unknown_code",
            Some("raw detail".into()),
        );
        assert_eq!(
            ai_error_summary_in("ja", &unknown),
            "モデルの準備に失敗しました"
        );
        assert_eq!(
            ai_error_detail_in("ja", &unknown).as_deref(),
            Some("raw detail")
        );
    }

    #[test]
    fn probe_and_uninstall_failures_use_the_operation_label() {
        assert_eq!(
            request_error_kind(&Request::ProbePlugin {
                plugin_id: AI_PLUGIN_ID.to_string(),
            }),
            AiErrorKind::Operation
        );
        assert_eq!(
            request_error_kind(&Request::UninstallPlugin {
                plugin_id: AI_PLUGIN_ID.to_string(),
                variant: None,
                version: None,
                delete_ai_tags: false,
            }),
            AiErrorKind::Operation
        );
        assert_eq!(
            request_error_kind(&Request::InstallPlugin {
                zip_path: "plugin.zip".to_string(),
            }),
            AiErrorKind::Install
        );
        assert_eq!(
            request_error_kind(&Request::StartModelDownload {
                plugin_id: AI_PLUGIN_ID.to_string(),
            }),
            AiErrorKind::Model
        );
        assert_eq!(
            ai_error_summary_in(
                "en",
                &AiErrorInfo::new(AiErrorKind::Operation, "not_ready", None)
            ),
            "The AI tagging operation failed"
        );
    }

    #[test]
    fn error_code_line_only_accepts_stable_identifiers() {
        assert!(ai_error_code_is_stable("model_download_failed"));
        assert!(ai_error_code_is_stable("plugin_probe_failed"));
        assert!(ai_error_code_is_stable("internal"));
        assert!(!ai_error_code_is_stable("signature verification failed"));
        assert!(!ai_error_code_is_stable("Could not open the file"));
        assert!(!ai_error_code_is_stable(""));
    }

    #[test]
    fn send_ai_request_treats_response_error_as_failure() {
        let outcome = ai_response_outcome(&Response::Error {
            code: ErrorCode::ConsentRequired,
            message: "license".to_string(),
        });
        assert_eq!(
            outcome,
            AiResponseOutcome::Error {
                code: "consent_required",
                message: "license".to_string(),
            }
        );
        assert_eq!(ai_response_outcome(&Response::Ok), AiResponseOutcome::Ok);
    }

    #[test]
    fn people_and_place_labels_are_readable() {
        assert_eq!(people_label(Some(3), Some(3)), "3人");
        assert_eq!(people_label(Some(3), Some(5)), "3〜5人");
        assert_eq!(people_label(Some(0), Some(0)), "0人");
        assert_eq!(people_label(None, None), "—");
        assert_eq!(people_label_in("en", Some(3), Some(3)), "3 people");
        assert_eq!(people_label_in("en", Some(3), Some(5)), "3–5 people");
        assert_eq!(people_label_in("en", Some(1), Some(1)), "1 person");
        assert_eq!(people_label_in("en", Some(0), Some(0)), "0 people");
        assert_eq!(people_label_in("en", None, Some(1)), "Up to 1 person");
        assert_eq!(people_label_in("en", None, None), "—");
        assert_eq!(people_label_in("ja", Some(1), Some(1)), "1人");
        assert_eq!(people_label_in("ja", None, Some(1)), "1人以下");
        assert_eq!(place_label(Some("indoor")), "屋内");
        assert_eq!(place_label(Some("outdoor")), "屋外");
        assert_eq!(place_label(Some("other")), "その他");
        assert_eq!(place_label(None), "—");
    }

    #[test]
    fn place_label_appends_confidence_when_available() {
        let json = r#"{"indoor":0.019,"outdoor":0.981,"other":0.0}"#;
        assert_eq!(
            place_label_with_confidence(Some("outdoor"), Some(json)),
            "屋外 98.1%"
        );
        assert_eq!(
            place_label_with_confidence(Some("indoor"), Some(json)),
            "屋内 1.9%"
        );
        assert_eq!(place_label_with_confidence(Some("outdoor"), None), "屋外");
        assert_eq!(
            place_label_with_confidence(Some("outdoor"), Some("not json")),
            "屋外"
        );
        assert_eq!(place_label_with_confidence(None, Some(json)), "—");
    }

    #[test]
    fn job_progress_ratio_clamps_out_of_range_values() {
        assert_eq!(download_progress_bar_ratio(0.0), 0.0);
        assert!((download_progress_bar_ratio(0.68) - 0.68).abs() < f32::EPSILON);
        assert_eq!(download_progress_bar_ratio(1.5), 1.0);
        assert_eq!(download_progress_bar_ratio(-0.2), 0.0);
        assert_eq!(download_progress_bar_ratio(f64::NAN), 0.0);
    }

    #[test]
    fn backend_selection_is_disabled_until_ready_and_enabled() {
        assert!(backend_selection_is_disabled_when_not_ready(false, false));
        assert!(backend_selection_is_disabled_when_not_ready(false, true));
        assert!(backend_selection_is_disabled_when_not_ready(true, false));
        assert!(!backend_selection_is_disabled_when_not_ready(true, true));
    }

    #[test]
    fn ai_tag_chip_shows_probability() {
        let tag = PhotoAiTagView {
            tag_key: "sea".to_string(),
            name: "海".to_string(),
            name_en: Some("Sea".to_string()),
            probability: 0.969,
            applied: true,
        };
        let ja = ai_tag_view(&tag, "ja");
        assert_eq!(ja.label, "海");
        assert!(ja.tooltip.contains("0.969"));
        assert_eq!(ja.probability_label, "96.9%");

        let en = ai_tag_view(&tag, "en");
        assert_eq!(en.label, "Sea");

        let with_context =
            ai_tag_view_with_context(&tag, "ja", Some("d1-2B Q8_0"), Some("2026-11-04 12:45"));
        assert!(with_context.tooltip.contains("0.969"));
        assert!(with_context.tooltip.contains("d1-2B Q8_0"));
        assert!(with_context.tooltip.contains("2026-11-04 12:45"));

        let without_context = ai_tag_view_with_context(&tag, "ja", None, None);
        assert_eq!(without_context.tooltip, ja.tooltip);
    }

    #[test]
    fn tagged_at_is_formatted_for_display() {
        let raw = "2026-11-04T12:45:01+00:00";
        let local = chrono::DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&chrono::Local);
        assert_eq!(
            format_tagged_at(raw),
            local.format("%Y-%m-%d %H:%M").to_string()
        );
        assert_eq!(format_tagged_at("not a timestamp"), "not a timestamp");
    }

    #[test]
    fn empty_state_copy_distinguishes_missing_and_unapplied_tags() {
        let (title, hint) = ai_empty_state_labels(false);
        assert_eq!(title, "AIタグ未生成");
        assert_eq!(hint, "「AIタグを再生成」で作成");

        let (title, hint) = ai_empty_state_labels(true);
        assert_eq!(title, "適用されたAIタグはありません");
        assert_eq!(hint, "しきい値を満たしたタグはありません");
    }

    #[test]
    fn selected_ai_tags_use_english_names_when_language_is_en() {
        let tags = [
            PhotoAiTagView {
                tag_key: "sea".to_string(),
                name: "海".to_string(),
                name_en: Some("Sea".to_string()),
                probability: 0.969,
                applied: true,
            },
            PhotoAiTagView {
                tag_key: "park".to_string(),
                name: "公園".to_string(),
                name_en: None,
                probability: 0.61,
                applied: true,
            },
        ];

        let ja = ai_tag_views(&tags, "ja");
        assert_eq!(
            ja.iter()
                .map(|view| view.label.as_str())
                .collect::<Vec<_>>(),
            ["海", "公園"]
        );

        let en = ai_tag_views(&tags, "en");
        assert_eq!(
            en.iter()
                .map(|view| view.label.as_str())
                .collect::<Vec<_>>(),
            ["Sea", "公園"],
            "a missing English name falls back to the catalog's Japanese name"
        );
    }

    #[test]
    fn load_photo_ai_returns_metadata_and_only_applied_tags() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        db.sync_ai_catalog(AI_PLUGIN_ID, &test_catalog()).unwrap();
        db.save_photo_ai(
            photo,
            AI_PLUGIN_ID,
            &test_ai_result(&[("sea", 0.97), ("park", 0.4)]),
        )
        .unwrap();

        let (ai, tags) = load_photo_ai(&db, photo);
        let ai = ai.expect("saved AI metadata");
        assert_eq!(ai.people_min, Some(2));
        assert_eq!(ai.place_branch.as_deref(), Some("outdoor"));
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag_key, "sea");
        assert!(tags[0].applied);

        let (missing_ai, missing_tags) = load_photo_ai(&db, photo + 1);
        assert!(missing_ai.is_none());
        assert!(missing_tags.is_empty());
    }

    #[test]
    fn manual_retag_enqueues_one_job_and_wakes() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);

        let outcome = enqueue_manual_retag(&db, true, true, &[photo]).unwrap();
        assert_eq!(outcome, ManualRetagOutcome::Queued(1));
        assert!(outcome.wakes());

        let jobs = db.tagging_jobs_for_photo(photo).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, TaggingJobStatus::Pending);
        assert_eq!(jobs[0].plugin_id, AI_PLUGIN_ID);
    }

    #[test]
    fn retag_requires_enabled_plugin() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);

        let disabled = enqueue_manual_retag(&db, false, true, &[photo]).unwrap();
        assert_eq!(disabled, ManualRetagOutcome::Disabled);
        assert!(!disabled.wakes());
        assert!(db.tagging_jobs_for_photo(photo).unwrap().is_empty());

        let not_ready = enqueue_manual_retag(&db, true, false, &[photo]).unwrap();
        assert_eq!(not_ready, ManualRetagOutcome::NotReady);
        assert!(!not_ready.wakes());
        assert!(db.tagging_jobs_for_photo(photo).unwrap().is_empty());
    }

    #[test]
    fn snapshot_maps_active_job_photo_names() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        db.enqueue_tagging_jobs(&[photo], AI_PLUGIN_ID, TaggingJobStatus::Pending)
            .unwrap();

        let snapshot = collect_ai_snapshot(&db);

        assert_eq!(snapshot.active_jobs.len(), 1);
        assert_eq!(
            snapshot.active_job_names.get(&photo).map(String::as_str),
            Some("a.png")
        );
    }

    #[test]
    fn snapshot_reads_the_install_probe_and_jobs() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_plugin_installation(&sample_install()).unwrap();
        let probe = sample_probe();
        db.set_plugin_probe_json(AI_PLUGIN_ID, Some(&serde_json::to_string(&probe).unwrap()))
            .unwrap();

        let mut ai = AiUiState::default();
        ai.apply_snapshot(collect_ai_snapshot(&db));

        assert_eq!(ai.installations.len(), 1);
        assert_eq!(
            ai.active.as_ref().map(|install| install.variant.as_str()),
            Some("linux-vulkan")
        );
        assert_eq!(ai.probe, Some(probe));
        assert_eq!(ai.job_counts, JobStatusCounts::default());
        assert!(ai.active_jobs.is_empty());
    }

    #[test]
    fn model_ready_toast_only_fires_after_a_download() {
        assert!(model_ready_toast_pending(true, ModelState::Ready));
        assert!(!model_ready_toast_pending(false, ModelState::Ready));
        assert!(!model_ready_toast_pending(true, ModelState::Downloading));
    }

    #[test]
    fn notifiable_job_errors_cover_sidecar_and_resource_failures() {
        for code in [
            "sidecar_exited",
            "sidecar_timeout",
            "sidecar_protocol",
            "protocol_error",
            "model_load_failed",
            "backend_unavailable",
            "out_of_memory",
        ] {
            assert!(ai_job_error_is_notifiable(code), "{code}");
        }
        assert!(!ai_job_error_is_notifiable("image_missing"));
        assert!(!ai_job_error_is_notifiable("cancelled"));
    }

    #[test]
    fn repeated_sidecar_crash_is_recorded_only_once() {
        let mut ai = ready_state();
        assert!(ai.apply_tagging_progress(
            "job-1",
            1,
            TaggingJobStatus::Failed,
            0.0,
            None,
            Some("sidecar_exited"),
        ));
        assert!(!ai.apply_tagging_progress(
            "job-2",
            2,
            TaggingJobStatus::Failed,
            0.0,
            None,
            Some("sidecar_exited"),
        ));
    }

    #[test]
    fn dashboard_ai_card_is_hidden_without_an_installation() {
        assert!(!ai_dashboard_view(&AiUiState::default(), true).visible);

        let mut installed = AiUiState::default();
        installed.installations.push(sample_install());
        let view = ai_dashboard_view(&installed, false);
        assert!(view.visible);
        assert_eq!(view.state, AiDashboardState::NotReady);
    }

    #[test]
    fn dashboard_ai_card_is_hidden_when_ready_but_not_enabled() {
        let ai = ready_state();
        let hidden = ai_dashboard_view(&ai, false);
        assert!(!hidden.visible);
        assert!(hidden.jobs.is_empty());

        assert_eq!(ai_dashboard_view(&ai, true).state, AiDashboardState::Idle);

        let mut active = ai.clone();
        active.job_counts.processing = 1;
        assert_eq!(
            ai_dashboard_view(&active, true).state,
            AiDashboardState::Active
        );

        let mut failed = active.clone();
        failed.job_counts.failed = 2;
        assert_eq!(
            ai_dashboard_view(&failed, true).state,
            AiDashboardState::Failed
        );

        let mut not_ready = failed.clone();
        not_ready.state = Some(sample_state(ModelState::Missing));
        assert_eq!(
            ai_dashboard_view(&not_ready, true).state,
            AiDashboardState::NotReady
        );
    }

    #[test]
    fn dashboard_ai_card_limits_jobs_to_three_and_counts_the_rest() {
        let mut ai = ready_state();
        ai.active_jobs = (0..5)
            .map(|index| sample_job(&format!("job-{index}"), TaggingJobStatus::Processing))
            .collect();
        ai.job_counts.processing = 5;
        let view = ai_dashboard_view(&ai, true);
        assert_eq!(view.jobs.len(), 3);
        assert_eq!(view.more, 2);
        assert_eq!(view.jobs[0].status, TaggingJobStatus::Processing);
    }

    #[test]
    fn dashboard_ai_card_counts_waiting_jobs_without_a_job_row() {
        let mut ai = ready_state();
        ai.job_counts.waiting = 4;
        let view = ai_dashboard_view(&ai, true);
        assert_eq!(view.state, AiDashboardState::Active);
        assert!(view.jobs.is_empty());
        assert_eq!(view.more, 4);
    }

    #[test]
    fn tagging_completion_schedules_the_background_live_refresh() {
        assert_eq!(
            super::App::tagging_live_refresh(false),
            TaggingLiveRefresh::None
        );
        assert_eq!(
            super::App::tagging_live_refresh(true),
            TaggingLiveRefresh::Background,
            "terminal jobs must use the coalesced background pass"
        );
    }
}
