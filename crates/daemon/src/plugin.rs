//! Daemon-owned lifecycle for AI tagging plugins.
//!
//! The service owns the installed plugin rows, the lazily started sidecar, and
//! every long-running install / model operation. IPC handlers only delegate to
//! this module.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use vrc_photon_ai_host::{
    AiHostError, ModelCheck, ModelHttpClient, PluginManifest, ProbeExpectations, SidecarClient,
    SidecarConfig, SidecarPaths, check_model, delete_model as delete_model_files,
    download_model as download_plugin_model, import_model_files as import_model_files_from_disk,
    install_from_zip as install_plugin_zip, parse_manifest, plugins_root,
};
use vrc_photon_ai_protocol::{Catalog, Hello, ModelState, Probe};
use vrc_photon_core::ai::PluginInstallation;
use vrc_photon_core::db::Db;
use vrc_photon_core::logging::{MAX_LOG_BYTES, prepare_log_file_in};
use vrc_photon_core::settings::AppSettings;
use vrc_photon_ipc::{Event, PluginInstallStatus};

use crate::ipc_server::EventBus;

/// The only AI plugin the daemon knows about in M2.
pub(crate) const AI_PLUGIN_ID: &str = "vrc-photon-autotag";

/// How often model progress is written to the database and event bus.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// Failures returned by [`PluginService`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum PluginError {
    #[error(transparent)]
    Host(#[from] AiHostError),
    #[error(transparent)]
    Core(#[from] vrc_photon_core::CoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("unknown plugin: {0}")]
    NotFound(String),
    #[error("another operation is already running for {0}")]
    Busy(String),
    #[error("plugin is not ready: {0}")]
    NotReady(String),
}

impl PluginError {
    /// True when the user has to accept the model licence before retrying.
    pub(crate) fn is_consent_required(&self) -> bool {
        matches!(self, Self::Host(AiHostError::ConsentRequired))
    }
}

pub(crate) type Result<T> = std::result::Result<T, PluginError>;

/// Owns plugin installs, the shared model files, the probe cache, and one
/// lazily started sidecar.
#[derive(Clone)]
pub(crate) struct PluginService {
    inner: Arc<PluginServiceInner>,
}

struct PluginServiceInner {
    db: Arc<Mutex<Db>>,
    data_dir: PathBuf,
    events: EventBus,
    keys: Vec<String>,
    http: Arc<dyn ModelHttpClient>,
    sidecar: Mutex<Option<Arc<SidecarClient>>>,
    operations: Mutex<HashSet<String>>,
    model_cancel: Mutex<HashMap<String, Arc<AtomicBool>>>,
    host_version: String,
}

/// Removes an in-flight operation marker even when the worker panics.
struct OperationGuard {
    inner: Arc<PluginServiceInner>,
    key: String,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        lock_unpoisoned(&self.inner.operations).remove(&self.key);
    }
}

impl PluginService {
    pub(crate) fn new(
        db: Arc<Mutex<Db>>,
        data_dir: PathBuf,
        events: EventBus,
        keys: Vec<String>,
        http: Arc<dyn ModelHttpClient>,
    ) -> Self {
        Self {
            inner: Arc::new(PluginServiceInner {
                db,
                data_dir,
                events,
                keys,
                http,
                sidecar: Mutex::new(None),
                operations: Mutex::new(HashSet::new()),
                model_cancel: Mutex::new(HashMap::new()),
                host_version: env!("CARGO_PKG_VERSION").to_string(),
            }),
        }
    }

    /// Accepts a signed plugin ZIP and processes it on a worker thread.
    pub(crate) fn install_from_zip(&self, zip_path: &Path) -> Result<()> {
        let guard = self.inner.begin_operation("install")?;
        let inner = self.inner.clone();
        let zip_path = zip_path.to_path_buf();
        thread::Builder::new()
            .name("plugin-install".to_string())
            .spawn(move || {
                let _guard = guard;
                match inner.install(&zip_path) {
                    Ok(install) => {
                        tracing::info!(
                            plugin_id = %install.plugin_id,
                            variant = %install.variant,
                            version = %install.version,
                            "installed AI plugin"
                        );
                        inner.events.publish(Event::PluginInstallProgress {
                            plugin_id: install.plugin_id,
                            variant: install.variant,
                            version: Some(install.version),
                            status: PluginInstallStatus::Installed,
                            error: None,
                        });
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            zip = %zip_path.display(),
                            "AI plugin install failed"
                        );
                        inner.events.publish(Event::PluginInstallProgress {
                            plugin_id: String::new(),
                            variant: String::new(),
                            version: None,
                            status: PluginInstallStatus::Failed,
                            error: Some(error.to_string()),
                        });
                    }
                }
            })
            .map_err(PluginError::Io)?;
        Ok(())
    }

    /// Removes one version, one variant, or every installation of a plugin.
    pub(crate) fn uninstall(
        &self,
        plugin_id: &str,
        variant: Option<&str>,
        version: Option<&str>,
        delete_ai_tags: bool,
    ) -> Result<()> {
        self.inner
            .uninstall(plugin_id, variant, version, delete_ai_tags)
    }

    /// Runs `--probe` for the active installation.
    pub(crate) fn probe(&self, plugin_id: &str) -> Result<()> {
        let install = self.inner.active_install(plugin_id)?;
        self.inner.probe_install(&install)
    }

    pub(crate) fn set_active_variant(
        &self,
        plugin_id: &str,
        variant: &str,
        version: Option<&str>,
    ) -> Result<()> {
        self.inner.set_active_variant(plugin_id, variant, version)
    }

    /// Starts a model download after checking the licence consent.
    pub(crate) fn start_model_download(&self, plugin_id: &str) -> Result<()> {
        let (_, manifest) = self.inner.active_install_and_manifest(plugin_id)?;
        {
            let db = self.inner.db();
            let settings = AppSettings::load(&db)?;
            if settings.ai_model_license_consent_at.is_none() {
                return Err(AiHostError::ConsentRequired.into());
            }
        }

        let guard = self.inner.begin_operation(format!("model:{plugin_id}"))?;
        let cancel = Arc::new(AtomicBool::new(false));
        lock_unpoisoned(&self.inner.model_cancel).insert(plugin_id.to_string(), cancel.clone());
        let inner = self.inner.clone();
        let thread_plugin_id = plugin_id.to_string();
        let spawned = thread::Builder::new()
            .name("plugin-model-download".to_string())
            .spawn(move || {
                let _guard = guard;
                if let Err(error) = inner.download_model_files(&thread_plugin_id, &manifest, cancel)
                {
                    tracing::warn!(
                        %error,
                        plugin_id = %thread_plugin_id,
                        "model download failed"
                    );
                }
                lock_unpoisoned(&inner.model_cancel).remove(&thread_plugin_id);
            });
        if let Err(error) = spawned {
            lock_unpoisoned(&self.inner.model_cancel).remove(plugin_id);
            return Err(PluginError::Io(error));
        }
        Ok(())
    }

    /// Requests cancellation of a running model download.
    ///
    /// Cancellation is one-shot: the token is removed here so a second call
    /// reports `false` even while the worker is stopping.
    pub(crate) fn cancel_model_download(&self, plugin_id: &str) -> Result<bool> {
        let token = lock_unpoisoned(&self.inner.model_cancel).remove(plugin_id);
        match token {
            Some(token) => {
                token.store(true, Ordering::SeqCst);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Imports full model files selected by the user on a worker thread.
    pub(crate) fn import_model_files(&self, plugin_id: &str, paths: &[PathBuf]) -> Result<()> {
        let (_, manifest) = self.inner.active_install_and_manifest(plugin_id)?;
        let guard = self.inner.begin_operation(format!("model:{plugin_id}"))?;
        let inner = self.inner.clone();
        let plugin_id = plugin_id.to_string();
        let paths = paths.to_vec();
        thread::Builder::new()
            .name("plugin-model-import".to_string())
            .spawn(move || {
                let _guard = guard;
                if let Err(error) = inner.import_model_files(&plugin_id, &manifest, &paths) {
                    tracing::warn!(%error, plugin_id = %plugin_id, "model import failed");
                }
            })
            .map_err(PluginError::Io)?;
        Ok(())
    }

    /// Stops the sidecar and removes the model files.
    pub(crate) fn delete_model(&self, plugin_id: &str) -> Result<()> {
        let (_, manifest) = self.inner.active_install_and_manifest(plugin_id)?;
        let _guard = self.inner.begin_operation(format!("model:{plugin_id}"))?;
        self.inner.delete_model_files(plugin_id, &manifest)
    }

    /// Shuts the sidecar down and waits up to its shutdown timeout.
    pub(crate) fn stop_sidecar(&self) {
        self.inner.stop_sidecar();
    }

    /// Returns the running sidecar, spawning it when necessary.
    pub(crate) fn sidecar(&self) -> Result<Arc<SidecarClient>> {
        self.inner.sidecar()
    }

    /// True when an active install, a ready model, and a saved probe exist.
    pub(crate) fn is_ready(&self, plugin_id: &str) -> bool {
        self.inner.is_ready(plugin_id)
    }

    pub(crate) fn sync_catalog_from_install(&self, install: &PluginInstallation) -> Result<()> {
        self.inner.sync_catalog_from_install(install)
    }

    pub(crate) fn sync_catalog_from_hello(&self, hello: &Hello) -> Result<()> {
        self.inner.sync_catalog_from_hello(hello)
    }

    /// Resets downloads interrupted by a daemon crash to `missing`.
    pub(crate) fn recover_interrupted_model_state(&self) -> Result<()> {
        self.inner.recover_interrupted_model_state()
    }
}

impl PluginServiceInner {
    fn db(&self) -> MutexGuard<'_, Db> {
        lock_unpoisoned(&self.db)
    }

    fn begin_operation(self: &Arc<Self>, key: impl Into<String>) -> Result<OperationGuard> {
        let key = key.into();
        let mut operations = lock_unpoisoned(&self.operations);
        if !operations.insert(key.clone()) {
            return Err(PluginError::Busy(key));
        }
        drop(operations);
        Ok(OperationGuard {
            inner: self.clone(),
            key,
        })
    }

    fn active_install(&self, plugin_id: &str) -> Result<PluginInstallation> {
        self.db()
            .active_plugin_installation(plugin_id)?
            .ok_or_else(|| PluginError::NotFound(plugin_id.to_string()))
    }

    fn active_install_and_manifest(
        &self,
        plugin_id: &str,
    ) -> Result<(PluginInstallation, PluginManifest)> {
        let install = self.active_install(plugin_id)?;
        let manifest = parse_manifest(install.manifest_json.as_bytes())?;
        Ok((install, manifest))
    }

    fn install(&self, zip_path: &Path) -> Result<PluginInstallation> {
        self.stop_sidecar();
        let installed =
            install_plugin_zip(zip_path, &self.data_dir, &self.keys, &self.host_version)?;
        let manifest = installed.manifest.clone();

        self.events.publish(Event::PluginInstallProgress {
            plugin_id: installed.plugin_id.clone(),
            variant: installed.variant.clone(),
            version: Some(installed.version.clone()),
            status: PluginInstallStatus::Running,
            error: None,
        });

        let now = Utc::now().to_rfc3339();
        let install = PluginInstallation {
            plugin_id: installed.plugin_id,
            variant: installed.variant,
            version: installed.version,
            protocol_version: installed.protocol_version,
            path: installed.path.to_string_lossy().into_owned(),
            manifest_json: installed.manifest_json,
            catalog_version: Some(manifest.catalog_version.clone()),
            active: true,
            installed_at: now.clone(),
            updated_at: now,
        };
        self.db().upsert_plugin_installation(&install)?;
        self.sync_catalog_from_install(&install)?;
        self.probe_install(&install)?;
        Ok(install)
    }

    fn uninstall(
        &self,
        plugin_id: &str,
        variant: Option<&str>,
        version: Option<&str>,
        delete_ai_tags: bool,
    ) -> Result<()> {
        let (targets, removes_active) = {
            let db = self.db();
            let targets: Vec<_> = db
                .list_plugin_installations(plugin_id)?
                .into_iter()
                .filter(|install| {
                    variant.is_none_or(|variant| install.variant == variant)
                        && version.is_none_or(|version| install.version == version)
                })
                .collect();
            if targets.is_empty() {
                return Err(PluginError::NotFound(format!(
                    "{plugin_id} variant={variant:?} version={version:?}"
                )));
            }
            let removes_active = db
                .active_plugin_installation(plugin_id)?
                .is_some_and(|active| {
                    targets.iter().any(|target| {
                        target.variant == active.variant && target.version == active.version
                    })
                });
            (targets, removes_active)
        };

        if removes_active {
            self.stop_sidecar();
        }

        for target in &targets {
            match fs::remove_dir_all(&target.path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        {
            let db = self.db();
            for target in &targets {
                db.remove_plugin_installation(plugin_id, &target.variant, &target.version)?;
            }
            if delete_ai_tags {
                db.delete_ai_data(plugin_id)?;
            }
            if db.active_plugin_installation(plugin_id)?.is_none() {
                db.set_plugin_probe_json(plugin_id, None)?;
            }
        }

        self.events.publish(Event::PluginStateChanged {
            plugin_id: plugin_id.to_string(),
        });
        Ok(())
    }

    fn set_active_variant(
        &self,
        plugin_id: &str,
        variant: &str,
        version: Option<&str>,
    ) -> Result<()> {
        if !self
            .db()
            .set_active_plugin_installation(plugin_id, variant, version)?
        {
            return Err(PluginError::NotFound(format!(
                "{plugin_id} variant={variant} version={version:?}"
            )));
        }
        self.stop_sidecar();
        self.events.publish(Event::PluginStateChanged {
            plugin_id: plugin_id.to_string(),
        });
        Ok(())
    }

    fn probe_install(&self, install: &PluginInstallation) -> Result<()> {
        let manifest = parse_manifest(install.manifest_json.as_bytes())?;
        let paths = sidecar_paths(install, &manifest);
        let expectations = ProbeExpectations::new(&install.plugin_id, &install.variant)
            .with_catalog_version(&manifest.catalog_version);
        let log_path = self.log_path()?;
        let probe = SidecarClient::probe_with_expectations(&paths, &expectations, &log_path)?;
        if probe.catalog_version != manifest.catalog_version {
            return Err(AiHostError::Integrity(format!(
                "probe reported catalog {} but the manifest declares {}",
                probe.catalog_version, manifest.catalog_version
            ))
            .into());
        }

        let probe_json = serde_json::to_string(&probe)?;
        self.db()
            .set_plugin_probe_json(&install.plugin_id, Some(&probe_json))?;
        self.events.publish(Event::PluginStateChanged {
            plugin_id: install.plugin_id.clone(),
        });
        Ok(())
    }

    fn sync_catalog_from_install(&self, install: &PluginInstallation) -> Result<()> {
        let manifest = parse_manifest(install.manifest_json.as_bytes())?;
        let catalog_bytes = fs::read(Path::new(&install.path).join("catalog.json"))?;
        let catalog: Catalog = serde_json::from_slice(&catalog_bytes)?;
        if catalog.version != manifest.catalog_version {
            return Err(AiHostError::Integrity(format!(
                "catalog.json version {} does not match manifest catalog_version {}",
                catalog.version, manifest.catalog_version
            ))
            .into());
        }
        self.db().sync_ai_catalog(&install.plugin_id, &catalog)?;
        Ok(())
    }

    fn sync_catalog_from_hello(&self, hello: &Hello) -> Result<()> {
        let definitions = self.db().ai_tag_definitions(&hello.plugin_id)?;
        let up_to_date = !definitions.is_empty()
            && definitions
                .iter()
                .all(|definition| definition.catalog_version == hello.catalog.version);
        if !up_to_date {
            self.db()
                .sync_ai_catalog(&hello.plugin_id, &hello.catalog)?;
        }
        Ok(())
    }

    fn is_ready(&self, plugin_id: &str) -> bool {
        let db = self.db();
        if db
            .active_plugin_installation(plugin_id)
            .ok()
            .flatten()
            .is_none()
        {
            return false;
        }
        db.plugin_state(plugin_id)
            .ok()
            .flatten()
            .is_some_and(|state| {
                state.model_state == ModelState::Ready && state.probe_json.is_some()
            })
    }

    fn stop_sidecar(&self) {
        let client = lock_unpoisoned(&self.sidecar).take();
        if let Some(client) = client
            && let Err(error) = client.shutdown()
        {
            tracing::warn!(%error, "AI sidecar shutdown failed; killing it");
            client.kill();
        }
    }

    fn sidecar(&self) -> Result<Arc<SidecarClient>> {
        let mut slot = lock_unpoisoned(&self.sidecar);
        if let Some(existing) = slot.as_ref()
            && existing.is_alive()
        {
            return Ok(existing.clone());
        }
        *slot = None;

        let (install, manifest, model, requested_backend) = {
            let db = self.db();
            let install = db
                .active_plugin_installation(AI_PLUGIN_ID)?
                .ok_or_else(|| PluginError::NotReady("no active AI plugin installation".into()))?;
            let manifest = parse_manifest(install.manifest_json.as_bytes())?;
            let model_ready = db
                .plugin_state(AI_PLUGIN_ID)?
                .is_some_and(|state| state.model_state == ModelState::Ready);
            if !model_ready {
                return Err(PluginError::NotReady("the model is not ready".to_string()));
            }
            let model = match check_model(&manifest, &self.data_dir) {
                ModelCheck::Ready(paths) => paths,
                ModelCheck::Missing => {
                    return Err(PluginError::NotReady(
                        "the model files are missing".to_string(),
                    ));
                }
                ModelCheck::Invalid(message) => return Err(PluginError::NotReady(message)),
            };
            let settings = AppSettings::load(&db)?;
            (
                install,
                manifest,
                model,
                settings.ai_tagging_backend.clone(),
            )
        };

        let paths = sidecar_paths(&install, &manifest);
        let log_path = self.log_path()?;
        let expectations = ProbeExpectations::new(&install.plugin_id, &install.variant)
            .with_catalog_version(&manifest.catalog_version);
        let probe = SidecarClient::probe_with_expectations(&paths, &expectations, &log_path)?;
        let backend = choose_backend(&manifest.backend, &requested_backend, &probe);
        let config = SidecarConfig {
            paths,
            model,
            variant: install.variant.clone(),
            backend,
            plugin_id: install.plugin_id.clone(),
            catalog_version: Some(manifest.catalog_version.clone()),
            log_path,
            env: Vec::new(),
        };
        let client = Arc::new(SidecarClient::spawn(config)?);
        self.sync_catalog_from_hello(client.hello())?;
        *slot = Some(client.clone());
        Ok(client)
    }

    fn delete_model_files(&self, plugin_id: &str, manifest: &PluginManifest) -> Result<()> {
        self.stop_sidecar();
        delete_model_files(manifest, &self.data_dir)?;
        let revision = manifest.model.revision.clone();
        self.set_model_state(plugin_id, ModelState::Missing, &revision, 0, None, None)?;
        self.events.publish(Event::PluginStateChanged {
            plugin_id: plugin_id.to_string(),
        });
        Ok(())
    }

    fn download_model_files(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
        cancel: Arc<AtomicBool>,
    ) -> Result<()> {
        let revision = manifest.model.revision.clone();
        let total = model_total(manifest);
        let mut progress = ModelProgress::new(self, plugin_id, &revision);
        progress.update(0, Some(total), ModelState::Downloading, true);

        let result = {
            let mut on_chunk = |bytes: u64, total: Option<u64>| {
                let complete = total.is_some_and(|total| bytes >= total);
                progress.update(bytes, total, ModelState::Downloading, complete);
            };
            download_plugin_model(
                manifest,
                &self.data_dir,
                self.http.as_ref(),
                &mut on_chunk,
                &cancel,
            )
        };

        let downloaded = progress.bytes;
        let result = match result {
            Ok(_) => self.verify_and_mark_ready(plugin_id, &revision, total, manifest),
            Err(error) => self.fail_model_operation(plugin_id, &revision, total, downloaded, error),
        };
        self.events.publish(Event::PluginStateChanged {
            plugin_id: plugin_id.to_string(),
        });
        result
    }

    fn import_model_files(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
        paths: &[PathBuf],
    ) -> Result<()> {
        let revision = manifest.model.revision.clone();
        let total = model_total(manifest);
        let mut progress = ModelProgress::new(self, plugin_id, &revision);
        progress.update(0, Some(total), ModelState::Downloading, true);

        let result = {
            let mut on_progress = |bytes: u64, total: Option<u64>| {
                let complete = total.is_some_and(|total| bytes >= total);
                progress.update(bytes, total, ModelState::Downloading, complete);
            };
            import_model_files_from_disk(manifest, paths, &self.data_dir, &mut on_progress)
        };

        let downloaded = progress.bytes;
        let result = match result {
            Ok(_) => self.verify_and_mark_ready(plugin_id, &revision, total, manifest),
            Err(error) => self.fail_model_operation(plugin_id, &revision, total, downloaded, error),
        };
        self.events.publish(Event::PluginStateChanged {
            plugin_id: plugin_id.to_string(),
        });
        result
    }

    fn verify_and_mark_ready(
        &self,
        plugin_id: &str,
        revision: &str,
        total: u64,
        manifest: &PluginManifest,
    ) -> Result<()> {
        self.set_model_state(
            plugin_id,
            ModelState::Verifying,
            revision,
            total,
            Some(total),
            None,
        )?;
        match check_model(manifest, &self.data_dir) {
            ModelCheck::Ready(_) => {
                self.set_model_state(
                    plugin_id,
                    ModelState::Ready,
                    revision,
                    total,
                    Some(total),
                    None,
                )?;
                Ok(())
            }
            ModelCheck::Missing => {
                let message = "the model files are missing after the operation".to_string();
                self.set_model_state(
                    plugin_id,
                    ModelState::Error,
                    revision,
                    total,
                    Some(total),
                    Some(&message),
                )?;
                Err(PluginError::NotReady(message))
            }
            ModelCheck::Invalid(message) => {
                self.set_model_state(
                    plugin_id,
                    ModelState::Error,
                    revision,
                    total,
                    Some(total),
                    Some(&message),
                )?;
                Err(AiHostError::Integrity(message).into())
            }
        }
    }

    fn fail_model_operation(
        &self,
        plugin_id: &str,
        revision: &str,
        total: u64,
        downloaded: u64,
        error: AiHostError,
    ) -> Result<()> {
        if matches!(error, AiHostError::Cancelled) {
            self.set_model_state(
                plugin_id,
                ModelState::Missing,
                revision,
                0,
                Some(total),
                None,
            )?;
            return Ok(());
        }
        let message = error.to_string();
        self.set_model_state(
            plugin_id,
            ModelState::Error,
            revision,
            downloaded,
            Some(total),
            Some(&message),
        )?;
        Err(error.into())
    }

    fn set_model_state(
        &self,
        plugin_id: &str,
        state: ModelState,
        revision: &str,
        downloaded: u64,
        total: Option<u64>,
        error: Option<&str>,
    ) -> Result<()> {
        let downloaded_i64 = i64::try_from(downloaded).unwrap_or(i64::MAX);
        let total_i64 = total.map(|value| i64::try_from(value).unwrap_or(i64::MAX));
        self.db().set_plugin_model_state(
            plugin_id,
            state,
            Some(revision),
            downloaded_i64,
            total_i64,
            error,
        )?;
        self.events.publish(Event::ModelDownloadProgress {
            plugin_id: plugin_id.to_string(),
            status: state,
            bytes: downloaded,
            total,
            error: error.map(str::to_string),
        });
        Ok(())
    }

    fn recover_interrupted_model_state(&self) -> Result<()> {
        let mut plugin_ids = HashSet::new();
        plugin_ids.insert(AI_PLUGIN_ID.to_string());
        match fs::read_dir(plugins_root(&self.data_dir)) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if !entry.file_type()?.is_dir() {
                        continue;
                    }
                    if let Some(plugin_id) = entry.file_name().to_str() {
                        plugin_ids.insert(plugin_id.to_string());
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        for plugin_id in plugin_ids {
            let Some(state) = self.db().plugin_state(&plugin_id)? else {
                continue;
            };
            if matches!(
                state.model_state,
                ModelState::Downloading | ModelState::Verifying
            ) {
                self.db().set_plugin_model_state(
                    &plugin_id,
                    ModelState::Missing,
                    state.model_revision.as_deref(),
                    0,
                    state.model_bytes_total,
                    Some("interrupted"),
                )?;
                self.events.publish(Event::PluginStateChanged { plugin_id });
            }
        }
        Ok(())
    }

    fn log_path(&self) -> Result<PathBuf> {
        Ok(prepare_log_file_in(
            &self.data_dir.join("logs"),
            "sidecar",
            MAX_LOG_BYTES,
        )?)
    }
}

/// Accumulates model progress and throttles database / event writes.
struct ModelProgress<'a> {
    service: &'a PluginServiceInner,
    plugin_id: &'a str,
    revision: &'a str,
    bytes: u64,
    last_update: Instant,
}

impl<'a> ModelProgress<'a> {
    fn new(service: &'a PluginServiceInner, plugin_id: &'a str, revision: &'a str) -> Self {
        Self {
            service,
            plugin_id,
            revision,
            bytes: 0,
            last_update: Instant::now() - PROGRESS_INTERVAL,
        }
    }

    fn update(&mut self, bytes: u64, total: Option<u64>, state: ModelState, force: bool) {
        self.bytes = bytes;
        let now = Instant::now();
        if !force && now.duration_since(self.last_update) < PROGRESS_INTERVAL {
            return;
        }
        self.last_update = now;
        if let Err(error) =
            self.service
                .set_model_state(self.plugin_id, state, self.revision, bytes, total, None)
        {
            tracing::debug!(%error, "cannot record model progress");
        }
    }
}

fn model_total(manifest: &PluginManifest) -> u64 {
    manifest.model.files.iter().map(|file| file.size).sum()
}

fn sidecar_paths(install: &PluginInstallation, manifest: &PluginManifest) -> SidecarPaths {
    let install_dir = PathBuf::from(&install.path);
    SidecarPaths {
        executable: install_dir.join(&manifest.entrypoint),
        catalog: install_dir.join("catalog.json"),
        calibration: install_dir.join("calibration.json"),
        install_dir,
    }
}

/// The sidecar calls ROCm `roc`; treat the manifest spelling as the same.
fn normalize_backend(kind: &str) -> String {
    match kind.trim().to_ascii_lowercase().as_str() {
        "rocm" => "roc".to_string(),
        other => other.to_string(),
    }
}

/// Picks a concrete backend from the probe result, falling back to `auto`.
fn choose_backend(manifest_backend: &str, requested: &str, probe: &Probe) -> String {
    let supports = |candidate: &str| {
        probe
            .backends
            .iter()
            .any(|backend| normalize_backend(&backend.kind) == candidate)
    };
    let requested = normalize_backend(requested);
    if requested != "auto" {
        return if supports(&requested) {
            requested
        } else {
            "auto".to_string()
        };
    }
    let manifest = normalize_backend(manifest_backend);
    if supports(&manifest) {
        return manifest;
    }
    probe.backends.first().map_or_else(
        || "auto".to_string(),
        |backend| normalize_backend(&backend.kind),
    )
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    use assert_cmd::cargo::cargo_bin;
    use tempfile::TempDir;
    use vrc_photon_ai_host::test_support::{
        TEST_ENTRYPOINT, TEST_PLUGIN_ID, TestEntry, TestKey, TestPackage, sample_manifest,
        sha256_hex,
    };
    use vrc_photon_ai_host::{
        AiHostError, ManifestFile, ModelCheck, ModelHttpClient, check_model, model_dir,
    };
    use vrc_photon_ai_protocol::{Catalog, ModelState, Probe};
    use vrc_photon_core::db::Db;
    use vrc_photon_core::settings::AppSettings;
    use vrc_photon_ipc::{Event, PluginInstallStatus};

    use super::{PluginError, PluginService};
    use crate::ipc_server::EventBus;

    const PLUGIN: &str = TEST_PLUGIN_ID;
    const CATALOG_VERSION: &str = "2026-11-03.1";
    const MODEL_BYTES: &[u8] = b"model";
    const MMPROJ_BYTES: &[u8] = b"mmproj";
    const MODEL_FILE: &str = "d1-2B-Q8_0.gguf";
    const MMPROJ_FILE: &str = "mmproj-d1-2B-Q8_0.gguf";

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

    struct MockModelClient {
        files: HashMap<String, Vec<u8>>,
        chunk_size: usize,
        chunk_delay: Duration,
        calls: AtomicUsize,
    }

    impl MockModelClient {
        fn new() -> Self {
            let manifest = sample_manifest();
            let files = manifest
                .model
                .files
                .iter()
                .map(|file| {
                    let bytes = if file.path == MMPROJ_FILE {
                        MMPROJ_BYTES.to_vec()
                    } else {
                        MODEL_BYTES.to_vec()
                    };
                    (file.url.clone(), bytes)
                })
                .collect();
            Self {
                files,
                chunk_size: 2,
                // Keeps cancellation observable without slowing the other
                // model tests down noticeably.
                chunk_delay: Duration::from_millis(5),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ModelHttpClient for MockModelClient {
        fn download(
            &self,
            url: &str,
            on_chunk: &mut dyn FnMut(&[u8]) -> vrc_photon_ai_host::Result<()>,
        ) -> vrc_photon_ai_host::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let bytes = self
                .files
                .get(url)
                .unwrap_or_else(|| panic!("unexpected model URL {url}"));
            for chunk in bytes.chunks(self.chunk_size) {
                thread::sleep(self.chunk_delay);
                on_chunk(chunk)?;
            }
            Ok(())
        }
    }

    fn mock_sidecar() -> PathBuf {
        static MOCK: OnceLock<PathBuf> = OnceLock::new();
        MOCK.get_or_init(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "--package", "vrc-photon-ai-mock-sidecar"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .expect("build the mock sidecar");
            assert!(status.success(), "cargo build failed for the mock sidecar");
            cargo_bin("mock-sidecar")
        })
        .clone()
    }

    fn package_with(
        key: &TestKey,
        mock: &Path,
        variant: &str,
        backend: &str,
        extra: &[TestEntry],
    ) -> TestPackage {
        let entrypoint = fs::read(mock).expect("read the mock sidecar binary");
        let mut entries = vec![
            TestEntry::with_mode(TEST_ENTRYPOINT, entrypoint, 0o755),
            TestEntry::new("catalog.json", CATALOG_JSON),
            TestEntry::new("calibration.json", b"{}"),
        ];
        entries.extend_from_slice(extra);

        let files = entries
            .iter()
            .map(|entry| ManifestFile {
                path: entry.path.clone(),
                size: entry.data.len() as u64,
                sha256: sha256_hex(&entry.data),
            })
            .collect();

        let mut package = TestPackage::with_key(key.clone());
        {
            let manifest = package.manifest_mut();
            manifest.variant = variant.to_string();
            manifest.backend = backend.to_string();
            manifest.files = files;
        }
        *package.entries_mut() = entries;
        package
    }

    struct TestContext {
        _dir: TempDir,
        db: Arc<Mutex<Db>>,
        service: PluginService,
        events: EventBus,
        data_dir: PathBuf,
        key: TestKey,
        mock: PathBuf,
        zip: PathBuf,
        install_rx: Receiver<Event>,
    }

    impl TestContext {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create the test directory");
            let mock = mock_sidecar();
            let key = TestKey::generate();
            let db = Arc::new(Mutex::new(
                Db::open(dir.path().join("library.db")).expect("open the test database"),
            ));
            let events = EventBus::default();
            let data_dir = dir.path().join("data");
            let package = package_with(&key, &mock, "linux-cpu", "cpu", &[]);
            let zip = dir.path().join("plugin.zip");
            package.write_zip(&zip).expect("write the test package");
            let service = PluginService::new(
                db.clone(),
                data_dir.clone(),
                events.clone(),
                vec![key.public_key()],
                Arc::new(MockModelClient::new()),
            );
            let install_rx = events.subscribe();
            Self {
                _dir: dir,
                db,
                service,
                events,
                data_dir,
                key,
                mock,
                zip,
                install_rx,
            }
        }

        fn db(&self) -> MutexGuard<'_, Db> {
            self.db
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn package(&self, variant: &str, backend: &str, extra: &[TestEntry]) -> TestPackage {
            package_with(&self.key, &self.mock, variant, backend, extra)
        }

        fn write_zip(&self, name: &str, package: &TestPackage) -> PathBuf {
            let path = self._dir.path().join(name);
            package.write_zip(&path).expect("write the test package");
            path
        }

        fn install_default(&self) {
            self.service
                .install_from_zip(&self.zip)
                .expect("accept the plugin install");
            self.wait_for_install();
        }

        fn wait_for_install(&self) {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.install_rx.recv_timeout(remaining) {
                    Ok(Event::PluginInstallProgress {
                        status: PluginInstallStatus::Installed,
                        ..
                    }) => return,
                    Ok(Event::PluginInstallProgress {
                        status: PluginInstallStatus::Failed,
                        error,
                        ..
                    }) => panic!("plugin install failed: {error:?}"),
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        panic!("timed out waiting for the plugin install")
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        panic!("the event bus stopped before the install finished")
                    }
                }
            }
        }

        fn wait_for(&self, what: &str, mut condition: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if condition() {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out waiting for {what}");
        }

        fn wait_for_installation_count(&self, count: usize) {
            self.wait_for("the plugin installations", || {
                self.db()
                    .list_plugin_installations(PLUGIN)
                    .map(|installs| installs.len() == count)
                    .unwrap_or(false)
            });
        }

        fn wait_for_model_state(&self, expected: ModelState) {
            self.wait_for("the model state", || {
                self.db()
                    .plugin_state(PLUGIN)
                    .ok()
                    .flatten()
                    .is_some_and(|state| state.model_state == expected)
            });
        }

        fn set_consent(&self) {
            let db = self.db();
            let mut settings = AppSettings::load(&db).expect("load the settings");
            settings.ai_model_license_consent_at = Some(1_700_000_000);
            settings.save(&db).expect("save the settings");
        }

        fn make_model_ready(&self) {
            let manifest = sample_manifest();
            let directory = model_dir(&self.data_dir, PLUGIN, &manifest.model.revision);
            fs::create_dir_all(&directory).expect("create the model directory");
            fs::write(directory.join(MODEL_FILE), MODEL_BYTES).expect("write the model");
            fs::write(directory.join(MMPROJ_FILE), MMPROJ_BYTES).expect("write the mmproj");
            self.db()
                .set_plugin_model_state(
                    PLUGIN,
                    ModelState::Ready,
                    Some(&manifest.model.revision),
                    11,
                    Some(11),
                    None,
                )
                .expect("store the ready model state");
        }

        fn collect_events(
            &self,
            rx: &Receiver<Event>,
            mut done: impl FnMut(&[Event]) -> bool,
        ) -> Vec<Event> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut events = Vec::new();
            loop {
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
                if done(&events) {
                    return events;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out collecting events: {events:?}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn model_progress(event: &Event) -> Option<(ModelState, u64, Option<u64>)> {
        match event {
            Event::ModelDownloadProgress {
                plugin_id,
                status,
                bytes,
                total,
                ..
            } if plugin_id == PLUGIN => Some((*status, *bytes, *total)),
            _ => None,
        }
    }

    #[test]
    fn install_probes_and_syncs_the_catalog() {
        let ctx = TestContext::new();
        let events = ctx.events.subscribe();
        ctx.service.install_from_zip(&ctx.zip).unwrap();
        ctx.wait_for_install();

        let install = ctx
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .unwrap();
        assert_eq!(install.variant, "linux-cpu");
        assert_eq!(install.catalog_version.as_deref(), Some(CATALOG_VERSION));
        assert_eq!(ctx.db().ai_tag_definitions(PLUGIN).unwrap().len(), 2);

        let state = ctx.db().plugin_state(PLUGIN).unwrap().unwrap();
        let probe: Probe =
            serde_json::from_str(state.probe_json.as_deref().expect("probe_json")).unwrap();
        assert_eq!(probe.plugin_id, PLUGIN);
        assert_eq!(probe.variant, "linux-cpu");
        assert_eq!(probe.catalog_version, CATALOG_VERSION);

        let events = ctx.collect_events(&events, |events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    Event::PluginInstallProgress {
                        status: PluginInstallStatus::Installed,
                        ..
                    }
                )
            })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            Event::PluginInstallProgress {
                status: PluginInstallStatus::Running,
                ..
            }
        )));
    }

    #[test]
    fn active_variant_switch_stops_the_sidecar() {
        let ctx = TestContext::new();
        ctx.install_default();

        let vulkan = ctx.package("linux-vulkan", "vulkan", &[]);
        let zip = ctx.write_zip("vulkan.zip", &vulkan);
        ctx.service.install_from_zip(&zip).unwrap();
        ctx.wait_for_install();
        ctx.wait_for_installation_count(2);
        ctx.service
            .set_active_variant(PLUGIN, "linux-cpu", None)
            .unwrap();
        ctx.make_model_ready();

        let client = ctx.service.sidecar().unwrap();
        assert_eq!(client.hello().variant, "linux-cpu");
        assert!(client.is_alive());

        let events = ctx.events.subscribe();
        ctx.service
            .set_active_variant(PLUGIN, "linux-vulkan", None)
            .unwrap();

        let active = ctx
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .unwrap();
        assert_eq!(active.variant, "linux-vulkan");
        assert!(
            !client.is_alive(),
            "switching variants must stop the sidecar"
        );
        assert!(
            ctx.collect_events(&events, |events| events.iter().any(|event| {
                matches!(event, Event::PluginStateChanged { plugin_id } if plugin_id == PLUGIN)
            }))
            .iter()
            .any(|event| matches!(event, Event::PluginStateChanged { .. }))
        );

        let next = ctx.service.sidecar().unwrap();
        assert_eq!(next.hello().variant, "linux-vulkan");
    }

    #[test]
    fn uninstall_keeps_or_deletes_ai_tags() {
        let ctx = TestContext::new();
        ctx.install_default();
        assert_eq!(ctx.db().ai_tag_definitions(PLUGIN).unwrap().len(), 2);

        let install_path = PathBuf::from(
            &ctx.db()
                .active_plugin_installation(PLUGIN)
                .unwrap()
                .unwrap()
                .path,
        );
        ctx.service.uninstall(PLUGIN, None, None, false).unwrap();

        assert!(
            ctx.db()
                .active_plugin_installation(PLUGIN)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            ctx.db().ai_tag_definitions(PLUGIN).unwrap().len(),
            2,
            "uninstall without deleting AI tags must keep the catalog"
        );
        assert!(!install_path.exists());

        ctx.install_default();
        ctx.service.uninstall(PLUGIN, None, None, true).unwrap();
        assert!(ctx.db().ai_tag_definitions(PLUGIN).unwrap().is_empty());
    }

    #[test]
    fn model_download_rejects_without_license_consent() {
        let ctx = TestContext::new();
        ctx.install_default();

        let error = ctx.service.start_model_download(PLUGIN).unwrap_err();

        assert!(
            matches!(error, PluginError::Host(AiHostError::ConsentRequired)),
            "{error:?}"
        );
        assert_eq!(
            ctx.db().plugin_state(PLUGIN).unwrap().unwrap().model_state,
            ModelState::Missing
        );
        let manifest = sample_manifest();
        assert!(!model_dir(&ctx.data_dir, PLUGIN, &manifest.model.revision).exists());
    }

    #[test]
    fn model_download_updates_state_and_progress() {
        let ctx = TestContext::new();
        ctx.install_default();
        ctx.set_consent();
        let events = ctx.events.subscribe();

        ctx.service.start_model_download(PLUGIN).unwrap();
        ctx.wait_for_model_state(ModelState::Ready);

        let manifest = sample_manifest();
        assert!(matches!(
            check_model(&manifest, &ctx.data_dir),
            ModelCheck::Ready(_)
        ));
        let state = ctx.db().plugin_state(PLUGIN).unwrap().unwrap();
        assert_eq!(state.model_state, ModelState::Ready);
        assert_eq!(
            state.model_revision.as_deref(),
            Some("5ebcce3bab1c0521f2cab68742a647c7d038adbe")
        );
        assert_eq!(state.model_bytes_downloaded, 11);
        assert_eq!(state.model_bytes_total, Some(11));
        assert_eq!(state.model_error, None);
        assert!(ctx.service.is_ready(PLUGIN));

        let events = ctx.collect_events(&events, |events| {
            events
                .iter()
                .filter_map(model_progress)
                .any(|(status, _, _)| status == ModelState::Ready)
        });
        let progress: Vec<_> = events.iter().filter_map(model_progress).collect();
        assert!(
            progress.iter().any(|(status, bytes, total)| {
                *status == ModelState::Downloading && *bytes > 0 && *total == Some(11)
            }),
            "expected download progress, got {progress:?}"
        );
        assert_eq!(progress.last(), Some(&(ModelState::Ready, 11, Some(11))));
    }

    #[test]
    fn import_model_files_updates_state() {
        let ctx = TestContext::new();
        ctx.install_default();
        let sources = tempfile::tempdir().unwrap();
        let model = sources.path().join(MODEL_FILE);
        let mmproj = sources.path().join(MMPROJ_FILE);
        fs::write(&model, MODEL_BYTES).unwrap();
        fs::write(&mmproj, MMPROJ_BYTES).unwrap();

        ctx.service
            .import_model_files(PLUGIN, &[model, mmproj])
            .unwrap();
        ctx.wait_for_model_state(ModelState::Ready);

        let manifest = sample_manifest();
        assert!(matches!(
            check_model(&manifest, &ctx.data_dir),
            ModelCheck::Ready(_)
        ));
        assert!(ctx.service.is_ready(PLUGIN));

        let state = ctx.db().plugin_state(PLUGIN).unwrap().unwrap();
        assert_eq!(state.model_bytes_downloaded, 11);
        assert_eq!(state.model_error, None);

        ctx.service.delete_model(PLUGIN).unwrap();
        assert_eq!(
            ctx.db().plugin_state(PLUGIN).unwrap().unwrap().model_state,
            ModelState::Missing
        );
        assert!(!model_dir(&ctx.data_dir, PLUGIN, &manifest.model.revision).exists());
        assert!(!ctx.service.is_ready(PLUGIN));
    }

    #[test]
    fn busy_operation_rejects_a_second_install() {
        let ctx = TestContext::new();
        let package = ctx.package(
            "linux-cpu",
            "cpu",
            &[TestEntry::new("padding.bin", vec![7u8; 8 * 1024 * 1024])],
        );
        let zip = ctx.write_zip("busy.zip", &package);

        ctx.service.install_from_zip(&zip).unwrap();
        let error = ctx.service.install_from_zip(&zip).unwrap_err();

        assert!(matches!(error, PluginError::Busy(_)), "{error:?}");
        ctx.wait_for_install();
        assert_eq!(
            ctx.db()
                .active_plugin_installation(PLUGIN)
                .unwrap()
                .unwrap()
                .variant,
            "linux-cpu"
        );
    }

    #[test]
    fn recover_interrupted_model_state_marks_the_download_missing() {
        let ctx = TestContext::new();
        ctx.install_default();
        let revision = sample_manifest().model.revision;
        ctx.db()
            .set_plugin_model_state(
                PLUGIN,
                ModelState::Downloading,
                Some(&revision),
                5,
                Some(11),
                None,
            )
            .unwrap();

        ctx.service.recover_interrupted_model_state().unwrap();

        let state = ctx.db().plugin_state(PLUGIN).unwrap().unwrap();
        assert_eq!(state.model_state, ModelState::Missing);
        assert_eq!(state.model_error.as_deref(), Some("interrupted"));
        assert_eq!(state.model_revision.as_deref(), Some(revision.as_str()));
    }

    #[test]
    fn model_download_can_be_cancelled() {
        let ctx = TestContext::new();
        ctx.install_default();
        ctx.set_consent();

        ctx.service.start_model_download(PLUGIN).unwrap();
        let cancelled = ctx.service.cancel_model_download(PLUGIN).unwrap();

        assert!(cancelled, "a running download must be cancellable");
        ctx.wait_for_model_state(ModelState::Missing);
        assert!(!ctx.service.cancel_model_download(PLUGIN).unwrap());
    }

    #[test]
    fn catalog_sync_rejects_a_mismatched_package_catalog() {
        let ctx = TestContext::new();
        let mut package = ctx.package("linux-cpu", "cpu", &[]);
        let catalog = catalog_with_version("2099-01-01.0");
        let entry = package
            .entries_mut()
            .iter_mut()
            .find(|entry| entry.path == "catalog.json")
            .unwrap();
        entry.data = serde_json::to_vec(&catalog).unwrap();
        let files = package
            .entries()
            .iter()
            .map(|entry| ManifestFile {
                path: entry.path.clone(),
                size: entry.data.len() as u64,
                sha256: sha256_hex(&entry.data),
            })
            .collect();
        package.manifest_mut().files = files;
        let zip = ctx.write_zip("mismatch.zip", &package);

        ctx.service.install_from_zip(&zip).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match ctx.install_rx.recv_timeout(remaining) {
                Ok(Event::PluginInstallProgress {
                    status: PluginInstallStatus::Failed,
                    error,
                    ..
                }) => {
                    assert!(
                        error.as_deref().unwrap_or_default().contains("catalog"),
                        "{error:?}"
                    );
                    break;
                }
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for the catalog mismatch"),
            }
        }
        assert!(ctx.db().ai_tag_definitions(PLUGIN).unwrap().is_empty());
    }

    fn catalog_with_version(version: &str) -> Catalog {
        serde_json::from_str::<Catalog>(CATALOG_JSON)
            .map(|mut catalog| {
                catalog.version = version.to_string();
                catalog
            })
            .unwrap()
    }
}
