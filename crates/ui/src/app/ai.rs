//! AI tagging UI state, actions, and daemon event handling.
//!
//! The daemon owns plugin installation, model download, and the tagging
//! worker. This module mirrors that state into the UI, sends the user's
//! requests over IPC, and turns progress events into card and inspector
//! updates. Rendering lives in `ui::settings`, `ui::integrations`, and
//! `ui::library`.

use std::time::Duration;

use gpui_kit::*;
use vrc_photon_ai_protocol::{
    ModelState, PluginInstallStatus, Probe, TaggingJobStatus, TaggingStage,
};
use vrc_photon_core::ai::{PhotoAiTagView, PluginInstallation, PluginState, TaggingJob};
use vrc_photon_core::db::Db;
use vrc_photon_core::model::JobStatusCounts;
use vrc_photon_core::settings::AppSettings;
use vrc_photon_i18n::{MISSING, tr, tr_args};
use vrc_photon_ipc::{
    ErrorCode, Event as IpcEvent, Request as IpcRequest, Response as IpcResponse,
};

use crate::app::App;
use crate::ipc_bridge;

/// Plugin id of the bundled AI tagging sidecar package.
pub(crate) const AI_PLUGIN_ID: &str = "vrc-photon-autotag";

/// Delay before an event burst triggers a state or inspector refresh.
const AI_REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Everything the AI tagging cards render. Populated from the database and
/// kept current by daemon events.
#[derive(Default)]
pub(crate) struct AiUiState {
    pub(crate) installations: Vec<PluginInstallation>,
    pub(crate) active: Option<PluginInstallation>,
    pub(crate) state: Option<PluginState>,
    pub(crate) probe: Option<Probe>,
    pub(crate) download: Option<ModelDownloadView>,
    pub(crate) install_busy: bool,
    pub(crate) job_counts: JobStatusCounts,
    pub(crate) active_jobs: Vec<TaggingJob>,
    pub(crate) license_modal_open: bool,
    pub(crate) uninstall_modal_open: bool,
    pub(crate) uninstall_delete_tags: bool,
    pub(crate) last_error: Option<String>,
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
    pub(crate) job_counts: JobStatusCounts,
    pub(crate) active_jobs: Vec<TaggingJob>,
}

/// The subset the three-second live poller collects on its background thread.
#[derive(Default)]
pub(crate) struct AiLiveSnapshot {
    pub(crate) job_counts: JobStatusCounts,
    pub(crate) active_jobs: Vec<TaggingJob>,
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
        self.download = self.state.as_ref().and_then(ModelDownloadView::from_state);
        self.job_counts = snapshot.job_counts;
        self.active_jobs = snapshot.active_jobs;
    }

    /// Applies the three-second poller snapshot. Returns whether anything the
    /// cards display changed.
    pub(crate) fn apply_live_snapshot(&mut self, snapshot: AiLiveSnapshot) -> bool {
        let changed = self.job_counts != snapshot.job_counts
            || self.active_jobs != snapshot.active_jobs
            || self.state != snapshot.state
            || self.probe != snapshot.probe;
        if changed {
            self.job_counts = snapshot.job_counts;
            self.active_jobs = snapshot.active_jobs;
            self.state = snapshot.state;
            self.probe = snapshot.probe;
            self.download = self.state.as_ref().and_then(ModelDownloadView::from_state);
        }
        changed
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
    ) {
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

        if status == TaggingJobStatus::Failed
            && let Some(error) = error
        {
            self.last_error = Some(error.to_string());
        }
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
                self.last_error = None;
                None
            }
            PluginInstallStatus::Installed => {
                self.install_busy = false;
                self.last_error = None;
                None
            }
            PluginInstallStatus::Failed => {
                self.install_busy = false;
                let error = error.unwrap_or("install_failed").to_string();
                if self.last_error.as_deref() == Some(error.as_str()) {
                    None
                } else {
                    self.last_error = Some(error.clone());
                    Some(error)
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
                self.last_error = None;
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
                self.last_error = None;
                self.update_model_state(status, bytes, total, None);
                None
            }
            ModelState::Error => {
                self.download = None;
                let error = error.unwrap_or("model_download_failed").to_string();
                self.update_model_state(status, bytes, total, Some(&error));
                if self.last_error.as_deref() == Some(error.as_str()) {
                    None
                } else {
                    self.last_error = Some(error.clone());
                    Some(error)
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

pub(crate) fn collect_ai_snapshot(db: &Db) -> AiSnapshot {
    let state = db.plugin_state(AI_PLUGIN_ID).unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot load AI plugin state");
        None
    });
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
        job_counts: db.tagging_job_counts(AI_PLUGIN_ID).unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot count AI tagging jobs");
            JobStatusCounts::default()
        }),
        active_jobs: db.active_tagging_jobs(20).unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot load active AI tagging jobs");
            Vec::new()
        }),
    }
}

pub(crate) fn collect_ai_live_snapshot(db: &Db) -> AiLiveSnapshot {
    let state = db.plugin_state(AI_PLUGIN_ID).unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot load AI plugin state");
        None
    });
    AiLiveSnapshot {
        probe: state.as_ref().and_then(probe_from_state),
        state,
        job_counts: db.tagging_job_counts(AI_PLUGIN_ID).unwrap_or_default(),
        active_jobs: db.active_tagging_jobs(20).unwrap_or_default(),
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

/// Backend chips are selectable only once the plugin is ready and AI tagging
/// is enabled, matching the mockup's "有効化後に選択" state.
pub(crate) fn backend_selection_is_disabled_when_not_ready(ready: bool, enabled: bool) -> bool {
    !ready || !enabled
}

/// Human-readable people count from the sidecar interval.
pub(crate) fn people_label(min: Option<u32>, max: Option<u32>) -> String {
    match (min, max) {
        (Some(min), Some(max)) if min == max => format!("{min}人"),
        (Some(min), Some(max)) => format!("{min}〜{max}人"),
        (Some(min), None) => format!("{min}人以上"),
        (None, Some(max)) => format!("{max}人以下"),
        (None, None) => "—".to_string(),
    }
}

/// Human-readable place branch from the classifier.
pub(crate) fn place_label(branch: Option<&str>) -> String {
    match branch {
        Some(branch) => match branch.trim().to_ascii_lowercase().as_str() {
            "indoor" => "屋内".to_string(),
            "outdoor" => "屋外".to_string(),
            _ => "その他".to_string(),
        },
        None => "—".to_string(),
    }
}

/// Compact queue summary for the runtime card.
pub(crate) fn ai_job_summary_label(counts: JobStatusCounts) -> String {
    format!(
        "待機 {} / 実行中 {} / 失敗 {}",
        counts.waiting,
        counts.pending + counts.processing,
        counts.failed
    )
}

/// The presentation of one AI tag chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiTagView {
    pub(crate) label: String,
    pub(crate) tooltip: String,
    pub(crate) ai_badge: bool,
}

/// Chooses the catalog name for the active language and formats the
/// probability tooltip. AI tags are read-only history, hence the badge.
pub(crate) fn ai_tag_view(tag: &PhotoAiTagView, language: &str) -> AiTagView {
    let label = if language.starts_with("en") {
        tag.name_en.clone().unwrap_or_else(|| tag.name.clone())
    } else {
        tag.name.clone()
    };
    let probability = format!("{:.3}", tag.probability);
    let tooltip = vrc_photon_i18n::tr_args_in(
        language,
        "photos.ai.probability",
        &[("probability", probability)],
    );
    AiTagView {
        label,
        tooltip,
        ai_badge: true,
    }
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

/// Prefer the translated label; when the code is unknown, show the daemon's
/// own message rather than the opaque code.
pub(crate) fn ai_error_text_in(language: &str, code: &str, message: &str) -> String {
    let label = ai_error_label_in(language, code);
    if label == code {
        message.to_string()
    } else {
        label
    }
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
        return None;
    }
    ai.apply_install_progress(status, error)
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
        if !self.ipc_connected {
            if clears_install_busy {
                self.ai.install_busy = false;
            }
            self.toast_error(tr!("daemon.ipc.disconnected"), cx);
            cx.notify();
            return;
        }
        let Some(endpoint) = self.ipc_endpoint.clone() else {
            if clears_install_busy {
                self.ai.install_busy = false;
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
                        }
                        AiResponseOutcome::Error { code, message } => {
                            if clears_install_busy {
                                this.ai.install_busy = false;
                            }
                            this.ai.last_error = Some(message.clone());
                            if code == "consent_required" {
                                this.ai.license_modal_open = true;
                            }
                            this.toast_error(
                                ai_error_text_in(
                                    &vrc_photon_i18n::current_locale(),
                                    code,
                                    &message,
                                ),
                                cx,
                            );
                        }
                    },
                    Err(error) => {
                        if clears_install_busy {
                            this.ai.install_busy = false;
                        }
                        this.toast_error(ipc_bridge::error_message(&error), cx);
                    }
                }
                this.refresh_ai_state(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn install_ai_plugin(&mut self, cx: &mut Context<Self>) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("ZIP", &["zip"])
            .pick_file()
        else {
            return;
        };
        self.ai.install_busy = true;
        self.ai.last_error = None;
        self.send_ai_request(
            IpcRequest::InstallPlugin {
                zip_path: path.to_string_lossy().into_owned(),
            },
            None,
            cx,
        );
    }

    pub(crate) fn import_ai_model_files(&mut self, cx: &mut Context<Self>) {
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("Model", &["gguf"])
            .pick_files()
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        self.ai.last_error = None;
        let paths = paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        self.send_ai_request(
            IpcRequest::ImportModelFiles {
                plugin_id: AI_PLUGIN_ID.to_string(),
                paths,
            },
            None,
            cx,
        );
    }

    /// Starts a model download. The first request opens the license modal;
    /// the persisted consent makes later downloads start immediately.
    pub(crate) fn request_ai_model_download(&mut self, cx: &mut Context<Self>) {
        if self.ai.active.is_none() {
            self.toast_warning(tr!("integrations.ai.not_ready"), cx);
            cx.notify();
            return;
        }
        if needs_license_consent(self.settings.ai_model_license_consent_at) {
            self.ai.license_modal_open = true;
            cx.notify();
            return;
        }
        self.start_ai_model_download(cx);
    }

    pub(crate) fn accept_ai_model_license(&mut self, cx: &mut Context<Self>) {
        let previous = self.settings.ai_model_license_consent_at;
        self.settings.ai_model_license_consent_at = Some(chrono::Utc::now().timestamp());
        if let Err(error) = self.settings.save(&self.db) {
            self.settings.ai_model_license_consent_at = previous;
            self.toast_error(tr_args!("settings.save_failed", error = error), cx);
            cx.notify();
            return;
        }
        self.ai.license_modal_open = false;
        self.start_ai_model_download(cx);
    }

    pub(crate) fn close_ai_license_modal(&mut self, cx: &mut Context<Self>) {
        self.ai.license_modal_open = false;
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

    pub(crate) fn uninstall_ai_plugin(&mut self, delete_ai_tags: bool, cx: &mut Context<Self>) {
        self.send_ai_request(
            IpcRequest::UninstallPlugin {
                plugin_id: AI_PLUGIN_ID.to_string(),
                variant: None,
                version: None,
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
        if !self.settings.ai_tagging_enabled {
            self.toast_warning(tr!("integrations.ai.disabled"), cx);
            cx.notify();
            return;
        }
        if !self.ai.ready() {
            self.toast_warning(tr!("integrations.ai.not_ready"), cx);
            cx.notify();
            return;
        }
        match self
            .db
            .enqueue_tagging_jobs(photo_ids, AI_PLUGIN_ID, TaggingJobStatus::Pending)
        {
            Ok(0) => self.toast_info(tr!("integrations.ai.enqueue.none"), cx),
            Ok(count) => {
                self.toast_success(
                    tr_args!("integrations.ai.enqueue.queued", count = count),
                    cx,
                );
                self.wake_daemon(IpcRequest::WakeTagging, cx);
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
                self.ai.apply_tagging_progress(
                    &job_id,
                    photo_id,
                    status,
                    progress,
                    stage,
                    error.as_deref(),
                );
                let terminal = matches!(
                    status,
                    TaggingJobStatus::Completed
                        | TaggingJobStatus::Failed
                        | TaggingJobStatus::Cancelled
                );
                // Progress events can arrive several times per photo. Coalesce
                // the database read and inspector refresh; the live poller
                // picks up the library revision after a terminal job.
                self.schedule_ai_refresh(terminal, cx);
                if terminal {
                    self.refresh_live(cx);
                }
                true
            }
            IpcEvent::PluginInstallProgress {
                plugin_id,
                status,
                error,
                ..
            } => {
                if !install_event_applies_to_ai(&plugin_id, status) {
                    return false;
                }
                if let Some(error) =
                    apply_install_event(&mut self.ai, &plugin_id, status, error.as_deref())
                {
                    self.toast_error(tr_args!("settings.ai.install_failed", error = error), cx);
                }
                self.refresh_ai_state(cx);
                true
            }
            IpcEvent::PluginStateChanged { plugin_id } => {
                if plugin_id != AI_PLUGIN_ID {
                    return false;
                }
                self.refresh_ai_state(cx);
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
                if let Some(error) =
                    self.ai
                        .apply_model_download(status, bytes, total, error.as_deref())
                {
                    self.toast_error(tr_args!("settings.ai.model_failed", error = error), cx);
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
    /// Task 20 extends `load_selected_meta` with the AI rows, so this stays
    /// the single refresh path for the inspector.
    pub(crate) fn refresh_selected_ai(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected {
            self.load_selected_meta(id);
            self.refresh_stale_preview(cx);
        }
        cx.notify();
    }

    pub(crate) fn ai_ready(&self) -> bool {
        self.ai.ready()
    }

    pub(crate) fn ai_error_label(&self, code: &str) -> String {
        ai_error_label_in(&vrc_photon_i18n::current_locale(), code)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AI_PLUGIN_ID, AiPluginAction, AiPluginCardState, AiPluginCardView, AiResponseOutcome,
        AiUiState, ModelDownloadView, ai_error_label_in, ai_error_text_in, ai_job_summary_label,
        ai_manifest_view, ai_plugin_card_state, ai_response_outcome, ai_tag_view,
        apply_install_event, backend_options, backend_selection_is_disabled_when_not_ready,
        collect_ai_snapshot, download_progress_bar_ratio, install_event_applies_to_ai,
        installation_row_id, is_active_installation, model_progress_ratio, needs_license_consent,
        people_label, place_label, variant_backend_label,
    };
    use vrc_photon_ai_protocol::{
        BackendInfo, ModelState, PluginInstallStatus, Probe, TaggingJobStatus,
    };
    use vrc_photon_core::ai::{PhotoAiTagView, PluginInstallation, PluginState, TaggingJob};
    use vrc_photon_core::db::Db;
    use vrc_photon_core::model::JobStatusCounts;
    use vrc_photon_ipc::{ErrorCode, Response};

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
        assert_eq!(ai.last_error.as_deref(), Some("sidecar_exited"));
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
        assert_eq!(
            ai.last_error.as_deref(),
            Some("signature verification failed")
        );

        let mut ai = AiUiState {
            install_busy: true,
            ..AiUiState::default()
        };
        assert!(
            apply_install_event(&mut ai, "", PluginInstallStatus::Running, None).is_none(),
            "a running event without an id is not attributable to the AI plugin"
        );
        assert!(
            apply_install_event(
                &mut ai,
                "another-plugin",
                PluginInstallStatus::Failed,
                Some("other failure")
            )
            .is_none(),
            "another plugin's failure must not clear the AI install state"
        );
        assert!(ai.install_busy);
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
        assert_eq!(
            ai_error_text_in("ja", "busy", "raw detail"),
            "別のAIタグ付け操作が実行中です"
        );
        assert_eq!(
            ai_error_text_in("ja", "unknown_code", "raw detail"),
            "raw detail"
        );
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
        assert_eq!(place_label(Some("indoor")), "屋内");
        assert_eq!(place_label(Some("outdoor")), "屋外");
        assert_eq!(place_label(Some("other")), "その他");
        assert_eq!(place_label(None), "—");
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
    fn ai_job_summary_label_lists_each_state() {
        assert_eq!(
            ai_job_summary_label(JobStatusCounts {
                waiting: 1,
                pending: 2,
                processing: 3,
                failed: 4,
            }),
            "待機 1 / 実行中 5 / 失敗 4"
        );
    }

    #[test]
    fn ai_tag_view_uses_the_requested_language_and_probability() {
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
        assert!(ja.ai_badge);

        let en = ai_tag_view(&tag, "en");
        assert_eq!(en.label, "Sea");
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
}
