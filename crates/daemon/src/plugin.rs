//! Daemon-owned lifecycle for AI tagging plugins.
//!
//! The service owns the installed plugin rows, the lazily started sidecar, and
//! every long-running install / model operation. IPC handlers only delegate to
//! this module.

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
    SidecarConfig, SidecarPaths, check_model, delete_model_revision,
    download_model as download_plugin_model, import_model_files as import_model_files_from_disk,
    install_from_zip_keeping_backup as install_plugin_zip_keeping_backup, parse_manifest,
    plugins_root, restore_installation_backup,
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

/// Operation key held by a plugin ZIP install (the plugin id is unknown until
/// the signed manifest is read).
const INSTALL_OPERATION: &str = "install";

fn lifecycle_operation(plugin_id: &str) -> String {
    format!("lifecycle:{plugin_id}")
}

fn model_operation(plugin_id: &str) -> String {
    format!("model:{plugin_id}")
}

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
    sidecar: Mutex<Option<CachedSidecar>>,
    operations: Arc<Mutex<HashSet<String>>>,
    model_cancel: Mutex<HashMap<String, Arc<AtomicBool>>>,
    host_version: String,
}

/// Identifies the package and model revision a cached sidecar was started for.
///
/// A cached client may only be reused while this identity matches the active
/// installation and model state; replacing, switching, or deleting a package
/// makes the fields differ (or the installation disappear) and forces a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SidecarIdentity {
    plugin_id: String,
    variant: String,
    version: String,
    catalog_version: String,
    model_revision: String,
}

struct CachedSidecar {
    identity: SidecarIdentity,
    client: Arc<SidecarClient>,
}

/// Removes an in-flight operation marker even when the worker panics.
struct OperationGuard {
    operations: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        lock_unpoisoned(&self.operations).remove(&self.key);
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
                operations: Arc::new(Mutex::new(HashSet::new())),
                model_cancel: Mutex::new(HashMap::new()),
                host_version: env!("CARGO_PKG_VERSION").to_string(),
            }),
        }
    }

    /// Accepts a signed plugin ZIP and processes it on a worker thread.
    pub(crate) fn install_from_zip(&self, zip_path: &Path) -> Result<()> {
        let guard = self.inner.begin_install()?;
        let inner = self.inner.clone();
        let zip_path = zip_path.to_path_buf();
        thread::Builder::new()
            .name("plugin-install".to_string())
            .spawn(move || {
                let result = inner.install(&zip_path);
                // Publish the terminal event only after the operation keys are
                // released, so a client reacting to `installed` can start the
                // next operation immediately.
                drop(guard);
                match result {
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
    #[cfg(test)]
    pub(crate) fn uninstall(
        &self,
        plugin_id: &str,
        variant: Option<&str>,
        version: Option<&str>,
        delete_ai_tags: bool,
    ) -> Result<()> {
        let guard = self.inner.begin_lifecycle(plugin_id)?;
        let result = self
            .inner
            .uninstall(plugin_id, variant, version, delete_ai_tags);
        // Release the lifecycle key before announcing the terminal state so a
        // client reacting to the event can start the next operation.
        drop(guard);
        if result.is_ok() {
            self.inner.events.publish(Event::PluginStateChanged {
                plugin_id: plugin_id.to_string(),
            });
        }
        result
    }

    /// Accepts an uninstall, then removes files and rows on a worker thread.
    ///
    /// Stopping the sidecar can take seconds, so IPC must not wait for it.
    /// The target is validated and the lifecycle key is reserved before this
    /// returns; the worker owns the key until it finishes.
    pub(crate) fn uninstall_async(
        &self,
        plugin_id: &str,
        variant: Option<&str>,
        version: Option<&str>,
        delete_ai_tags: bool,
    ) -> Result<()> {
        let guard = self.inner.begin_lifecycle(plugin_id)?;
        let (_, removes_active) = self.inner.uninstall_targets(plugin_id, variant, version)?;
        if removes_active {
            self.inner.invalidate_readiness(plugin_id)?;
        }
        let inner = self.inner.clone();
        let plugin_id = plugin_id.to_string();
        let variant = variant.map(str::to_string);
        let version = version.map(str::to_string);
        thread::Builder::new()
            .name("plugin-uninstall".to_string())
            .spawn(move || {
                let result = {
                    let _guard = guard;
                    inner.uninstall(
                        &plugin_id,
                        variant.as_deref(),
                        version.as_deref(),
                        delete_ai_tags,
                    )
                };
                if let Err(error) = &result {
                    tracing::warn!(%error, plugin_id = %plugin_id, "AI plugin uninstall failed");
                }
                // Publish after the guard is released, on success and failure
                // alike: a client that already got `Ok` must not wait forever.
                inner.events.publish(Event::PluginStateChanged {
                    plugin_id: plugin_id.clone(),
                });
            })
            .map_err(PluginError::Io)?;
        Ok(())
    }

    /// Runs `--probe` for the active installation.
    #[cfg(test)]
    pub(crate) fn probe(&self, plugin_id: &str) -> Result<()> {
        let guard = self.inner.begin_lifecycle(plugin_id)?;
        let install = self.inner.active_install(plugin_id)?;
        let result = self.inner.run_probe(&install);
        drop(guard);
        if result.is_ok() {
            self.inner.events.publish(Event::PluginStateChanged {
                plugin_id: install.plugin_id.clone(),
            });
        }
        result
    }

    /// Accepts a probe, then spawns and waits for the sidecar on a worker.
    pub(crate) fn probe_async(&self, plugin_id: &str) -> Result<()> {
        let guard = self.inner.begin_lifecycle(plugin_id)?;
        let install = self.inner.active_install(plugin_id)?;
        let inner = self.inner.clone();
        thread::Builder::new()
            .name("plugin-probe".to_string())
            .spawn(move || {
                let result = {
                    let _guard = guard;
                    inner.run_probe(&install)
                };
                if let Err(error) = &result {
                    tracing::warn!(
                        %error,
                        plugin_id = %install.plugin_id,
                        "AI plugin probe failed"
                    );
                }
                // The cache is refreshed on success and stays empty on
                // failure; either way clients re-read it after the guard drop.
                inner.events.publish(Event::PluginStateChanged {
                    plugin_id: install.plugin_id.clone(),
                });
            })
            .map_err(PluginError::Io)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_active_variant(
        &self,
        plugin_id: &str,
        variant: &str,
        version: Option<&str>,
    ) -> Result<()> {
        let _guard = self.inner.begin_lifecycle(plugin_id)?;
        self.inner.set_active_variant(plugin_id, variant, version)
    }

    /// Switches the active installation synchronously, then stops the old
    /// sidecar and probes the new variant on a worker thread.
    ///
    /// The row change and the cleared probe cache are visible as soon as this
    /// returns `Ok`, so the UI can render the switch before the sidecar work
    /// finishes.
    pub(crate) fn set_active_variant_async(
        &self,
        plugin_id: &str,
        variant: &str,
        version: Option<&str>,
    ) -> Result<()> {
        let guard = self.inner.begin_lifecycle(plugin_id)?;
        let activated = self
            .inner
            .db()
            .set_active_plugin_installation(plugin_id, variant, version)?;
        if !activated {
            return Err(PluginError::NotFound(format!(
                "{plugin_id} variant={variant} version={version:?}"
            )));
        }
        self.inner.db().set_plugin_probe_json(plugin_id, None)?;
        self.inner.events.publish(Event::PluginStateChanged {
            plugin_id: plugin_id.to_string(),
        });

        let inner = self.inner.clone();
        let plugin_id = plugin_id.to_string();
        thread::Builder::new()
            .name("plugin-variant".to_string())
            .spawn(move || {
                let result = {
                    let _guard = guard;
                    inner.stop_sidecar();
                    (|| -> Result<()> {
                        let install = inner.active_install(&plugin_id)?;
                        inner.run_probe(&install)
                    })()
                };
                if let Err(error) = &result {
                    tracing::warn!(
                        %error,
                        plugin_id = %plugin_id,
                        "AI plugin probe after variant switch failed"
                    );
                }
                // Announce the terminal state after releasing the lifecycle
                // key, whether the probe succeeded or not.
                inner.events.publish(Event::PluginStateChanged {
                    plugin_id: plugin_id.clone(),
                });
            })
            .map_err(PluginError::Io)?;
        Ok(())
    }

    /// Starts a model download after checking the licence consent.
    pub(crate) fn start_model_download(&self, plugin_id: &str) -> Result<()> {
        let guard = self.inner.begin_model(plugin_id)?;
        let (_, manifest) = self.inner.active_install_and_manifest(plugin_id)?;
        {
            let db = self.inner.db();
            let settings = AppSettings::load(&db)?;
            if settings.ai_model_license_consent_at.is_none() {
                return Err(AiHostError::ConsentRequired.into());
            }
        }

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
        let guard = self.inner.begin_model(plugin_id)?;
        let (_, manifest) = self.inner.active_install_and_manifest(plugin_id)?;
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

    /// Stops the sidecar and removes the stored model revision.
    #[cfg(test)]
    pub(crate) fn delete_model(&self, plugin_id: &str) -> Result<()> {
        let guard = self.inner.begin_lifecycle(plugin_id)?;
        let revision = self.inner.stored_model_revision(plugin_id)?;
        self.inner.invalidate_readiness(plugin_id)?;
        let result = self.inner.delete_model_files(plugin_id, &revision);
        drop(guard);
        if result.is_ok() {
            self.inner.events.publish(Event::PluginStateChanged {
                plugin_id: plugin_id.to_string(),
            });
        }
        result
    }

    /// Accepts a model deletion, then stops the sidecar and removes the files
    /// on a worker thread. The stored revision is validated before returning.
    pub(crate) fn delete_model_async(&self, plugin_id: &str) -> Result<()> {
        let guard = self.inner.begin_lifecycle(plugin_id)?;
        let revision = self.inner.stored_model_revision(plugin_id)?;
        self.inner.invalidate_readiness(plugin_id)?;
        let inner = self.inner.clone();
        let plugin_id = plugin_id.to_string();
        thread::Builder::new()
            .name("plugin-model-delete".to_string())
            .spawn(move || {
                let result = {
                    let _guard = guard;
                    inner.delete_model_files(&plugin_id, &revision)
                };
                if let Err(error) = &result {
                    tracing::warn!(%error, plugin_id = %plugin_id, "AI model delete failed");
                }
                // Publish after the guard is released, on success and failure
                // alike: a client that already got `Ok` must not wait forever.
                inner.events.publish(Event::PluginStateChanged {
                    plugin_id: plugin_id.clone(),
                });
            })
            .map_err(PluginError::Io)?;
        Ok(())
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

    pub(crate) fn sync_catalog_from_hello(&self, hello: &Hello) -> Result<()> {
        self.inner.sync_catalog_from_hello(hello)
    }

    /// Resets downloads interrupted by a daemon crash to `missing`.
    pub(crate) fn recover_interrupted_model_state(&self) -> Result<()> {
        self.inner.recover_interrupted_model_state()
    }

    /// Test-only: reports whether an install/lifecycle/model operation is
    /// still holding this plugin's keys.
    #[cfg(test)]
    pub(crate) fn operation_active(&self, plugin_id: &str) -> bool {
        let operations = lock_unpoisoned(&self.inner.operations);
        operations.contains(INSTALL_OPERATION)
            || operations.contains(&lifecycle_operation(plugin_id))
            || operations.contains(&model_operation(plugin_id))
    }
}

impl PluginServiceInner {
    fn db(&self) -> MutexGuard<'_, Db> {
        lock_unpoisoned(&self.db)
    }

    fn begin_install(&self) -> Result<OperationGuard> {
        let mut operations = lock_unpoisoned(&self.operations);
        if !operations.is_empty() {
            return Err(PluginError::Busy(INSTALL_OPERATION.to_string()));
        }
        operations.insert(INSTALL_OPERATION.to_string());
        drop(operations);
        Ok(OperationGuard {
            operations: self.operations.clone(),
            key: INSTALL_OPERATION.to_string(),
        })
    }

    /// Reserves the plugin lifecycle key while the install worker already
    /// holds the global install key. Other operations are blocked by the
    /// install key, so this cannot race a new lifecycle operation.
    fn begin_install_lifecycle(&self, plugin_id: &str) -> Result<OperationGuard> {
        let key = lifecycle_operation(plugin_id);
        let mut operations = lock_unpoisoned(&self.operations);
        if operations.contains(&key) || operations.contains(&model_operation(plugin_id)) {
            return Err(PluginError::Busy(plugin_id.to_string()));
        }
        operations.insert(key.clone());
        drop(operations);
        Ok(OperationGuard {
            operations: self.operations.clone(),
            key,
        })
    }

    fn begin_lifecycle(&self, plugin_id: &str) -> Result<OperationGuard> {
        let key = lifecycle_operation(plugin_id);
        let mut operations = lock_unpoisoned(&self.operations);
        if operations.contains(INSTALL_OPERATION)
            || operations.contains(&key)
            || operations.contains(&model_operation(plugin_id))
        {
            return Err(PluginError::Busy(plugin_id.to_string()));
        }
        operations.insert(key.clone());
        drop(operations);
        Ok(OperationGuard {
            operations: self.operations.clone(),
            key,
        })
    }

    fn begin_model(&self, plugin_id: &str) -> Result<OperationGuard> {
        let key = model_operation(plugin_id);
        let mut operations = lock_unpoisoned(&self.operations);
        if operations.contains(INSTALL_OPERATION)
            || operations.contains(&key)
            || operations.contains(&lifecycle_operation(plugin_id))
        {
            return Err(PluginError::Busy(plugin_id.to_string()));
        }
        operations.insert(key.clone());
        drop(operations);
        Ok(OperationGuard {
            operations: self.operations.clone(),
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
        let (installed, backup) = install_plugin_zip_keeping_backup(
            zip_path,
            &self.data_dir,
            &self.keys,
            &self.host_version,
        )?;
        // Extraction is complete. Stop any client that raced with the
        // replacement before the new package is probed or activated.
        self.stop_sidecar();
        let manifest = installed.manifest.clone();
        let _lifecycle = self.begin_install_lifecycle(&installed.plugin_id)?;

        let (previous_active, previous_probe) = {
            let db = self.db();
            (
                db.active_plugin_installation(&installed.plugin_id)?,
                db.plugin_state(&installed.plugin_id)?
                    .and_then(|state| state.probe_json),
            )
        };
        // The package contents are about to change under the plugin. Drop the
        // cached readiness until the new package passes catalog sync + probe.
        self.db()
            .set_plugin_probe_json(&installed.plugin_id, None)?;

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
            // The previous active installation stays active until this one has
            // passed catalog sync and probe.
            active: false,
            installed_at: now.clone(),
            updated_at: now,
        };
        self.db().upsert_plugin_installation(&install)?;

        let result = (|| -> Result<()> {
            self.sync_catalog_from_install(&install)?;
            self.probe_install(&install)?;
            // Re-check the package and stop the sidecar immediately before
            // activating, so a client started for the previous package cannot
            // survive the switch.
            self.stop_sidecar();
            if !self.db().set_active_plugin_installation(
                &install.plugin_id,
                &install.variant,
                Some(&install.version),
            )? {
                return Err(PluginError::NotFound(format!(
                    "{} {} {}",
                    install.plugin_id, install.variant, install.version
                )));
            }
            Ok(())
        })();

        if let Err(error) = &result {
            // Never leave a failed package looking ready or keep using it.
            self.stop_sidecar();
            let _ = self.db().set_plugin_probe_json(&install.plugin_id, None);

            let restored_files = match &backup {
                Some(backup) => match restore_installation_backup(Path::new(&install.path), backup)
                {
                    Ok(()) => true,
                    Err(restore) => {
                        tracing::error!(
                            %restore,
                            plugin_id = %install.plugin_id,
                            "cannot restore the previous AI plugin files"
                        );
                        false
                    }
                },
                None => match fs::remove_dir_all(&install.path) {
                    Ok(()) => true,
                    Err(remove) if remove.kind() == io::ErrorKind::NotFound => true,
                    Err(remove) => {
                        tracing::error!(
                            %remove,
                            plugin_id = %install.plugin_id,
                            "cannot remove the failed AI plugin package"
                        );
                        false
                    }
                },
            };

            let same_as_previous = previous_active.as_ref().is_some_and(|previous| {
                previous.variant == install.variant && previous.version == install.version
            });
            if (restored_files || !same_as_previous)
                && let Err(remove) = self.db().remove_plugin_installation(
                    &install.plugin_id,
                    &install.variant,
                    &install.version,
                )
            {
                tracing::warn!(
                    %remove,
                    plugin_id = %install.plugin_id,
                    "cannot remove the failed AI plugin row"
                );
            }
            if restored_files && let Some(previous) = &previous_active {
                if let Err(restore) = self.db().upsert_plugin_installation(previous) {
                    tracing::warn!(
                        %restore,
                        plugin_id = %install.plugin_id,
                        "cannot restore the previous AI plugin installation"
                    );
                }
                let catalog_restored = match self.sync_catalog_from_install(previous) {
                    Ok(()) => true,
                    Err(restore) => {
                        tracing::warn!(
                            %restore,
                            plugin_id = %install.plugin_id,
                            "cannot restore the previous AI plugin catalog"
                        );
                        false
                    }
                };
                if catalog_restored
                    && let Err(restore) = self
                        .db()
                        .set_plugin_probe_json(&install.plugin_id, previous_probe.as_deref())
                {
                    tracing::warn!(
                        %restore,
                        plugin_id = %install.plugin_id,
                        "cannot restore the previous AI plugin probe"
                    );
                }
            }
            tracing::warn!(
                %error,
                plugin_id = %install.plugin_id,
                "AI plugin install did not activate"
            );
        }

        result.map(|()| install)
    }

    /// Resolves the installations an uninstall request targets and whether it
    /// removes the active one. Shared by the synchronous and accepted (async)
    /// paths so both reject unknown targets before doing any work.
    fn uninstall_targets(
        &self,
        plugin_id: &str,
        variant: Option<&str>,
        version: Option<&str>,
    ) -> Result<(Vec<PluginInstallation>, bool)> {
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
        Ok((targets, removes_active))
    }

    fn uninstall(
        &self,
        plugin_id: &str,
        variant: Option<&str>,
        version: Option<&str>,
        delete_ai_tags: bool,
    ) -> Result<()> {
        let (targets, removes_active) = self.uninstall_targets(plugin_id, variant, version)?;

        if removes_active {
            self.invalidate_readiness(plugin_id)?;
            // Stop before removing files: an accepted request must never
            // leave a client running on a package that is about to disappear.
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

        Ok(())
    }

    /// Drops cached readiness before an operation that changes the active
    /// package or model. The probe is restored only by a successful rollback
    /// or a new probe.
    fn invalidate_readiness(&self, plugin_id: &str) -> Result<()> {
        self.db().set_plugin_probe_json(plugin_id, None)?;
        Ok(())
    }

    #[cfg(test)]
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
        self.db().set_plugin_probe_json(plugin_id, None)?;
        self.stop_sidecar();
        self.events.publish(Event::PluginStateChanged {
            plugin_id: plugin_id.to_string(),
        });
        Ok(())
    }

    /// Runs the probe and stores the result, without announcing it. Callers
    /// that hold the lifecycle key publish the terminal state after releasing
    /// it.
    fn run_probe(&self, install: &PluginInstallation) -> Result<()> {
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
        Ok(())
    }

    /// Probes and publishes the state change. Used by the install worker,
    /// which announces its own terminal event afterwards.
    fn probe_install(&self, install: &PluginInstallation) -> Result<()> {
        self.run_probe(install)?;
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
        if self.operations_block_readiness(plugin_id) {
            return false;
        }
        let db = self.db();
        let Ok(Some(install)) = db.active_plugin_installation(plugin_id) else {
            return false;
        };
        let Ok(Some(state)) = db.plugin_state(plugin_id) else {
            return false;
        };
        if state.model_state != ModelState::Ready {
            return false;
        }
        let Ok(manifest) = parse_manifest(install.manifest_json.as_bytes()) else {
            return false;
        };
        if state.model_revision.as_deref() != Some(manifest.model.revision.as_str()) {
            return false;
        }
        let Some(probe_json) = state.probe_json.as_deref() else {
            return false;
        };
        let Ok(probe) = serde_json::from_str::<Probe>(probe_json) else {
            return false;
        };
        probe.plugin_id == install.plugin_id
            && probe.variant == install.variant
            && probe.catalog_version == manifest.catalog_version
    }

    /// True while an install or lifecycle operation owns the plugin. Readiness
    /// must be false for the whole operation so a tagging worker cannot start
    /// against a package that is being replaced or deleted.
    fn operations_block_readiness(&self, plugin_id: &str) -> bool {
        let operations = lock_unpoisoned(&self.operations);
        operations.contains(INSTALL_OPERATION)
            || operations.contains(&lifecycle_operation(plugin_id))
    }

    fn stop_sidecar(&self) {
        let client = lock_unpoisoned(&self.sidecar).take();
        if let Some(client) = client {
            shutdown_sidecar(&client.client);
        }
    }

    /// The identity of the active installation and model, or `None` when
    /// either is missing. Used to decide whether a cached client is still
    /// valid; the strict readiness checks happen before spawning.
    fn cached_sidecar_identity(&self, plugin_id: &str) -> Result<Option<SidecarIdentity>> {
        let db = self.db();
        let Some(install) = db.active_plugin_installation(plugin_id)? else {
            return Ok(None);
        };
        let Ok(manifest) = parse_manifest(install.manifest_json.as_bytes()) else {
            return Ok(None);
        };
        let Some(state) = db.plugin_state(plugin_id)? else {
            return Ok(None);
        };
        Ok(Some(SidecarIdentity {
            plugin_id: install.plugin_id,
            variant: install.variant,
            version: install.version,
            catalog_version: manifest.catalog_version,
            model_revision: state.model_revision.unwrap_or_default(),
        }))
    }

    /// Resolves everything needed to start a sidecar for the active package.
    fn sidecar_target(
        &self,
        plugin_id: &str,
    ) -> Result<(
        PluginInstallation,
        PluginManifest,
        vrc_photon_ai_host::ModelPaths,
        String,
        SidecarIdentity,
    )> {
        let (install, manifest, model_revision, model, requested_backend) = {
            let db = self.db();
            let install = db
                .active_plugin_installation(plugin_id)?
                .ok_or_else(|| PluginError::NotReady("no active AI plugin installation".into()))?;
            let manifest = parse_manifest(install.manifest_json.as_bytes())?;
            let state = db
                .plugin_state(plugin_id)?
                .ok_or_else(|| PluginError::NotReady("no plugin state".to_string()))?;
            if state.model_state != ModelState::Ready {
                return Err(PluginError::NotReady("the model is not ready".to_string()));
            }
            let model_revision = state.model_revision.ok_or_else(|| {
                PluginError::NotReady("the model revision is missing".to_string())
            })?;
            if model_revision != manifest.model.revision {
                return Err(PluginError::NotReady(
                    "the model revision does not match the active plugin".to_string(),
                ));
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
                model_revision,
                model,
                settings.ai_tagging_backend.clone(),
            )
        };

        let identity = SidecarIdentity {
            plugin_id: install.plugin_id.clone(),
            variant: install.variant.clone(),
            version: install.version.clone(),
            catalog_version: manifest.catalog_version.clone(),
            model_revision,
        };
        Ok((install, manifest, model, requested_backend, identity))
    }

    fn sidecar(&self) -> Result<Arc<SidecarClient>> {
        let plugin_id = AI_PLUGIN_ID;
        if self.operations_block_readiness(plugin_id) {
            return Err(PluginError::NotReady(
                "a plugin operation is running".to_string(),
            ));
        }

        let active_identity = self.cached_sidecar_identity(plugin_id)?;
        let mut slot = lock_unpoisoned(&self.sidecar);
        if let Some(cached) = slot.as_ref()
            && cached.client.is_alive()
            && active_identity.as_ref() == Some(&cached.identity)
            && !self.operations_block_readiness(plugin_id)
        {
            return Ok(cached.client.clone());
        }

        if let Some(stale) = slot.take() {
            shutdown_sidecar(&stale.client);
        }

        if self.operations_block_readiness(plugin_id) {
            return Err(PluginError::NotReady(
                "a plugin operation is running".to_string(),
            ));
        }

        let (install, manifest, model, requested_backend, identity) =
            self.sidecar_target(plugin_id)?;
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

        // The package can change while the process starts. Never hand out a
        // client that belongs to a package replaced during startup.
        if self.operations_block_readiness(plugin_id)
            || self.cached_sidecar_identity(plugin_id)?.as_ref() != Some(&identity)
        {
            shutdown_sidecar(&client);
            return Err(PluginError::NotReady(
                "the active AI plugin changed while starting the sidecar".to_string(),
            ));
        }
        *slot = Some(CachedSidecar {
            identity,
            client: client.clone(),
        });
        Ok(client)
    }

    fn stored_model_revision(&self, plugin_id: &str) -> Result<String> {
        self.db()
            .plugin_state(plugin_id)?
            .and_then(|state| state.model_revision)
            .ok_or_else(|| PluginError::NotFound(format!("model revision for {plugin_id}")))
    }

    /// Deletes the stored model files and updates the persisted state. The
    /// caller publishes the terminal `PluginStateChanged` after releasing the
    /// lifecycle key.
    fn delete_model_files(&self, plugin_id: &str, revision: &str) -> Result<()> {
        self.stop_sidecar();
        delete_model_revision(&self.data_dir, plugin_id, revision)?;
        self.set_model_state(plugin_id, ModelState::Missing, revision, 0, None, None)?;
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
                continue;
            }
            if state.model_state != ModelState::Ready {
                continue;
            }

            // A `ready` row can outlive the files it describes (external
            // deletion, a crash before verification, or a crashed installer).
            // Re-check them so tagging is not reported ready for a missing or
            // corrupt model.
            let Some(install) = self.db().active_plugin_installation(&plugin_id)? else {
                continue;
            };
            let manifest = match parse_manifest(install.manifest_json.as_bytes()) {
                Ok(manifest) => manifest,
                Err(error) => {
                    self.db().set_plugin_model_state(
                        &plugin_id,
                        ModelState::Error,
                        state.model_revision.as_deref(),
                        state.model_bytes_downloaded,
                        state.model_bytes_total,
                        Some(&error.to_string()),
                    )?;
                    self.events.publish(Event::PluginStateChanged { plugin_id });
                    continue;
                }
            };
            match check_model(&manifest, &self.data_dir) {
                ModelCheck::Ready(_) => {}
                ModelCheck::Missing => {
                    self.db().set_plugin_model_state(
                        &plugin_id,
                        ModelState::Missing,
                        state.model_revision.as_deref(),
                        0,
                        state.model_bytes_total,
                        Some("the model files are missing"),
                    )?;
                    self.events.publish(Event::PluginStateChanged { plugin_id });
                }
                ModelCheck::Invalid(message) => {
                    self.db().set_plugin_model_state(
                        &plugin_id,
                        ModelState::Error,
                        state.model_revision.as_deref(),
                        state.model_bytes_downloaded,
                        state.model_bytes_total,
                        Some(&message),
                    )?;
                    self.events.publish(Event::PluginStateChanged { plugin_id });
                }
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

fn shutdown_sidecar(client: &SidecarClient) {
    if let Err(error) = client.shutdown() {
        tracing::warn!(%error, "AI sidecar shutdown failed; killing it");
        client.kill();
    }
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
        AiHostError, ManifestFile, ModelCheck, ModelHttpClient, check_model, model_dir, plugin_dir,
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
            }
        }
    }

    impl ModelHttpClient for MockModelClient {
        fn download(
            &self,
            url: &str,
            on_chunk: &mut dyn FnMut(&[u8]) -> vrc_photon_ai_host::Result<()>,
        ) -> vrc_photon_ai_host::Result<()> {
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
        package_from_entrypoint(key, entrypoint, variant, backend, extra)
    }

    fn package_from_entrypoint(
        key: &TestKey,
        entrypoint: Vec<u8>,
        variant: &str,
        backend: &str,
        extra: &[TestEntry],
    ) -> TestPackage {
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

        fn wait_for_install_failure(&self) -> String {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.install_rx.recv_timeout(remaining) {
                    Ok(Event::PluginInstallProgress {
                        status: PluginInstallStatus::Failed,
                        error,
                        ..
                    }) => return error.unwrap_or_default(),
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        panic!("timed out waiting for the plugin install failure")
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        panic!("the event bus stopped before the install failed")
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
                !self.service.operation_active(PLUGIN)
                    && self
                        .db()
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
    fn failed_same_version_install_restores_the_previous_package_and_readiness() {
        let ctx = TestContext::new();
        ctx.install_default();
        ctx.make_model_ready();
        assert!(ctx.service.is_ready(PLUGIN));

        let previous = ctx
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .unwrap();
        let previous_path = PathBuf::from(&previous.path);
        let previous_entrypoint = fs::read(previous_path.join(TEST_ENTRYPOINT)).unwrap();
        fs::write(previous_path.join("keep.txt"), b"keep").unwrap();

        let bad = package_from_entrypoint(
            &ctx.key,
            b"#!/bin/sh\nexit 1\n".to_vec(),
            "linux-cpu",
            "cpu",
            &[],
        );
        // The test package has the same version as the installed one.
        assert_eq!(bad.manifest().plugin_version, previous.version);
        let zip = ctx.write_zip("bad-same-version.zip", &bad);

        ctx.service.install_from_zip(&zip).unwrap();
        let error = ctx.wait_for_install_failure();
        assert!(!error.is_empty());

        let restored = ctx
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .unwrap();
        assert_eq!(restored.version, previous.version);
        assert_eq!(restored.variant, previous.variant);
        assert_eq!(restored.path, previous.path);
        assert_eq!(
            fs::read(previous_path.join(TEST_ENTRYPOINT)).unwrap(),
            previous_entrypoint,
            "the old entrypoint must be back in place"
        );
        assert_eq!(fs::read(previous_path.join("keep.txt")).unwrap(), b"keep");
        assert!(ctx.service.is_ready(PLUGIN));
        let client = ctx.service.sidecar().unwrap();
        assert!(client.is_alive());
        assert_eq!(client.hello().variant, "linux-cpu");
    }

    #[test]
    fn failed_different_version_install_keeps_the_previous_package_usable() {
        let ctx = TestContext::new();
        ctx.install_default();
        ctx.make_model_ready();
        assert!(ctx.service.is_ready(PLUGIN));

        let previous = ctx
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .unwrap();
        let previous_path = PathBuf::from(&previous.path);
        let previous_entrypoint = fs::read(previous_path.join(TEST_ENTRYPOINT)).unwrap();
        fs::write(previous_path.join("keep.txt"), b"keep").unwrap();

        let mut bad = package_from_entrypoint(
            &ctx.key,
            b"#!/bin/sh\nexit 1\n".to_vec(),
            "linux-cpu",
            "cpu",
            &[],
        );
        bad.manifest_mut().plugin_version = "2.0.0".to_string();
        let zip = ctx.write_zip("bad-new-version.zip", &bad);
        let failed_path = plugin_dir(&ctx.data_dir, PLUGIN, "linux-cpu", "2.0.0");

        ctx.service.install_from_zip(&zip).unwrap();
        let error = ctx.wait_for_install_failure();
        assert!(!error.is_empty());

        let active = ctx
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .unwrap();
        assert_eq!(active.version, previous.version);
        assert_eq!(active.path, previous.path);
        assert_eq!(
            fs::read(previous_path.join(TEST_ENTRYPOINT)).unwrap(),
            previous_entrypoint
        );
        assert_eq!(fs::read(previous_path.join("keep.txt")).unwrap(), b"keep");
        assert!(!failed_path.exists(), "the failed package must be removed");
        assert!(
            ctx.db()
                .list_plugin_installations(PLUGIN)
                .unwrap()
                .iter()
                .all(|install| install.version != "2.0.0"),
            "the failed installation row must be removed"
        );
        assert!(ctx.service.is_ready(PLUGIN));
        assert!(ctx.service.sidecar().unwrap().is_alive());
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
    fn switching_variants_clears_the_probe_cache() {
        let ctx = TestContext::new();
        ctx.install_default();

        let vulkan = ctx.package("linux-vulkan", "vulkan", &[]);
        let zip = ctx.write_zip("vulkan-cache.zip", &vulkan);
        ctx.service.install_from_zip(&zip).unwrap();
        ctx.wait_for_install();
        ctx.make_model_ready();
        assert!(ctx.service.is_ready(PLUGIN));

        ctx.service
            .set_active_variant(PLUGIN, "linux-cpu", None)
            .unwrap();
        assert!(
            !ctx.service.is_ready(PLUGIN),
            "a new active variant must be probed again"
        );
        assert!(
            ctx.db()
                .plugin_state(PLUGIN)
                .unwrap()
                .unwrap()
                .probe_json
                .is_none()
        );

        ctx.service.probe(PLUGIN).unwrap();
        assert!(ctx.service.is_ready(PLUGIN));
    }

    #[test]
    fn readiness_requires_the_active_model_revision() {
        let ctx = TestContext::new();
        ctx.install_default();
        ctx.make_model_ready();
        assert!(ctx.service.is_ready(PLUGIN));

        ctx.db()
            .set_plugin_model_state(
                PLUGIN,
                ModelState::Ready,
                Some("other-revision"),
                11,
                Some(11),
                None,
            )
            .unwrap();
        assert!(!ctx.service.is_ready(PLUGIN));

        let revision = sample_manifest().model.revision;
        ctx.db()
            .set_plugin_model_state(
                PLUGIN,
                ModelState::Ready,
                Some(&revision),
                11,
                Some(11),
                None,
            )
            .unwrap();
        assert!(ctx.service.is_ready(PLUGIN));
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
    fn delete_model_uses_the_stored_revision() {
        let ctx = TestContext::new();
        ctx.install_default();
        let manifest_dir = model_dir(&ctx.data_dir, PLUGIN, &sample_manifest().model.revision);
        fs::create_dir_all(&manifest_dir).unwrap();

        let stored_revision = "stored-revision";
        let stored_dir = model_dir(&ctx.data_dir, PLUGIN, stored_revision);
        fs::create_dir_all(&stored_dir).unwrap();
        fs::write(stored_dir.join(MODEL_FILE), MODEL_BYTES).unwrap();
        fs::write(stored_dir.join(MMPROJ_FILE), MMPROJ_BYTES).unwrap();
        ctx.db()
            .set_plugin_model_state(
                PLUGIN,
                ModelState::Ready,
                Some(stored_revision),
                11,
                Some(11),
                None,
            )
            .unwrap();

        ctx.service.delete_model(PLUGIN).unwrap();

        assert!(!stored_dir.exists());
        assert!(
            manifest_dir.exists(),
            "delete_model must follow plugin_state.model_revision"
        );
        let state = ctx.db().plugin_state(PLUGIN).unwrap().unwrap();
        assert_eq!(state.model_state, ModelState::Missing);
        assert_eq!(state.model_revision.as_deref(), Some(stored_revision));
    }

    #[test]
    fn delete_model_works_after_uninstall() {
        let ctx = TestContext::new();
        ctx.install_default();
        ctx.make_model_ready();
        let directory = model_dir(&ctx.data_dir, PLUGIN, &sample_manifest().model.revision);
        assert!(directory.exists());

        ctx.service.uninstall(PLUGIN, None, None, false).unwrap();
        assert!(
            directory.exists(),
            "uninstall keeps model files for a later delete"
        );

        ctx.service.delete_model(PLUGIN).unwrap();

        assert!(!directory.exists());
        assert_eq!(
            ctx.db().plugin_state(PLUGIN).unwrap().unwrap().model_state,
            ModelState::Missing
        );
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
    fn lifecycle_and_model_starts_reject_while_an_install_runs() {
        let ctx = TestContext::new();
        ctx.set_consent();
        let package = ctx.package(
            "linux-cpu",
            "cpu",
            &[TestEntry::new("padding.bin", vec![7u8; 8 * 1024 * 1024])],
        );
        let zip = ctx.write_zip("busy-lifecycle.zip", &package);

        ctx.service.install_from_zip(&zip).unwrap();
        let probe_error = ctx.service.probe(PLUGIN).unwrap_err();
        let model_error = ctx.service.start_model_download(PLUGIN).unwrap_err();

        assert!(
            matches!(probe_error, PluginError::Busy(_)),
            "{probe_error:?}"
        );
        assert!(
            matches!(model_error, PluginError::Busy(_)),
            "{model_error:?}"
        );
        ctx.wait_for_install();
    }

    #[test]
    fn lifecycle_operations_reject_while_a_model_download_runs() {
        let ctx = TestContext::new();
        ctx.install_default();
        ctx.set_consent();

        ctx.service.start_model_download(PLUGIN).unwrap();
        let error = ctx.service.probe(PLUGIN).unwrap_err();

        assert!(matches!(error, PluginError::Busy(_)), "{error:?}");
        ctx.wait_for_model_state(ModelState::Ready);
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
    fn recover_interrupted_model_state_downgrades_a_stale_ready_state() {
        let ctx = TestContext::new();
        ctx.install_default();
        ctx.make_model_ready();
        assert!(ctx.service.is_ready(PLUGIN));
        let revision = sample_manifest().model.revision;
        let directory = model_dir(&ctx.data_dir, PLUGIN, &revision);

        fs::remove_dir_all(&directory).unwrap();
        ctx.service.recover_interrupted_model_state().unwrap();
        let state = ctx.db().plugin_state(PLUGIN).unwrap().unwrap();
        assert_eq!(state.model_state, ModelState::Missing);
        assert!(!ctx.service.is_ready(PLUGIN));

        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(MODEL_FILE), b"bad").unwrap();
        fs::write(directory.join(MMPROJ_FILE), MMPROJ_BYTES).unwrap();
        ctx.db()
            .set_plugin_model_state(
                PLUGIN,
                ModelState::Ready,
                Some(&revision),
                11,
                Some(11),
                None,
            )
            .unwrap();
        ctx.service.recover_interrupted_model_state().unwrap();
        let state = ctx.db().plugin_state(PLUGIN).unwrap().unwrap();
        assert_eq!(state.model_state, ModelState::Error);
        assert!(state.model_error.is_some());
        assert!(!ctx.service.is_ready(PLUGIN));
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
    fn async_probe_runs_in_the_background() {
        let ctx = TestContext::new();
        ctx.install_default();
        ctx.make_model_ready();
        ctx.db().set_plugin_probe_json(PLUGIN, None).unwrap();
        assert!(!ctx.service.is_ready(PLUGIN));

        ctx.service.probe_async(PLUGIN).unwrap();

        ctx.wait_for("the background probe", || ctx.service.is_ready(PLUGIN));
    }

    #[test]
    fn async_probe_returns_busy_while_the_worker_holds_the_key() {
        let ctx = TestContext::new();
        ctx.install_default();
        let install = ctx
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .unwrap();
        let executable = Path::new(&install.path).join(TEST_ENTRYPOINT);
        fs::write(&executable, b"#!/bin/sh\nsleep 2\nexit 1\n").unwrap();

        ctx.service.probe_async(PLUGIN).unwrap();
        let error = ctx.service.probe_async(PLUGIN).unwrap_err();
        assert!(matches!(error, PluginError::Busy(_)), "{error:?}");
        ctx.wait_for("the background probe", || {
            !ctx.service.operation_active(PLUGIN)
        });
    }

    #[test]
    fn async_variant_switch_updates_the_row_before_the_sidecar_work() {
        let ctx = TestContext::new();
        ctx.install_default();
        let vulkan = ctx.package("linux-vulkan", "vulkan", &[]);
        let zip = ctx.write_zip("vulkan-async.zip", &vulkan);
        ctx.service.install_from_zip(&zip).unwrap();
        ctx.wait_for_install();
        ctx.wait_for_installation_count(2);
        ctx.service
            .set_active_variant(PLUGIN, "linux-cpu", None)
            .unwrap();
        ctx.make_model_ready();
        ctx.service.probe(PLUGIN).unwrap();
        assert!(ctx.service.is_ready(PLUGIN));

        let events = ctx.events.subscribe();
        ctx.service
            .set_active_variant_async(PLUGIN, "linux-vulkan", None)
            .unwrap();

        let active = ctx
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .unwrap();
        assert_eq!(active.variant, "linux-vulkan");
        ctx.wait_for("the background variant probe", || {
            ctx.service.is_ready(PLUGIN)
        });
        assert_eq!(
            ctx.service.sidecar().unwrap().hello().variant,
            "linux-vulkan"
        );
        ctx.collect_events(&events, |events| {
            events.iter().any(|event| {
                matches!(event, Event::PluginStateChanged { plugin_id } if plugin_id == PLUGIN)
            })
        });
    }

    #[test]
    fn async_delete_model_removes_files_in_the_background() {
        let ctx = TestContext::new();
        let error = ctx.service.delete_model_async(PLUGIN).unwrap_err();
        assert!(matches!(error, PluginError::NotFound(_)), "{error:?}");

        ctx.install_default();
        ctx.make_model_ready();
        let directory = model_dir(&ctx.data_dir, PLUGIN, &sample_manifest().model.revision);
        assert!(directory.exists());

        ctx.service.delete_model_async(PLUGIN).unwrap();

        ctx.wait_for("the background model delete", || {
            !directory.exists()
                && ctx.db().plugin_state(PLUGIN).unwrap().unwrap().model_state
                    == ModelState::Missing
        });
        assert!(!ctx.service.is_ready(PLUGIN));
    }

    #[test]
    fn async_delete_model_publishes_state_changed_on_failure() {
        let ctx = TestContext::new();
        ctx.install_default();
        let manifest = sample_manifest();
        let directory = model_dir(&ctx.data_dir, PLUGIN, &manifest.model.revision);
        fs::create_dir_all(directory.parent().unwrap()).unwrap();
        // A regular file where a directory is expected makes remove_dir_all
        // fail deterministically, including when the test runs as root.
        fs::write(&directory, b"not a directory").unwrap();
        ctx.db()
            .set_plugin_model_state(
                PLUGIN,
                ModelState::Ready,
                Some(&manifest.model.revision),
                11,
                Some(11),
                None,
            )
            .unwrap();

        let events = ctx.events.subscribe();
        ctx.service.delete_model_async(PLUGIN).unwrap();

        let events = ctx.collect_events(&events, |events| {
            events.iter().any(|event| {
                matches!(event, Event::PluginStateChanged { plugin_id } if plugin_id == PLUGIN)
            })
        });
        assert!(events.iter().any(|event| {
            matches!(event, Event::PluginStateChanged { plugin_id } if plugin_id == PLUGIN)
        }));
        assert!(
            !ctx.service.operation_active(PLUGIN),
            "the failure event must be published after the guard is released"
        );
    }

    #[test]
    fn async_uninstall_publishes_state_changed_on_failure() {
        let ctx = TestContext::new();
        ctx.install_default();
        let path = PathBuf::from(
            &ctx.db()
                .active_plugin_installation(PLUGIN)
                .unwrap()
                .unwrap()
                .path,
        );
        fs::remove_dir_all(&path).unwrap();
        // A regular file where the install directory is expected makes
        // remove_dir_all fail deterministically, including as root.
        fs::write(&path, b"not a directory").unwrap();

        let events = ctx.events.subscribe();
        ctx.service
            .uninstall_async(PLUGIN, None, None, false)
            .unwrap();

        let events = ctx.collect_events(&events, |events| {
            events.iter().any(|event| {
                matches!(event, Event::PluginStateChanged { plugin_id } if plugin_id == PLUGIN)
            })
        });
        assert!(events.iter().any(|event| {
            matches!(event, Event::PluginStateChanged { plugin_id } if plugin_id == PLUGIN)
        }));
        assert!(
            !ctx.service.operation_active(PLUGIN),
            "the failure event must be published after the guard is released"
        );
        assert!(
            ctx.db()
                .active_plugin_installation(PLUGIN)
                .unwrap()
                .is_some(),
            "a failed uninstall must leave the rows intact"
        );
    }

    #[test]
    fn async_uninstall_rejects_unknown_and_removes_in_the_background() {
        let ctx = TestContext::new();
        let error = ctx
            .service
            .uninstall_async(PLUGIN, None, None, true)
            .unwrap_err();
        assert!(matches!(error, PluginError::NotFound(_)), "{error:?}");

        ctx.install_default();
        let install_path = PathBuf::from(
            &ctx.db()
                .active_plugin_installation(PLUGIN)
                .unwrap()
                .unwrap()
                .path,
        );

        ctx.service
            .uninstall_async(PLUGIN, None, None, true)
            .unwrap();

        ctx.wait_for("the background uninstall", || {
            ctx.db()
                .active_plugin_installation(PLUGIN)
                .unwrap()
                .is_none()
                && !install_path.exists()
        });
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
