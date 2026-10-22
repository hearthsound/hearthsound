//! The background check for newer versions of the installed plugins.
//!
//! [`UpdateChecker`] answers one question per installed plugin: does the index
//! the plugin came from publish a newer version this server can host? An
//! official or bundled plugin is checked against the official index, a plugin an
//! administrator installed from a repository against that repository, and a
//! sideloaded plugin against nothing, because a file someone uploaded has no
//! index that would know about it.
//!
//! [`UpdateCheckJobHandler`] runs the check on the system schedule and holds
//! what the last completed check found, so an administrative view can report the
//! pending updates without reading every index again. A check is due on
//! `plugins.update_check_hours`; zero switches the checks off. When
//! `plugins.auto_update` is on, every update the check finds is installed - and
//! an upgrade that asks for a permission the installed version does not hold
//! still waits for review, because the install path stages it instead of
//! switching it in.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_jobs::{Job, JobError, JobHandler};
use uuid::Uuid;

use crate::index::Index;
use crate::installer::{PluginInstaller, RepositoryRef};
use crate::repo::{PluginRepository, PluginRow, PluginSource, now_millis};

/// The job kind of the plugin update check.
pub const UPDATE_CHECK_JOB_KIND: &str = "plugins.update_check";

/// The cron expression of the plugin update check: once an hour, on the hour.
///
/// The schedule only decides how often the check is offered; how many hours must
/// pass between two checks is `plugins.update_check_hours`, so the hourly tick
/// can be turned into a daily or weekly check without changing the schedule.
pub const UPDATE_CHECK_SCHEDULE: &str = "0 * * * *";

/// One installed plugin a newer version is published for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpdate {
    /// The plugin id.
    pub plugin_id: String,
    /// The version installed now.
    pub current: String,
    /// The newest version the plugin's index publishes that this server hosts.
    pub version: String,
    /// Where the installed plugin came from.
    pub source: PluginSource,
    /// The repository that publishes the update, `None` for the official index.
    pub repository_id: Option<Uuid>,
}

impl PluginUpdate {
    /// Returns the reference the update is installed through.
    ///
    /// # Returns
    ///
    /// The repository the update came from, or the official index.
    fn reference(&self) -> RepositoryRef {
        match self.repository_id {
            Some(id) => RepositoryRef::Id(id),
            None => RepositoryRef::Official,
        }
    }
}

/// What one completed update check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheckReport {
    /// When the check completed, in unix milliseconds.
    pub checked_at: i64,
    /// The newer versions the check found, in the order of the installed
    /// plugins.
    pub updates: Vec<PluginUpdate>,
}

/// Looks for newer versions of the installed plugins.
///
/// One checker serves every source: it reads the installed rows, reads each
/// index once per check and picks the newest version each plugin's entry offers
/// that targets the API this server hosts.
pub struct UpdateChecker {
    /// The installer every upgrade is installed and verified with.
    installer: Arc<PluginInstaller>,
    /// The installed plugin rows and the staged upgrades.
    repository: PluginRepository,
}

impl UpdateChecker {
    /// Creates a checker.
    ///
    /// # Parameters
    ///
    /// - `installer`: the installer an automatic update is applied through.
    /// - `repository`: the installed plugin rows and their staged upgrades.
    pub fn new(installer: Arc<PluginInstaller>, repository: PluginRepository) -> UpdateChecker {
        UpdateChecker {
            installer,
            repository,
        }
    }

    /// Returns the settings the check reads its gates from.
    ///
    /// The snapshot is the plugin host's, so `plugins.*` settings are read the
    /// same way the host reads them.
    pub(crate) fn settings(&self) -> &Arc<SettingsStore> {
        self.installer.settings()
    }

    /// Lists the newer versions of the installed plugins.
    ///
    /// Every index is read at most once, and an index that cannot be read
    /// contributes no updates to this check: the failure is logged and the next
    /// check tries again, so one unreachable repository cannot stop an
    /// administrator from seeing the updates of the others.
    ///
    /// # Returns
    ///
    /// One update per installed plugin whose index publishes a newer compatible
    /// version, in the order the installed rows are read. A plugin at the newest
    /// version, a plugin whose index does not list it any more, and a sideloaded
    /// plugin are not reported.
    pub async fn check(&self) -> Vec<PluginUpdate> {
        let installed = match self.repository.plugins().await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "could not read the installed plugins for an update check");
                return Vec::new();
            }
        };

        let mut indexes: HashMap<SourceKey, Option<Index>> = HashMap::new();
        let mut updates = Vec::new();
        for row in &installed {
            let Some((key, source)) = SourceKey::of(row) else {
                continue;
            };
            if let Entry::Vacant(slot) = indexes.entry(key) {
                let index = match self.installer.resolve(Some(key.reference())).await {
                    Ok(resolved) => {
                        tracing::debug!(index = %resolved.url, "read a plugin index for an update check");
                        Some(resolved.index)
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not read a plugin index for an update check");
                        None
                    }
                };
                slot.insert(index);
            }
            let Some(entry) = indexes
                .get(&key)
                .and_then(Option::as_ref)
                .and_then(|index| index.plugin(&row.id))
            else {
                continue;
            };
            let Some(newest) = entry.newest_compatible() else {
                continue;
            };
            if !is_newer(&newest.version, &row.version) {
                continue;
            }
            updates.push(PluginUpdate {
                plugin_id: row.id.clone(),
                current: row.version.clone(),
                version: newest.version.clone(),
                source,
                repository_id: row.repository_id,
            });
        }
        updates
    }

    /// Installs the updates a check found, when automatic updates are on.
    ///
    /// An upgrade that requests a permission the installed version does not hold
    /// is staged for review instead of being switched in, which is what the
    /// install path does for every new version; the plugin keeps serving the
    /// version it runs until an administrator applies the staged one.
    ///
    /// # Parameters
    ///
    /// - `updates`: the updates a [`UpdateChecker::check`] found. They are passed
    ///   in rather than looked up again, so one check reads each index once.
    ///
    /// # Returns
    ///
    /// The updates the install path took: the ones now in service and the ones
    /// waiting for review. Empty when `plugins.auto_update` is off, and an
    /// update that is already staged is not attempted again. An update that
    /// cannot be installed is logged and left for an administrator, and the
    /// remaining updates are still installed.
    pub async fn apply_auto_updates(&self, updates: &[PluginUpdate]) -> Vec<PluginUpdate> {
        if updates.is_empty() {
            return Vec::new();
        }
        match self.settings().get(keys::PLUGINS_AUTO_UPDATE) {
            Ok(true) => {}
            Ok(false) => return Vec::new(),
            Err(error) => {
                tracing::warn!(%error, "could not read plugins.auto_update");
                return Vec::new();
            }
        }

        let mut applied = Vec::new();
        for update in updates {
            if self.is_staged(update).await {
                continue;
            }
            match self
                .installer
                .install_from_index(
                    &update.plugin_id,
                    Some(&update.version),
                    Some(update.reference()),
                )
                .await
            {
                Ok(_) => {
                    if self.is_staged(update).await {
                        tracing::info!(
                            plugin = %update.plugin_id,
                            version = %update.version,
                            "staged a plugin update for review"
                        );
                    } else {
                        tracing::info!(
                            plugin = %update.plugin_id,
                            version = %update.version,
                            "installed a plugin update"
                        );
                    }
                    applied.push(update.clone());
                }
                Err(error) => {
                    tracing::warn!(
                        plugin = %update.plugin_id,
                        version = %update.version,
                        %error,
                        "could not install a plugin update"
                    );
                }
            }
        }
        applied
    }

    /// Reports whether the update is already waiting for review.
    ///
    /// # Returns
    ///
    /// `true` when the plugin's staged upgrade is this version, so installing it
    /// again would only rewrite the files that are already there.
    async fn is_staged(&self, update: &PluginUpdate) -> bool {
        match self.repository.upgrade(&update.plugin_id).await {
            Ok(staged) => staged.is_some_and(|staged| staged.version == update.version),
            Err(error) => {
                tracing::warn!(
                    plugin = %update.plugin_id,
                    %error,
                    "could not read a staged plugin upgrade"
                );
                true
            }
        }
    }
}

/// The index one installed plugin is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SourceKey {
    /// The official index.
    Official,
    /// One repository an administrator registered.
    Repository(Uuid),
}

impl SourceKey {
    /// Returns the index to check one installed row against.
    ///
    /// # Parameters
    ///
    /// - `row`: the installed plugin row.
    ///
    /// # Returns
    ///
    /// The index and the source the plugin was installed from, or `None` for a
    /// sideloaded plugin and for a plugin whose repository has been removed,
    /// neither of which has an index to check.
    fn of(row: &PluginRow) -> Option<(SourceKey, PluginSource)> {
        let source = row.source.parse::<PluginSource>().ok()?;
        match source {
            PluginSource::Official | PluginSource::Bundled => Some((SourceKey::Official, source)),
            PluginSource::Repository => row
                .repository_id
                .map(|id| (SourceKey::Repository(id), source)),
            PluginSource::Sideload => None,
        }
    }

    /// Returns the reference the index is read through.
    fn reference(self) -> RepositoryRef {
        match self {
            SourceKey::Official => RepositoryRef::Official,
            SourceKey::Repository(id) => RepositoryRef::Id(id),
        }
    }
}

/// Runs the plugin update check on the system schedule.
///
/// The handler holds what the last completed check found, which is what an
/// administrative view reports as the updates each plugin has waiting.
pub struct UpdateCheckJobHandler {
    /// The checker each run uses.
    checker: Arc<UpdateChecker>,
    /// What the last completed check found, `None` before the first one.
    last: Mutex<Option<UpdateCheckReport>>,
}

impl UpdateCheckJobHandler {
    /// Creates the handler.
    ///
    /// # Parameters
    ///
    /// - `checker`: the checker every run uses.
    pub fn new(checker: Arc<UpdateChecker>) -> UpdateCheckJobHandler {
        UpdateCheckJobHandler {
            checker,
            last: Mutex::new(None),
        }
    }

    /// Returns what the last completed check found.
    ///
    /// # Returns
    ///
    /// The report, or `None` when no check has completed in this process.
    pub fn last_check(&self) -> Option<UpdateCheckReport> {
        lock(&self.last).clone()
    }

    /// Runs a check when one is due.
    ///
    /// A check is due when `plugins.update_check_hours` is positive and at least
    /// that many hours passed since the last completed check. The first check of
    /// a process is therefore due unless the setting is off, which is what makes
    /// the startup check run and what makes a zero interval stop the checks.
    ///
    /// # Returns
    ///
    /// The report of the check that ran, or `None` when none was due.
    pub async fn check_if_due(&self) -> Option<UpdateCheckReport> {
        let hours = match self
            .checker
            .settings()
            .get(keys::PLUGINS_UPDATE_CHECK_HOURS)
        {
            Ok(hours) => hours,
            Err(error) => {
                tracing::warn!(%error, "could not read plugins.update_check_hours");
                return None;
            }
        };
        if hours <= 0 {
            return None;
        }

        let interval = hours.saturating_mul(3_600_000);
        let now = now_millis();
        if let Some(last) = self.last_check()
            && now.saturating_sub(last.checked_at) < interval
        {
            return None;
        }
        Some(self.run(now).await)
    }

    /// Runs one check, applies the automatic updates, and records the result.
    ///
    /// # Parameters
    ///
    /// - `checked_at`: when the check started, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// What the check found.
    async fn run(&self, checked_at: i64) -> UpdateCheckReport {
        let updates = self.checker.check().await;
        self.checker.apply_auto_updates(&updates).await;
        let report = UpdateCheckReport {
            checked_at,
            updates,
        };
        *lock(&self.last) = Some(report.clone());
        report
    }
}

#[async_trait]
impl JobHandler for UpdateCheckJobHandler {
    /// Runs one check when one is due.
    ///
    /// The job never fails: an index that cannot be read and an update that
    /// cannot be installed are logged by the check and tried again on the next
    /// run, and a plugin whose update failed keeps serving the version it runs.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        match self.check_if_due().await {
            Some(report) => tracing::debug!(
                kind = %job.kind,
                updates = report.updates.len(),
                "checked for plugin updates"
            ),
            None => tracing::debug!(kind = %job.kind, "no plugin update check was due"),
        }
        Ok(())
    }
}

/// Locks a mutex, ignoring poisoning.
///
/// A poisoned lock means another thread panicked while holding it; the report it
/// holds is still structurally sound, and refusing to run would turn one panic
/// into a check that never runs again.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Reports whether `candidate` is a newer version than `installed`.
///
/// # Parameters
///
/// - `candidate`: the version the index publishes.
/// - `installed`: the version in service.
///
/// # Returns
///
/// `true` when both versions are semantic versions and `candidate` is the newer
/// one. A version that cannot be read as a semantic version is never newer, so
/// the plugin it belongs to is left alone.
fn is_newer(candidate: &str, installed: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(installed),
    ) {
        (Ok(candidate), Ok(installed)) => candidate > installed,
        _ => false,
    }
}
