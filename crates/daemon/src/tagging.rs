//! Cancellation plumbing and the four-stage tagging pipeline.
//!
//! `TaggingControl` is the registration table that lets `CancelTagging` reach
//! an in-flight sidecar request. `TaggingWorker` claims one durable job at a
//! time, snapshots the photo, runs presence → people_count → place_class →
//! place_tags, and stores the result only while the job is still processing.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use vrc_photon_ai_host::{AiHostError, CancelToken, SidecarClient};
use vrc_photon_ai_protocol::{
    ImageRef, Op, SidecarError, SidecarErrorCode, SidecarResult, TagBranch, TaggingErrorCode,
    TaggingJobStatus, TaggingStage,
};
use vrc_photon_core::CoreError;
use vrc_photon_core::ai::{MAX_TAGGING_RETRIES, NewPhotoAi, TaggingJob};
use vrc_photon_core::db::Db;
use vrc_photon_core::model::PhotoStatus;
use vrc_photon_core::settings::AppSettings;
use vrc_photon_core::tagging_image::{prepare_tagging_snapshot, tagging_cache_dir};
use vrc_photon_ipc::Event;

use crate::ipc_server::EventBus;
use crate::plugin::{AI_PLUGIN_ID, PluginError, PluginService};
use crate::signals::JobSignal;

/// Idle timeout used when the caller did not override it. Kept equal to the
/// default in [`AppSettings`], so an unmodified config follows the live
/// setting while tests can override it with a short duration.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Delay before retrying a job while the plugin is not ready. This avoids
/// repeatedly claiming the same pending job in a tight loop.
const NOT_READY_DELAY: Duration = Duration::from_secs(2);

/// Tracks one cancel token per running tagging job and wakes the worker.
pub(crate) struct TaggingControl {
    #[cfg(test)]
    signal: Arc<JobSignal>,
    tokens: Mutex<HashMap<String, CancelToken>>,
}

impl TaggingControl {
    pub(crate) fn new(signal: Arc<JobSignal>) -> Self {
        #[cfg(not(test))]
        let _ = signal;
        Self {
            #[cfg(test)]
            signal,
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a job before the worker issues its first sidecar request.
    /// Re-registering a job replaces its previous token.
    pub(crate) fn register(&self, job_id: &str) -> CancelToken {
        let token = CancelToken::default();
        lock_unpoisoned(&self.tokens).insert(job_id.to_string(), token.clone());
        token
    }

    /// Drops the token once the job reached a terminal state.
    pub(crate) fn unregister(&self, job_id: &str) {
        lock_unpoisoned(&self.tokens).remove(job_id);
    }

    /// Cancels the token for a running job. Returns false when the job has no
    /// registered token (for example a waiting job); the database row is the
    /// authoritative state either way.
    pub(crate) fn cancel(&self, job_id: &str) -> bool {
        match lock_unpoisoned(&self.tokens).get(job_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Cancels every registered job token. Called when the daemon stops so an
    /// in-flight sidecar request is interrupted instead of delaying shutdown.
    pub(crate) fn cancel_all(&self) {
        for token in lock_unpoisoned(&self.tokens).values() {
            token.cancel();
        }
    }

    /// Nudges the tagging worker.
    #[cfg(test)]
    pub(crate) fn notify(&self) {
        self.signal.notify();
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Retry delays, idle timeout, and poll interval for [`TaggingWorker`].
#[derive(Debug, Clone)]
pub(crate) struct TaggingWorkerConfig {
    pub retry_delays: [Duration; 2],
    pub idle_timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for TaggingWorkerConfig {
    fn default() -> Self {
        Self {
            retry_delays: [Duration::from_secs(5), Duration::from_secs(30)],
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            poll_interval: Duration::from_millis(400),
        }
    }
}

/// Runs tagging jobs one at a time against the daemon-owned sidecar.
pub(crate) struct TaggingWorker {
    db: Arc<Mutex<Db>>,
    ai_data_dir: PathBuf,
    plugin: PluginService,
    events: EventBus,
    running: Arc<AtomicBool>,
    /// Pause gate read before claiming the next job. A job already running
    /// finishes; pause only stops new dequeues.
    paused: Arc<AtomicBool>,
    signal: Arc<JobSignal>,
    control: Arc<TaggingControl>,
    config: TaggingWorkerConfig,
    last_activity: Mutex<Instant>,
}

impl TaggingWorker {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        db: Arc<Mutex<Db>>,
        ai_data_dir: PathBuf,
        plugin: PluginService,
        events: EventBus,
        running: Arc<AtomicBool>,
        signal: Arc<JobSignal>,
        control: Arc<TaggingControl>,
        config: TaggingWorkerConfig,
    ) -> Self {
        Self {
            db,
            ai_data_dir,
            plugin,
            events,
            running,
            paused: Arc::new(AtomicBool::new(false)),
            signal,
            control,
            config,
            last_activity: Mutex::new(Instant::now()),
        }
    }

    /// Binds the shared pause gate. The default is unpaused so test workers
    /// and maintenance callers keep their existing behavior.
    pub(crate) fn with_paused(mut self, paused: Arc<AtomicBool>) -> Self {
        self.paused = paused;
        self
    }

    /// Claims and processes one pending job. Returns `false` when the queue is
    /// empty (and performs the idle sidecar check) or when the daemon paused.
    pub(crate) fn process_once(&self) -> Result<bool> {
        if self.is_stopping() || self.is_paused() {
            return Ok(false);
        }
        let job = self.db().next_pending_tagging_job()?;
        let Some(job) = job else {
            self.maybe_stop_idle_sidecar()?;
            return Ok(false);
        };
        if self.is_stopping() {
            // Leave the claimed row `processing`; the next owner startup
            // resumes it instead of losing the retry.
            return Ok(false);
        }
        self.process_job(job)?;
        *lock_unpoisoned(&self.last_activity) = Instant::now();
        Ok(true)
    }

    fn is_stopping(&self) -> bool {
        !self.running.load(Ordering::Relaxed)
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Waits for a wakeup or the poll interval before the next dequeue attempt.
    fn wait_for_next_poll(&self, seen: &mut u64) {
        self.signal.wait(seen, self.config.poll_interval);
    }

    fn db(&self) -> MutexGuard<'_, Db> {
        lock_unpoisoned(&self.db)
    }

    fn process_job(&self, first_job: TaggingJob) -> Result<()> {
        let cancel = self.control.register(&first_job.id);
        let _cleanup = JobCleanup {
            worker: self,
            job_id: first_job.id.clone(),
        };
        let mut job = first_job;
        self.publish_processing(&job, 0.05, None);

        loop {
            if self.is_stopping() {
                break;
            }
            let result = self.run_pipeline(&job, &cancel);
            if self.is_stopping() {
                // Do not write a terminal status while stopping. The job stays
                // `processing` and `resume_interrupted_tagging_jobs` recovers
                // it on the next owner startup.
                break;
            }
            match result {
                Ok(Completion::Completed) => break,
                Ok(Completion::Cancelled) => {
                    self.finish_cancelled(&job)?;
                    break;
                }
                Ok(Completion::Deferred) => {
                    self.finish_deferred(&job)?;
                    break;
                }
                Err(JobFailure::Cancelled) => {
                    self.finish_cancelled(&job)?;
                    break;
                }
                Err(JobFailure::Pipeline {
                    code: TaggingErrorCode::PluginNotReady,
                    ..
                }) => {
                    self.finish_deferred(&job)?;
                    break;
                }
                Err(JobFailure::Pipeline { code, retryable }) => {
                    if self.is_cancelled(&job, &cancel) {
                        self.finish_cancelled(&job)?;
                        break;
                    }
                    if !retryable || job.retry_count >= MAX_TAGGING_RETRIES {
                        self.db().fail_tagging_job(&job.id, code)?;
                        self.publish_failed(&job, code);
                        break;
                    }
                    // Keep the claim while backing off. Releasing the job here
                    // would let another pending job (for example one released
                    // from the backlog) be claimed under this job's token and
                    // cleanup, and a daemon restart would lose the retry.
                    if !self.db().bump_tagging_retry(&job.id)? {
                        if self.is_cancelled(&job, &cancel) {
                            self.finish_cancelled(&job)?;
                        } else {
                            self.db().fail_tagging_job(&job.id, code)?;
                            self.publish_failed(&job, code);
                        }
                        break;
                    }
                    let delay = self.config.retry_delays[job.retry_count as usize];
                    job.retry_count += 1;
                    match self.wait_for_retry(delay, &job, &cancel) {
                        RetryWait::Cancelled => {
                            self.finish_cancelled(&job)?;
                            break;
                        }
                        RetryWait::Stopping => break,
                        RetryWait::Elapsed => {}
                    }
                    self.publish_processing(&job, 0.05, None);
                }
                Err(JobFailure::Unexpected(error)) => {
                    tracing::error!(
                        %error,
                        job_id = %job.id,
                        "tagging pipeline failed unexpectedly"
                    );
                    self.db()
                        .fail_tagging_job(&job.id, TaggingErrorCode::Internal)?;
                    self.publish_failed(&job, TaggingErrorCode::Internal);
                    break;
                }
            }
        }

        Ok(())
    }

    fn run_pipeline(
        &self,
        job: &TaggingJob,
        cancel: &CancelToken,
    ) -> std::result::Result<Completion, JobFailure> {
        if self.is_stopping() {
            return Ok(Completion::Cancelled);
        }
        if self.is_cancelled(job, cancel) {
            return Ok(Completion::Cancelled);
        }
        if !self.plugin.is_ready(AI_PLUGIN_ID) {
            return Ok(Completion::Deferred);
        }

        let photo = self.db().photo_by_id(job.photo_id)?;
        let Some(photo) = photo else {
            return Ok(Completion::Cancelled);
        };
        if !matches!(photo.status, PhotoStatus::Present | PhotoStatus::Replaced) {
            return Ok(Completion::Cancelled);
        }

        let cache_dir = tagging_cache_dir(&self.ai_data_dir);
        let snapshot =
            prepare_tagging_snapshot(Path::new(photo.effective_path()), &cache_dir, &job.id)
                .map_err(map_snapshot_error)?;
        let image = ImageRef {
            path: snapshot.path.to_string_lossy().into_owned(),
            mtime_ms: snapshot.mtime_ms,
            bytes: snapshot.bytes,
        };

        if self.is_cancelled(job, cancel) {
            return Ok(Completion::Cancelled);
        }

        let client = self.plugin.sidecar().map_err(map_plugin_error)?;
        self.plugin
            .sync_catalog_from_hello(client.hello())
            .map_err(map_plugin_error)?;
        let hello = client.hello().clone();

        // The user may change the presence threshold between jobs; read it
        // when this job starts rather than caching it in the worker.
        let presence_threshold = {
            let db = self.db();
            let settings = AppSettings::load(&db)?;
            settings
                .ai_presence_threshold
                .unwrap_or(hello.defaults.presence_threshold)
        };

        self.record_progress(job, 0.1, TaggingStage::Presence);
        if self.is_cancelled(job, cancel) {
            return Ok(Completion::Cancelled);
        }
        let result = self.request(&client, Op::Presence, &image, None, cancel)?;
        let SidecarResult::Presence { present, p_yes } = result else {
            return Err(JobFailure::non_retryable(TaggingErrorCode::SidecarProtocol));
        };
        if self.is_cancelled(job, cancel) {
            return Ok(Completion::Cancelled);
        }
        let character_present = present && p_yes >= presence_threshold;

        let (people_min, people_max, people_median) = if character_present {
            self.record_progress(job, 0.3, TaggingStage::PeopleCount);
            if self.is_cancelled(job, cancel) {
                return Ok(Completion::Cancelled);
            }
            let result = self.request(&client, Op::PeopleCount, &image, None, cancel)?;
            let SidecarResult::PeopleCount { count, interval } = result else {
                return Err(JobFailure::non_retryable(TaggingErrorCode::SidecarProtocol));
            };
            if self.is_cancelled(job, cancel) {
                return Ok(Completion::Cancelled);
            }
            (Some(interval[0]), Some(interval[1]), Some(count))
        } else {
            // A photo without characters has no people to count.
            (Some(0), Some(0), Some(0))
        };

        self.record_progress(job, 0.5, TaggingStage::PlaceClass);
        if self.is_cancelled(job, cancel) {
            return Ok(Completion::Cancelled);
        }
        let result = self.request(&client, Op::PlaceClass, &image, None, cancel)?;
        let SidecarResult::PlaceClass {
            branch,
            probabilities,
        } = result
        else {
            return Err(JobFailure::non_retryable(TaggingErrorCode::SidecarProtocol));
        };
        if self.is_cancelled(job, cancel) {
            return Ok(Completion::Cancelled);
        }
        let place_branch = branch.as_str().to_string();
        let place_probabilities_json = serde_json::to_string(&probabilities)
            .map_err(|error| JobFailure::Unexpected(error.into()))?;

        // `other` is not a tag branch: the host always asks for outdoor tags
        // when the first stage could not decide between indoor and outdoor.
        let tag_branch = match branch {
            vrc_photon_ai_protocol::PlaceBranch::Indoor => TagBranch::Indoor,
            vrc_photon_ai_protocol::PlaceBranch::Outdoor
            | vrc_photon_ai_protocol::PlaceBranch::Other => TagBranch::Outdoor,
        };
        self.record_progress(job, 0.7, TaggingStage::PlaceTags);
        if self.is_cancelled(job, cancel) {
            return Ok(Completion::Cancelled);
        }
        let result = self.request(&client, Op::PlaceTags, &image, Some(tag_branch), cancel)?;
        let SidecarResult::PlaceTags {
            branch: returned_branch,
            tags,
        } = result
        else {
            return Err(JobFailure::non_retryable(TaggingErrorCode::SidecarProtocol));
        };
        if returned_branch != tag_branch {
            return Err(JobFailure::non_retryable(TaggingErrorCode::SidecarProtocol));
        }
        if self.is_cancelled(job, cancel) {
            return Ok(Completion::Cancelled);
        }
        let tags = tags.into_iter().map(|tag| (tag.key, tag.p_yes)).collect();

        self.record_progress(job, 0.9, TaggingStage::Write);
        let result = NewPhotoAi {
            variant: hello.variant.clone(),
            sidecar_version: hello.sidecar_version.clone(),
            model_version: format!("{}-{}", hello.model.id, hello.model.quantization),
            model_revision: hello.model.revision.clone(),
            catalog_version: hello.catalog.version.clone(),
            source: if photo.display_derivative_id.is_some() {
                "derivative".to_string()
            } else {
                "original".to_string()
            },
            presence_threshold,
            character_present,
            presence_probability: p_yes,
            people_min,
            people_max,
            people_median,
            place_branch: Some(place_branch),
            place_probabilities_json: Some(place_probabilities_json),
            tags,
        };

        // The write transaction re-checks that the job is still processing, so
        // a cancel that raced this stage never leaves a late result behind.
        let saved = self.db().save_photo_ai_for_job(&job.id, &result)?;
        if !saved {
            return Ok(Completion::Cancelled);
        }
        self.db().complete_tagging_job(&job.id)?;
        let status = self.db().tagging_job(&job.id)?.map(|job| job.status);
        if status != Some(TaggingJobStatus::Completed) {
            // A cancel landed between the write commit and the completion.
            return Ok(Completion::Cancelled);
        }
        self.publish(job, TaggingJobStatus::Completed, 1.0, None, None);
        Ok(Completion::Completed)
    }

    fn request(
        &self,
        client: &SidecarClient,
        op: Op,
        image: &ImageRef,
        branch: Option<TagBranch>,
        cancel: &CancelToken,
    ) -> std::result::Result<SidecarResult, JobFailure> {
        client
            .request(op, image, branch, cancel)
            .map_err(map_ai_host_error)
    }

    fn is_cancelled(&self, job: &TaggingJob, cancel: &CancelToken) -> bool {
        if cancel.is_cancelled() {
            return true;
        }
        match self.db().tagging_job(&job.id) {
            Ok(Some(current)) => current.status == TaggingJobStatus::Cancelled,
            Ok(None) => true,
            Err(error) => {
                tracing::warn!(%error, job_id = %job.id, "cannot read tagging job state");
                false
            }
        }
    }

    /// Sleeps through the retry backoff while polling for cancellation or a
    /// daemon stop. The job stays `processing` for the whole delay.
    fn wait_for_retry(&self, delay: Duration, job: &TaggingJob, cancel: &CancelToken) -> RetryWait {
        let deadline = Instant::now() + delay;
        loop {
            if self.is_stopping() {
                return RetryWait::Stopping;
            }
            if self.is_cancelled(job, cancel) {
                return RetryWait::Cancelled;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return RetryWait::Elapsed;
            }
            thread::sleep(remaining.min(Duration::from_millis(25)));
        }
    }

    /// Waits for `duration`, but returns immediately once shutdown starts.
    fn sleep_until_stopped(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while !self.is_stopping() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            thread::sleep(remaining.min(Duration::from_millis(25)));
        }
    }

    fn finish_cancelled(&self, job: &TaggingJob) -> Result<()> {
        self.db().cancel_tagging_job(&job.id)?;
        self.publish(job, TaggingJobStatus::Cancelled, 0.0, None, None);
        Ok(())
    }

    fn finish_deferred(&self, job: &TaggingJob) -> Result<()> {
        {
            let db = self.db();
            db.defer_tagging_job(&job.id, TaggingErrorCode::PluginNotReady)?;
        }
        let status = self.db().tagging_job(&job.id)?.map(|job| job.status);
        if status == Some(TaggingJobStatus::Cancelled) {
            self.publish(job, TaggingJobStatus::Cancelled, 0.0, None, None);
        } else {
            self.publish(
                job,
                TaggingJobStatus::Pending,
                0.0,
                None,
                Some(TaggingErrorCode::PluginNotReady.as_str()),
            );
        }
        self.sleep_until_stopped(NOT_READY_DELAY);
        Ok(())
    }

    fn publish_failed(&self, job: &TaggingJob, code: TaggingErrorCode) {
        let status = self
            .db()
            .tagging_job(&job.id)
            .ok()
            .flatten()
            .map(|job| job.status);
        if status == Some(TaggingJobStatus::Cancelled) {
            self.publish(job, TaggingJobStatus::Cancelled, 0.0, None, None);
        } else {
            self.publish(
                job,
                TaggingJobStatus::Failed,
                0.0,
                None,
                Some(code.as_str()),
            );
        }
    }

    fn record_progress(&self, job: &TaggingJob, progress: f64, stage: TaggingStage) {
        if let Err(error) = self.db().update_tagging_progress(&job.id, progress, stage) {
            tracing::debug!(
                %error,
                job_id = %job.id,
                "cannot record tagging progress"
            );
        }
        self.publish_processing(job, progress, Some(stage));
    }

    fn publish_processing(&self, job: &TaggingJob, progress: f64, stage: Option<TaggingStage>) {
        self.publish(job, TaggingJobStatus::Processing, progress, stage, None);
    }

    fn publish(
        &self,
        job: &TaggingJob,
        status: TaggingJobStatus,
        progress: f64,
        stage: Option<TaggingStage>,
        error: Option<&str>,
    ) {
        self.events.publish(Event::TaggingProgress {
            job_id: job.id.clone(),
            photo_id: job.photo_id,
            status,
            progress,
            stage,
            error: error.map(str::to_string),
        });
    }

    fn maybe_stop_idle_sidecar(&self) -> Result<()> {
        let last = *lock_unpoisoned(&self.last_activity);
        let timeout = if self.config.idle_timeout == DEFAULT_IDLE_TIMEOUT {
            let db = self.db();
            Duration::from_secs(AppSettings::load(&db)?.ai_tagging_idle_timeout_sec)
        } else {
            self.config.idle_timeout
        };
        if last.elapsed() >= timeout {
            self.plugin.stop_sidecar();
            *lock_unpoisoned(&self.last_activity) = Instant::now();
        }
        Ok(())
    }

    fn delete_snapshot(&self, job_id: &str) {
        let cache_dir = tagging_cache_dir(&self.ai_data_dir);
        for name in [format!("{job_id}.jpg"), format!("{job_id}.jpg.tmp")] {
            match fs::remove_file(cache_dir.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::debug!(%error, job_id, "cannot remove tagging snapshot");
                }
            }
        }
    }
}

/// Releases the cancel token and the temporary snapshot when a job reaches a
/// terminal state, including early `?` returns from database errors.
struct JobCleanup<'a> {
    worker: &'a TaggingWorker,
    job_id: String,
}

impl Drop for JobCleanup<'_> {
    fn drop(&mut self) {
        self.worker.control.unregister(&self.job_id);
        self.worker.delete_snapshot(&self.job_id);
    }
}

/// Runs the tagging loop until the daemon stops. The supervisor in
/// [`crate::supervisor`] wraps this in `catch_unwind` and restarts it, so a
/// panic leaves the job recoverable as `processing` and the daemon alive.
pub(crate) fn run_worker_loop(worker: &TaggingWorker) {
    let mut seen = 0;
    while !worker.is_stopping() {
        if worker.is_paused() {
            worker.wait_for_next_poll(&mut seen);
            continue;
        }
        match worker.process_once() {
            Ok(true) => {}
            Ok(false) => {
                worker.wait_for_next_poll(&mut seen);
            }
            Err(error) => {
                tracing::error!(%error, "tagging worker failed");
                worker.wait_for_next_poll(&mut seen);
            }
        }
    }
}

enum RetryWait {
    Elapsed,
    Cancelled,
    Stopping,
}

enum Completion {
    Completed,
    Cancelled,
    Deferred,
}

enum JobFailure {
    Cancelled,
    Pipeline {
        code: TaggingErrorCode,
        retryable: bool,
    },
    Unexpected(anyhow::Error),
}

impl JobFailure {
    fn non_retryable(code: TaggingErrorCode) -> Self {
        Self::Pipeline {
            code,
            retryable: false,
        }
    }

    fn retryable(code: TaggingErrorCode) -> Self {
        Self::Pipeline {
            code,
            retryable: true,
        }
    }
}

impl From<CoreError> for JobFailure {
    fn from(error: CoreError) -> Self {
        Self::Unexpected(error.into())
    }
}

fn map_snapshot_error(error: CoreError) -> JobFailure {
    match error {
        CoreError::NotFound(_) => JobFailure::non_retryable(TaggingErrorCode::ImageMissing),
        CoreError::Invalid(_) => JobFailure::non_retryable(TaggingErrorCode::ImageDecodeFailed),
        other => JobFailure::Unexpected(other.into()),
    }
}

fn map_ai_host_error(error: AiHostError) -> JobFailure {
    match error {
        AiHostError::Cancelled => JobFailure::Cancelled,
        AiHostError::SidecarRejected(error) | AiHostError::SidecarFatal(error) => {
            map_sidecar_error(error)
        }
        AiHostError::SidecarExited => JobFailure::retryable(TaggingErrorCode::SidecarExited),
        AiHostError::SidecarTimeout => JobFailure::retryable(TaggingErrorCode::SidecarTimeout),
        AiHostError::SidecarProtocol(_)
        | AiHostError::UnsupportedProtocol(_)
        | AiHostError::SidecarIdentity(_) => {
            JobFailure::non_retryable(TaggingErrorCode::SidecarProtocol)
        }
        AiHostError::SidecarBackendMismatch { .. } => {
            JobFailure::non_retryable(TaggingErrorCode::BackendUnavailable)
        }
        other => JobFailure::Unexpected(other.into()),
    }
}

fn map_sidecar_error(error: SidecarError) -> JobFailure {
    let code = match error.code {
        SidecarErrorCode::ProtocolError => TaggingErrorCode::SidecarProtocol,
        SidecarErrorCode::ImageMissing => TaggingErrorCode::ImageMissing,
        SidecarErrorCode::ImageDecodeFailed => TaggingErrorCode::ImageDecodeFailed,
        SidecarErrorCode::ModelLoadFailed => TaggingErrorCode::ModelMissing,
        SidecarErrorCode::BackendUnavailable => TaggingErrorCode::BackendUnavailable,
        SidecarErrorCode::Busy => TaggingErrorCode::Busy,
        SidecarErrorCode::OutOfMemory => TaggingErrorCode::OutOfMemory,
        SidecarErrorCode::Cancelled => return JobFailure::Cancelled,
        SidecarErrorCode::Internal => TaggingErrorCode::Internal,
    };
    JobFailure::Pipeline {
        code,
        retryable: error.retryable,
    }
}

fn map_plugin_error(error: PluginError) -> JobFailure {
    match error {
        PluginError::Host(error) => map_ai_host_error(error),
        PluginError::NotReady(_) | PluginError::NotFound(_) => {
            JobFailure::non_retryable(TaggingErrorCode::PluginNotReady)
        }
        PluginError::Busy(_) => JobFailure::retryable(TaggingErrorCode::Busy),
        PluginError::Core(error) => JobFailure::Unexpected(error.into()),
        PluginError::Json(error) => JobFailure::Unexpected(error.into()),
        PluginError::Io(error) => JobFailure::Unexpected(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::Receiver;
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;
    use vrc_photon_ai_host::test_support::{
        TEST_ENTRYPOINT, TestEntry, TestKey, TestPackage, sample_manifest, sha256_hex,
    };
    use vrc_photon_ai_host::{ManifestFile, UreqModelClient, model_dir};
    use vrc_photon_ai_protocol::{
        ModelState, PlaceProbabilities, TaggingErrorCode, TaggingJobStatus, TaggingStage,
    };

    use crate::test_support::mock_sidecar;
    use vrc_photon_core::db::Db;
    use vrc_photon_core::model::NewPhoto;
    use vrc_photon_core::tagging_image::tagging_cache_dir;
    use vrc_photon_ipc::Event;

    use crate::ipc_server::EventBus;
    use crate::plugin::{AI_PLUGIN_ID, PluginService};

    const CATALOG_JSON: &str = r#"{
  "version": "2026-11-03.1",
  "outdoor": [
    {
      "key": "sea",
      "name": "海",
      "name_en": "Sea",
      "description": "海が見える屋外",
      "default_threshold": 0.5
    }
  ],
  "indoor": [
    {
      "key": "japanese_room",
      "name": "和室",
      "name_en": "Japanese room",
      "description": "畳・障子のある和室",
      "default_threshold": 0.5
    }
  ]
}"#;
    const MODEL_BYTES: &[u8] = b"model";
    const MMPROJ_BYTES: &[u8] = b"mmproj";
    const MODEL_FILE: &str = "d1-2B-Q8_0.gguf";
    const MMPROJ_FILE: &str = "mmproj-d1-2B-Q8_0.gguf";

    #[test]
    fn paused_worker_does_not_claim_a_pending_job() {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        {
            let db = db.lock().unwrap();
            let root = db.add_root("/photos", "reference").unwrap();
            let photo_id = db
                .upsert_photo(&NewPhoto {
                    root_id: root,
                    rel_path: "a.png".to_string(),
                    abs_path: "/photos/a.png".to_string(),
                    file_name: "a.png".to_string(),
                    captured_at_utc: 1_700_000_000,
                    captured_at_local: "2026-10-21 21:14:03".to_string(),
                    tz_offset_sec: 9 * 3600,
                    width: 16,
                    height: 10,
                    file_size: 10,
                    mtime: 1_700_000_000,
                    blake3: None,
                })
                .unwrap();
            db.enqueue_tagging_jobs(&[photo_id], AI_PLUGIN_ID, TaggingJobStatus::Pending)
                .unwrap();
        }
        let events = EventBus::default();
        let data_dir = PathBuf::from("/tmp/vrc-photon-tagging-pause-test");
        let plugin = PluginService::new(
            db.clone(),
            data_dir.clone(),
            events.clone(),
            Vec::new(),
            Arc::new(UreqModelClient::new()),
        );
        let signal = Arc::new(JobSignal::default());
        let worker = TaggingWorker::new(
            db.clone(),
            data_dir,
            plugin,
            events,
            Arc::new(AtomicBool::new(true)),
            signal.clone(),
            Arc::new(TaggingControl::new(signal)),
            TaggingWorkerConfig::default(),
        )
        .with_paused(Arc::new(AtomicBool::new(true)));

        assert!(!worker.process_once().unwrap());
        let counts = db.lock().unwrap().tagging_job_counts(AI_PLUGIN_ID).unwrap();
        assert_eq!(counts.pending, 1, "pause must not claim the queued job");
        assert_eq!(counts.processing, 0);
    }

    #[test]
    fn cancel_only_affects_registered_jobs() {
        let signal = Arc::new(JobSignal::default());
        let control = TaggingControl::new(signal);
        assert!(!control.cancel("missing"));

        let token = control.register("job-1");
        assert!(!token.is_cancelled());
        assert!(control.cancel("job-1"));
        assert!(token.is_cancelled());

        control.unregister("job-1");
        assert!(!control.cancel("job-1"));
    }

    #[test]
    fn cancel_all_cancels_every_registered_token() {
        let signal = Arc::new(JobSignal::default());
        let control = TaggingControl::new(signal);
        let first = control.register("job-1");
        let second = control.register("job-2");
        assert!(!first.is_cancelled());
        assert!(!second.is_cancelled());

        control.cancel_all();

        assert!(first.is_cancelled());
        assert!(second.is_cancelled());
        control.unregister("job-1");
        assert!(!control.cancel("job-1"));
    }

    #[test]
    fn notify_wakes_the_shared_signal() {
        let signal = Arc::new(JobSignal::default());
        let control = TaggingControl::new(signal.clone());
        let mut seen = 0;
        control.notify();
        assert!(signal.wait(&mut seen, Duration::from_millis(1)));
    }

    #[derive(Clone, Copy)]
    struct PresenceResponse {
        present: bool,
        p_yes: f32,
    }

    fn presence_response(present: bool, p_yes: f32) -> PresenceResponse {
        PresenceResponse { present, p_yes }
    }

    #[derive(Clone, Copy, Default)]
    struct PeopleResponse {
        count: u32,
        interval: [u32; 2],
    }

    fn people_response(count: u32, interval: [u32; 2]) -> PeopleResponse {
        PeopleResponse { count, interval }
    }

    fn test_config() -> TaggingWorkerConfig {
        TaggingWorkerConfig {
            retry_delays: [Duration::from_millis(1), Duration::from_millis(1)],
            idle_timeout: Duration::from_secs(600),
            poll_interval: Duration::from_millis(5),
        }
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn wrapper_entrypoint(
        mock: &Path,
        mode: &str,
        presence: PresenceResponse,
        people: PeopleResponse,
        place: &str,
        delay_ms: Option<u64>,
    ) -> Vec<u8> {
        let delay = delay_ms
            .map(|delay| format!("export MOCK_SIDECAR_DELAY_MS={delay}\n"))
            .unwrap_or_default();
        format!(
            "#!/bin/sh\n\
             export MOCK_SIDECAR_MODE={mode}\n\
             export MOCK_SIDECAR_PRESENT={present}\n\
             export MOCK_SIDECAR_PRESENCE_P={p_yes}\n\
             export MOCK_SIDECAR_PEOPLE_COUNT={count}\n\
             export MOCK_SIDECAR_PEOPLE_INTERVAL='{low},{high}'\n\
             export MOCK_SIDECAR_PLACE_BRANCH={place}\n\
             {delay}\
             exec {mock} \"$@\"\n",
            present = presence.present,
            p_yes = presence.p_yes,
            count = people.count,
            low = people.interval[0],
            high = people.interval[1],
            mock = shell_quote(&mock.to_string_lossy()),
        )
        .into_bytes()
    }

    /// A probe that stays busy long enough for a concurrent operation test.
    fn delayed_probe_entrypoint(mock: &Path) -> Vec<u8> {
        format!(
            "#!/bin/sh\nsleep 1\nexec {mock} \"$@\"\n",
            mock = shell_quote(&mock.to_string_lossy()),
        )
        .into_bytes()
    }

    fn package_with_entrypoint(key: &TestKey, entrypoint: Vec<u8>) -> TestPackage {
        let entries = vec![
            TestEntry::with_mode(TEST_ENTRYPOINT, entrypoint, 0o755),
            TestEntry::new("catalog.json", CATALOG_JSON),
            TestEntry::new("calibration.json", b"{}"),
        ];
        let files = entries
            .iter()
            .map(|entry| ManifestFile {
                path: entry.path.clone(),
                size: entry.data.len() as u64,
                sha256: sha256_hex(&entry.data),
            })
            .collect();

        let mut package = TestPackage::with_key(key.clone());
        package.manifest_mut().files = files;
        *package.entries_mut() = entries;
        package
    }

    fn wait_for_install(rx: &Receiver<Event>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(Event::PluginInstallProgress {
                    status: vrc_photon_ipc::PluginInstallStatus::Installed,
                    ..
                }) => return,
                Ok(Event::PluginInstallProgress {
                    status: vrc_photon_ipc::PluginInstallStatus::Failed,
                    error,
                    ..
                }) => panic!("plugin install failed: {error:?}"),
                Ok(_) => {}
                Err(error) => panic!("timed out waiting for the plugin install: {error}"),
            }
        }
    }

    fn make_model_ready(db: &Arc<Mutex<Db>>, data_dir: &Path) {
        let manifest = sample_manifest();
        let directory = model_dir(data_dir, AI_PLUGIN_ID, &manifest.model.revision);
        fs::create_dir_all(&directory).expect("create the model directory");
        fs::write(directory.join(MODEL_FILE), MODEL_BYTES).expect("write the model");
        fs::write(directory.join(MMPROJ_FILE), MMPROJ_BYTES).expect("write the mmproj");
        db.lock()
            .unwrap()
            .set_plugin_model_state(
                AI_PLUGIN_ID,
                ModelState::Ready,
                Some(&manifest.model.revision),
                11,
                Some(11),
                None,
            )
            .expect("store the ready model state");
    }

    fn add_photo(db: &Arc<Mutex<Db>>, photos_dir: &Path, name: &str) -> i64 {
        let path = photos_dir.join(name);
        image::RgbaImage::from_pixel(32, 24, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .expect("write the test photo");
        let metadata = fs::metadata(&path).expect("read the photo metadata");
        let db = db.lock().unwrap();
        let root = db
            .add_root(&photos_dir.to_string_lossy(), "reference")
            .expect("add the photo root");
        db.upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: name.to_string(),
            abs_path: path.to_string_lossy().into_owned(),
            file_name: name.to_string(),
            captured_at_utc: 1_700_000_000,
            captured_at_local: "2026-11-04 12:00:00".to_string(),
            tz_offset_sec: 9 * 3600,
            width: 32,
            height: 24,
            file_size: metadata.len(),
            mtime: 1_700_000_000,
            blake3: None,
        })
        .expect("insert the test photo")
    }

    struct TaggingTestContext {
        _dir: TempDir,
        db: Arc<Mutex<Db>>,
        plugin: PluginService,
        events: EventBus,
        control: Arc<TaggingControl>,
        running: Arc<AtomicBool>,
        worker: Arc<TaggingWorker>,
        ai_data_dir: PathBuf,
        photos_dir: PathBuf,
        photo_id: i64,
        key: TestKey,
        mock: PathBuf,
    }

    impl TaggingTestContext {
        fn new(mode: &str, presence: PresenceResponse) -> Self {
            Self::with_scenario(
                mode,
                presence,
                PeopleResponse::default(),
                "other",
                None,
                true,
                test_config(),
            )
        }

        fn with_scenario(
            mode: &str,
            presence: PresenceResponse,
            people: PeopleResponse,
            place: &str,
            delay_ms: Option<u64>,
            ready: bool,
            config: TaggingWorkerConfig,
        ) -> Self {
            let dir = tempfile::tempdir().expect("create the test directory");
            let mock = mock_sidecar();
            let key = TestKey::generate();
            let db = Arc::new(Mutex::new(
                Db::open(dir.path().join("library.db")).expect("open the test database"),
            ));
            let events = EventBus::default();
            let ai_data_dir = dir.path().join("data");
            let service = PluginService::new(
                db.clone(),
                ai_data_dir.clone(),
                events.clone(),
                vec![key.public_key()],
                Arc::new(UreqModelClient::new()),
            );

            if ready {
                let entrypoint = wrapper_entrypoint(&mock, mode, presence, people, place, delay_ms);
                let package = package_with_entrypoint(&key, entrypoint);
                let zip = dir.path().join("plugin.zip");
                package.write_zip(&zip).expect("write the test package");
                let install_rx = events.subscribe();
                service
                    .install_from_zip(&zip)
                    .expect("accept the plugin install");
                wait_for_install(&install_rx);
                make_model_ready(&db, &ai_data_dir);
            }

            let photos_dir = dir.path().join("photos");
            fs::create_dir_all(&photos_dir).expect("create the photo directory");
            let photo_id = add_photo(&db, &photos_dir, "first.png");

            let signal = Arc::new(JobSignal::default());
            let control = Arc::new(TaggingControl::new(signal.clone()));
            let running = Arc::new(AtomicBool::new(true));
            let worker = Arc::new(TaggingWorker::new(
                db.clone(),
                ai_data_dir.clone(),
                service.clone(),
                events.clone(),
                running.clone(),
                signal,
                control.clone(),
                config,
            ));

            Self {
                _dir: dir,
                db,
                plugin: service,
                events,
                control,
                running,
                worker,
                ai_data_dir,
                photos_dir,
                photo_id,
                key,
                mock,
            }
        }

        fn db(&self) -> MutexGuard<'_, Db> {
            self.db
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn enqueue_one(&self) -> String {
            self.enqueue(self.photo_id)
        }

        fn enqueue(&self, photo_id: i64) -> String {
            self.db()
                .enqueue_tagging_jobs(&[photo_id], AI_PLUGIN_ID, TaggingJobStatus::Pending)
                .expect("enqueue the tagging job");
            self.db()
                .tagging_jobs_for_photo(photo_id)
                .expect("read the tagging jobs")
                .into_iter()
                .find(|job| job.status == TaggingJobStatus::Pending)
                .expect("the job is pending")
                .id
        }

        fn add_photo(&self, name: &str) -> i64 {
            add_photo(&self.db, &self.photos_dir, name)
        }

        fn snapshot_path(&self, job_id: &str) -> PathBuf {
            tagging_cache_dir(&self.ai_data_dir).join(format!("{job_id}.jpg"))
        }
    }

    fn progress_events(
        rx: &Receiver<Event>,
        job_id: &str,
    ) -> Vec<(TaggingJobStatus, f64, Option<TaggingStage>)> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Event::TaggingProgress {
                job_id: id,
                status,
                progress,
                stage,
                ..
            } = event
                && id == job_id
            {
                events.push((status, progress, stage));
            }
        }
        events
    }

    fn wait_for_presence_stage(rx: &Receiver<Event>, job_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(Event::TaggingProgress {
                    job_id: id,
                    progress,
                    ..
                }) if id == job_id && (progress - 0.1).abs() < 1e-9 => return,
                Ok(_) => {}
                Err(error) => panic!("timed out waiting for the presence stage: {error}"),
            }
        }
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn presence_false_skips_count_and_still_writes_place() {
        // A nonzero people response proves the count operation was skipped
        // rather than returning the mock's default 0/0.
        let ctx = TaggingTestContext::with_scenario(
            "normal",
            presence_response(false, 0.02),
            people_response(3, [2, 4]),
            "other",
            None,
            true,
            test_config(),
        );
        ctx.enqueue_one();
        assert!(ctx.worker.process_once().unwrap());
        let ai = ctx.db().photo_ai(ctx.photo_id).unwrap().unwrap();
        assert!(!ai.character_present);
        assert_eq!((ai.people_min, ai.people_max), (Some(0), Some(0)));
        assert_eq!(ai.people_median, Some(0));
        assert_eq!(ai.place_branch.as_deref(), Some("other"));
        let tags = ctx.db().photo_ai_tags(ctx.photo_id, true).unwrap();
        assert!(tags.iter().any(|tag| tag.tag_key == "sea")); // other -> outdoor
    }

    #[test]
    fn presence_true_runs_all_four_ops_and_publishes_progress() {
        let ctx = TaggingTestContext::with_scenario(
            "normal",
            presence_response(true, 0.99),
            people_response(3, [2, 4]),
            "other",
            None,
            true,
            test_config(),
        );
        let job_id = ctx.enqueue_one();
        let events = ctx.events.subscribe();
        assert!(ctx.worker.process_once().unwrap());

        let ai = ctx.db().photo_ai(ctx.photo_id).unwrap().unwrap();
        assert!(ai.character_present);
        assert!((ai.presence_probability - 0.99).abs() < 1e-6);
        assert_eq!(
            (ai.people_min, ai.people_max, ai.people_median),
            (Some(2), Some(4), Some(3))
        );
        assert_eq!(ai.place_branch.as_deref(), Some("other"));
        assert_eq!(ai.source, "original");
        assert_eq!(ai.variant, "linux-vulkan");
        assert_eq!(ai.sidecar_version, "1.0.0-mock");
        assert_eq!(ai.model_revision, "mock");
        assert_eq!(ai.catalog_version, "2026-11-03.1");
        assert!((ai.presence_threshold - 0.9).abs() < 1e-6);

        let events = progress_events(&events, &job_id);
        let has_stage = |progress: f64, stage: TaggingStage| {
            events.iter().any(|(status, value, event_stage)| {
                *status == TaggingJobStatus::Processing
                    && (*value - progress).abs() < 1e-9
                    && *event_stage == Some(stage)
            })
        };
        assert!(has_stage(0.1, TaggingStage::Presence));
        assert!(has_stage(0.3, TaggingStage::PeopleCount));
        assert!(has_stage(0.5, TaggingStage::PlaceClass));
        assert!(has_stage(0.7, TaggingStage::PlaceTags));
        assert!(events.iter().any(|(status, value, _)| {
            *status == TaggingJobStatus::Completed && (*value - 1.0).abs() < 1e-9
        }));
    }

    #[test]
    fn place_class_indoor_uses_indoor_tags() {
        let ctx = TaggingTestContext::with_scenario(
            "normal",
            presence_response(true, 0.99),
            people_response(2, [1, 3]),
            "indoor",
            None,
            true,
            test_config(),
        );
        ctx.enqueue_one();
        assert!(ctx.worker.process_once().unwrap());

        let ai = ctx.db().photo_ai(ctx.photo_id).unwrap().unwrap();
        assert_eq!(ai.place_branch.as_deref(), Some("indoor"));
        let probabilities: PlaceProbabilities =
            serde_json::from_str(ai.place_probabilities_json.as_deref().unwrap()).unwrap();
        assert!(probabilities.indoor > probabilities.outdoor);

        let tags = ctx.db().photo_ai_tags(ctx.photo_id, true).unwrap();
        assert!(tags.iter().any(|tag| tag.tag_key == "japanese_room"));
        assert!(!tags.iter().any(|tag| tag.tag_key == "sea"));
    }

    #[test]
    fn plugin_not_ready_defers_without_retry() {
        let ctx = TaggingTestContext::with_scenario(
            "normal",
            presence_response(false, 0.02),
            PeopleResponse::default(),
            "other",
            None,
            false,
            test_config(),
        );
        let job_id = ctx.enqueue_one();
        assert!(ctx.worker.process_once().unwrap());

        let job = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Pending);
        assert_eq!(job.retry_count, 0);
        assert_eq!(
            job.error.as_deref(),
            Some(TaggingErrorCode::PluginNotReady.as_str())
        );
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_none());
        assert!(!ctx.snapshot_path(&job_id).exists());
    }

    #[test]
    fn pending_job_does_not_use_a_package_being_replaced() {
        let ctx = TaggingTestContext::new("normal", presence_response(false, 0.02));
        let job_id = ctx.enqueue_one();
        let install_events = ctx.events.subscribe();

        let package = package_with_entrypoint(&ctx.key, delayed_probe_entrypoint(&ctx.mock));
        let zip = ctx._dir.path().join("slow-install.zip");
        package.write_zip(&zip).unwrap();
        ctx.plugin.install_from_zip(&zip).unwrap();

        assert!(
            ctx.plugin.operation_active(AI_PLUGIN_ID),
            "the install key must still be held"
        );
        assert!(
            !ctx.plugin.is_ready(AI_PLUGIN_ID),
            "readiness must be false while the package is replaced"
        );
        assert!(
            matches!(ctx.plugin.sidecar(), Err(PluginError::NotReady(_))),
            "a sidecar must not be started for a package being replaced"
        );

        assert!(ctx.worker.process_once().unwrap());
        let job = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Pending);
        assert_eq!(job.retry_count, 0);
        assert_eq!(
            job.error.as_deref(),
            Some(TaggingErrorCode::PluginNotReady.as_str())
        );
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_none());

        wait_for_install(&install_events);
        assert!(ctx.plugin.is_ready(AI_PLUGIN_ID));
    }

    #[test]
    fn pending_job_does_not_use_a_package_being_uninstalled() {
        let ctx = TaggingTestContext::with_scenario(
            // The mock keeps running until the host hits its shutdown timeout,
            // so the lifecycle key stays held while the job is processed.
            "no_shutdown",
            presence_response(false, 0.02),
            PeopleResponse::default(),
            "other",
            None,
            true,
            test_config(),
        );
        let client = ctx.plugin.sidecar().unwrap();
        assert!(client.is_alive());
        let job_id = ctx.enqueue_one();

        ctx.plugin
            .uninstall_async(AI_PLUGIN_ID, None, None, false)
            .unwrap();

        assert!(
            ctx.db()
                .active_plugin_installation(AI_PLUGIN_ID)
                .unwrap()
                .is_some(),
            "the uninstall worker must still be running"
        );
        assert!(
            ctx.plugin.operation_active(AI_PLUGIN_ID),
            "the lifecycle key must still be held"
        );
        assert!(
            !ctx.plugin.is_ready(AI_PLUGIN_ID),
            "readiness must be false while the package is removed"
        );
        assert!(
            matches!(ctx.plugin.sidecar(), Err(PluginError::NotReady(_))),
            "a sidecar must not be started for a package being removed"
        );

        assert!(ctx.worker.process_once().unwrap());
        let job = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Pending);
        assert_eq!(job.retry_count, 0);
        assert_eq!(
            job.error.as_deref(),
            Some(TaggingErrorCode::PluginNotReady.as_str())
        );
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_none());

        wait_until("the uninstall to finish", || {
            !ctx.plugin.operation_active(AI_PLUGIN_ID)
                && ctx
                    .db()
                    .active_plugin_installation(AI_PLUGIN_ID)
                    .unwrap()
                    .is_none()
        });
        assert!(!client.is_alive());
    }

    #[test]
    fn retryable_error_retries_twice_then_fails() {
        let ctx = TaggingTestContext::new("busy", presence_response(true, 0.99));
        let job_id = ctx.enqueue_one();
        assert!(ctx.worker.process_once().unwrap());

        let job = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Failed);
        assert_eq!(job.retry_count, 2);
        assert_eq!(job.error.as_deref(), Some(TaggingErrorCode::Busy.as_str()));
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_none());
        assert!(!ctx.snapshot_path(&job_id).exists());
    }

    #[test]
    fn non_retryable_error_fails_immediately() {
        let ctx = TaggingTestContext::new("error", presence_response(true, 0.99));
        let job_id = ctx.enqueue_one();
        assert!(ctx.worker.process_once().unwrap());

        let job = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Failed);
        assert_eq!(job.retry_count, 0);
        assert_eq!(
            job.error.as_deref(),
            Some(TaggingErrorCode::ImageDecodeFailed.as_str())
        );
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_none());
    }

    #[test]
    fn cancel_during_request_marks_job_cancelled_and_writes_nothing() {
        let ctx = TaggingTestContext::with_scenario(
            "slow",
            presence_response(true, 0.99),
            PeopleResponse::default(),
            "other",
            Some(5_000),
            true,
            test_config(),
        );
        let job_id = ctx.enqueue_one();
        let events = ctx.events.subscribe();
        let worker = ctx.worker.clone();
        let handle = thread::spawn(move || worker.process_once());

        wait_for_presence_stage(&events, &job_id);
        // Mirrors the IPC handler: the database row is the authoritative
        // cancellation, and the token stops the in-flight request.
        let cancelled_at = Instant::now();
        ctx.db().cancel_tagging_job(&job_id).unwrap();
        assert!(ctx.control.cancel(&job_id));

        assert!(handle.join().unwrap().unwrap());
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(2),
            "the worker waited for the 5 s sidecar response despite cancellation"
        );
        let job = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Cancelled);
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_none());
        assert!(
            ctx.db()
                .photo_ai_tags(ctx.photo_id, false)
                .unwrap()
                .is_empty()
        );
        assert!(!ctx.snapshot_path(&job_id).exists());
    }

    #[test]
    fn cancel_during_retry_backoff_marks_cancelled() {
        let mut config = test_config();
        config.retry_delays = [Duration::from_secs(2), Duration::from_secs(2)];
        let ctx = TaggingTestContext::with_scenario(
            "busy",
            presence_response(true, 0.99),
            PeopleResponse::default(),
            "other",
            None,
            true,
            config,
        );
        let job_id = ctx.enqueue_one();
        let worker = ctx.worker.clone();
        let handle = thread::spawn(move || worker.process_once());

        wait_until("the first retry backoff", || {
            ctx.db().tagging_job(&job_id).unwrap().unwrap().retry_count == 1
        });
        let backoff = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(backoff.status, TaggingJobStatus::Processing);

        let cancelled_at = Instant::now();
        ctx.db().cancel_tagging_job(&job_id).unwrap();
        assert!(ctx.control.cancel(&job_id));
        assert!(handle.join().unwrap().unwrap());
        assert!(cancelled_at.elapsed() < Duration::from_secs(1));

        let job = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Cancelled);
        assert_eq!(job.retry_count, 1);
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_none());
    }

    #[test]
    fn stopping_during_retry_backoff_leaves_the_job_processing() {
        let mut config = test_config();
        config.retry_delays = [Duration::from_secs(5), Duration::from_secs(5)];
        let ctx = TaggingTestContext::with_scenario(
            "busy",
            presence_response(true, 0.99),
            PeopleResponse::default(),
            "other",
            None,
            true,
            config,
        );
        let job_id = ctx.enqueue_one();
        let worker = ctx.worker.clone();
        let handle = thread::spawn(move || worker.process_once());

        wait_until("the first retry backoff", || {
            ctx.db().tagging_job(&job_id).unwrap().unwrap().retry_count == 1
        });

        let stopped_at = Instant::now();
        ctx.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        ctx.control.cancel_all();
        assert!(handle.join().unwrap().unwrap());
        assert!(
            stopped_at.elapsed() < Duration::from_secs(1),
            "the worker waited for the 5 s retry backoff despite stopping"
        );

        let job = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Processing);
        assert_eq!(job.retry_count, 1);
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_none());
    }

    #[test]
    fn restart_during_retry_backoff_recovers() {
        let ctx = TaggingTestContext::new("normal", presence_response(false, 0.02));
        let job_id = ctx.enqueue_one();
        let claimed = ctx
            .db()
            .next_pending_tagging_job()
            .unwrap()
            .expect("claim the job");
        assert_eq!(claimed.id, job_id);
        assert!(ctx.db().bump_tagging_retry(&job_id).unwrap());

        let interrupted = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(interrupted.status, TaggingJobStatus::Processing);
        assert_eq!(interrupted.retry_count, 1);

        assert_eq!(ctx.db().resume_interrupted_tagging_jobs().unwrap(), 1);
        let resumed = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(resumed.status, TaggingJobStatus::Pending);
        assert_eq!(resumed.retry_count, 1);

        assert!(ctx.worker.process_once().unwrap());
        let completed = ctx.db().tagging_job(&job_id).unwrap().unwrap();
        assert_eq!(completed.status, TaggingJobStatus::Completed);
        assert_eq!(completed.retry_count, 1);
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_some());
    }

    #[test]
    fn retry_of_one_job_does_not_touch_another_pending_job() {
        let mut config = test_config();
        config.retry_delays = [Duration::from_secs(1), Duration::from_secs(1)];
        let ctx = TaggingTestContext::with_scenario(
            "busy",
            presence_response(true, 0.99),
            PeopleResponse::default(),
            "other",
            None,
            true,
            config,
        );

        // B is older than A and is released from the backlog while A backs
        // off. The retried job must keep its claim instead of claiming B.
        let second_photo = ctx.add_photo("second.png");
        ctx.db()
            .enqueue_tagging_jobs(&[second_photo], AI_PLUGIN_ID, TaggingJobStatus::Waiting)
            .unwrap();
        let second_job = ctx
            .db()
            .tagging_jobs_for_photo(second_photo)
            .unwrap()
            .into_iter()
            .find(|job| job.status == TaggingJobStatus::Waiting)
            .expect("the waiting job")
            .id;
        let first_job = ctx.enqueue_one();

        fs::create_dir_all(tagging_cache_dir(&ctx.ai_data_dir)).unwrap();
        fs::write(ctx.snapshot_path(&second_job), b"keep").unwrap();
        let second_token = ctx.control.register(&second_job);

        let worker = ctx.worker.clone();
        let handle = thread::spawn(move || worker.process_once());
        wait_until("the first retry backoff", || {
            ctx.db()
                .tagging_job(&first_job)
                .unwrap()
                .unwrap()
                .retry_count
                == 1
        });
        assert_eq!(
            ctx.db().release_waiting_tagging_jobs(AI_PLUGIN_ID).unwrap(),
            1
        );
        assert!(handle.join().unwrap().unwrap());

        let first = ctx.db().tagging_job(&first_job).unwrap().unwrap();
        assert_eq!(first.status, TaggingJobStatus::Failed);
        assert_eq!(first.retry_count, 2);
        let second = ctx.db().tagging_job(&second_job).unwrap().unwrap();
        assert_eq!(second.status, TaggingJobStatus::Pending);
        assert_eq!(second.retry_count, 0);
        assert!(ctx.db().photo_ai(second_photo).unwrap().is_none());
        assert!(!second_token.is_cancelled());
        assert!(ctx.snapshot_path(&second_job).exists());
        // The token was never unregistered, so a later cancel still reaches it.
        assert!(ctx.control.cancel(&second_job));
        assert!(second_token.is_cancelled());
    }

    #[test]
    fn crash_marks_job_failed_and_keeps_other_jobs() {
        let ctx = TaggingTestContext::new("crash", presence_response(true, 0.99));
        let first_job = ctx.enqueue_one();
        let second_photo = ctx.add_photo("second.png");
        let second_job = ctx.enqueue(second_photo);

        assert!(ctx.worker.process_once().unwrap());

        let failed = ctx.db().tagging_job(&first_job).unwrap().unwrap();
        assert_eq!(failed.status, TaggingJobStatus::Failed);
        assert_eq!(
            failed.error.as_deref(),
            Some(TaggingErrorCode::SidecarExited.as_str())
        );
        assert_eq!(failed.retry_count, 2);
        let pending = ctx.db().tagging_job(&second_job).unwrap().unwrap();
        assert_eq!(pending.status, TaggingJobStatus::Pending);
        assert!(ctx.db().photo_ai(ctx.photo_id).unwrap().is_none());
        assert!(ctx.db().photo_ai(second_photo).unwrap().is_none());
    }

    #[test]
    fn snapshot_is_deleted_after_success_and_failure() {
        let success = TaggingTestContext::new("normal", presence_response(false, 0.02));
        let success_job = success.enqueue_one();
        assert!(success.worker.process_once().unwrap());
        assert!(!success.snapshot_path(&success_job).exists());
        drop(success);

        let failure = TaggingTestContext::new("error", presence_response(true, 0.99));
        let failure_job = failure.enqueue_one();
        assert!(failure.worker.process_once().unwrap());
        assert!(!failure.snapshot_path(&failure_job).exists());
    }

    #[test]
    fn idle_timeout_shuts_down_the_sidecar() {
        let mut config = test_config();
        config.idle_timeout = Duration::from_millis(100);
        let ctx = TaggingTestContext::with_scenario(
            "normal",
            presence_response(false, 0.02),
            PeopleResponse::default(),
            "other",
            None,
            true,
            config,
        );
        ctx.enqueue_one();
        assert!(ctx.worker.process_once().unwrap());

        let client = ctx.plugin.sidecar().unwrap();
        assert!(client.is_alive());
        thread::sleep(Duration::from_millis(150));

        assert!(!ctx.worker.process_once().unwrap());
        assert!(!client.is_alive());
    }
}
