//! The background jobs the composition root registers.
//!
//! A job handler in this module is the server's side of work whose owner crate
//! has no dependency on `hearthsound-jobs`, or whose kind belongs to no single
//! domain crate. The library scanner's handler lives in `hearthsound-library`
//! because scanning is that crate's work; the plugin key-value sweep lives here
//! because the host owns the `plugin_kv` table and the sweep needs nothing but
//! the database handle the composition root already holds, and the plugin health
//! check lives here because it drives the provider registry the composition root
//! builds.

use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use hearthsound_catalog::{
    CatalogService, DiscoveryService, EnrichJobPayload, TOP_TRACKS_CACHE_LIMIT,
};
use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::{Audience, CatalogEnriched, EventBus, ServerEvent};
use hearthsound_core::{ClientId, Layer};
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler};
use hearthsound_plugin_host::ProviderRegistry;
use hearthsound_plugin_host::kv;

/// The job kind that deletes expired rows from `plugin_kv`.
pub const PLUGIN_KV_SWEEP_JOB_KIND: &str = "plugin.kv.sweep";

/// The cron expression of the sweep: once an hour, on the hour.
///
/// An entry that a plugin stored with a time to live keeps its expiry date
/// either way; the sweep only reclaims the quota it occupied, so an hourly
/// cadence is frequent enough to keep the store from growing without waking the
/// process often.
pub const PLUGIN_KV_SWEEP_CRON: &str = "0 * * * *";

/// The job kind that probes every provider instance's health.
pub const PLUGIN_HEALTH_JOB_KIND: &str = "plugin.health";

/// The cron expression of the health check: every fifteen minutes.
///
/// The interval an administrator configures is applied inside the handler, so a
/// quarter-hour schedule is the finest granularity the setting can express and a
/// setting of an hour simply skips three ticks.
pub const PLUGIN_HEALTH_SCHEDULE: &str = "*/15 * * * *";

/// Fetches one entity's missing catalog extras and announces that they arrived.
///
/// The job is request-driven: a page read enqueues it when it answers from the
/// library and the cache alone, and nothing schedules it. A run is one
/// [`CatalogService::enrich`] call, an optional discovery top-tracks warm-up for
/// the artist it established, and one `catalog.enriched` event for the listener
/// whose page asked.
pub struct EnrichJobHandler {
    /// The catalog layer that does the provider work.
    catalog: Arc<CatalogService>,
    /// The discovery layer a personal top-tracks warm-up uses.
    discovery: Arc<DiscoveryService>,
    /// The bus the completion is published on.
    events: Arc<EventBus>,
}

impl EnrichJobHandler {
    /// Builds the handler over the layers it drives.
    ///
    /// # Parameters
    ///
    /// - `catalog`: the catalog layer.
    /// - `discovery`: the discovery layer, warmed when an artist was resolved.
    /// - `events`: the event bus.
    pub fn new(
        catalog: Arc<CatalogService>,
        discovery: Arc<DiscoveryService>,
        events: Arc<EventBus>,
    ) -> EnrichJobHandler {
        EnrichJobHandler {
            catalog,
            discovery,
            events,
        }
    }
}

#[async_trait]
impl JobHandler for EnrichJobHandler {
    /// Fetches what the payload names and publishes the completion.
    ///
    /// A missing or malformed payload and an id that names no entity are
    /// permanent failures: the runner retires the job instead of retrying it. A
    /// provider failure is returned so the run is retried on its schedule.
    ///
    /// # Parameters
    ///
    /// - `job`: the claimed job, whose payload names the entity and the listener.
    ///
    /// # Errors
    ///
    /// Returns [`JobError`] when the payload is unusable and when the catalog
    /// work failed.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let payload: EnrichJobPayload = job
            .payload_json::<EnrichJobPayload>()
            .map_err(|error| JobError::new(format!("catalog.enrich payload: {error}")))?
            .ok_or_else(|| JobError::new("catalog.enrich carries no payload"))?;
        let id = ClientId::parse(&payload.entity_id)
            .map_err(|error| JobError::new(format!("catalog.enrich entity id: {error}")))?;
        let report = self
            .catalog
            .enrich(&id)
            .await
            .map_err(|error| JobError::new(error.to_string()))?;
        if let Some(seed) = &report.artist
            && !self
                .catalog
                .registry()
                .layers(Layer::Discovery)
                .is_empty()
        {
            self.discovery
                .artist_top_tracks(payload.user_id, seed, TOP_TRACKS_CACHE_LIMIT)
                .await
                .map(|_| ())
                .unwrap_or_else(|error| {
                    tracing::warn!(
                        %error,
                        entity = %payload.entity_id,
                        "a discovery instance could not warm an artist's top tracks"
                    )
                });
        }
        self.catalog
            .finish_enrichment(&id, payload.user_id)
            .await;
        self.events.publish_for(
            Audience::User(payload.user_id),
            ServerEvent::CatalogEnriched(CatalogEnriched {
                entity_id: payload.entity_id.clone(),
                kind: payload.kind,
            }),
        );
        tracing::debug!(
            kind = %job.kind,
            entity = %payload.entity_id,
            enriched = report.enriched,
            "enriched a catalog entity"
        );
        Ok(())
    }
}

/// Deletes the `plugin_kv` rows whose time to live has passed.
///
/// The handler runs the sweep on the writer pool, so it can never delete a row
/// another transaction is writing.
pub struct KvSweepJobHandler {
    /// The database the expired rows are deleted from.
    database: Arc<Database>,
}

impl KvSweepJobHandler {
    /// Builds the handler over an open database.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database whose `plugin_kv` rows are swept.
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
    }
}

#[async_trait]
impl JobHandler for KvSweepJobHandler {
    /// Sweeps every expired entry and reports how many were deleted.
    ///
    /// # Parameters
    ///
    /// - `job`: the claimed job, used for its kind in the log line.
    ///
    /// # Errors
    ///
    /// Returns [`JobError`] when the delete fails, which the queue then retries
    /// on its retry schedule.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        match kv::sweep_expired(self.database.writer()).await {
            Ok(swept) => {
                tracing::debug!(kind = %job.kind, swept, "swept expired plugin key-value rows");
                Ok(())
            }
            Err(error) => Err(JobError::new(error.to_string())),
        }
    }
}

/// Probes the health of every provider instance in service.
///
/// The check is the same one `GET /api/v1/admin/providers/{slug}/health` runs,
/// through the same registry call, so a scheduled result is exactly what the
/// manual check publishes. The interval comes from
/// `plugins.health_check_minutes` and is applied here rather than in the cron
/// expression, so changing it needs no schedule rewrite and a value of 0 stops
/// the periodic checks while creating or enabling an instance still checks it.
pub struct PluginHealthJobHandler {
    /// The registry whose instances are probed.
    registry: Arc<ProviderRegistry>,
    /// The runtime settings, read again at every run.
    settings: Settings,
    /// When the last check ran in this process, in unix milliseconds.
    last: Mutex<Option<i64>>,
}

impl PluginHealthJobHandler {
    /// Builds the handler over a registry and the runtime settings.
    ///
    /// # Parameters
    ///
    /// - `registry`: the provider registry whose instances are probed.
    /// - `settings`: the runtime settings the interval is read from.
    pub fn new(registry: Arc<ProviderRegistry>, settings: Settings) -> PluginHealthJobHandler {
        PluginHealthJobHandler {
            registry,
            settings,
            last: Mutex::new(None),
        }
    }

    /// Returns whether a check is due, and records that one ran at `now`.
    ///
    /// The first run of a process is due unless the setting is off, so a restart
    /// does not leave stale results on the Providers page for a whole interval.
    ///
    /// # Parameters
    ///
    /// - `now`: the current time in unix milliseconds.
    ///
    /// # Returns
    ///
    /// Whether a check should run.
    pub fn check_if_due(&self, now: i64) -> bool {
        let minutes = match self
            .settings
            .current()
            .get(keys::PLUGINS_HEALTH_CHECK_MINUTES)
        {
            Ok(minutes) => minutes,
            Err(error) => {
                tracing::warn!(%error, "could not read plugins.health_check_minutes");
                return false;
            }
        };
        let mut last = lock(&self.last);
        let due = health_check_due(minutes, *last, now);
        if due {
            *last = Some(now);
        }
        due
    }

    /// Probes every instance in service and records each outcome.
    ///
    /// # Parameters
    ///
    /// - `now`: the current time in unix milliseconds.
    ///
    /// # Returns
    ///
    /// How many instances answered.
    pub async fn run(&self, now: i64) -> usize {
        let mut checked = 0usize;
        for instance in self.registry.instances() {
            let slug = instance.slug().as_str();
            match self.registry.check_health(slug).await {
                Ok(Some(outcome)) => {
                    checked += 1;
                    tracing::debug!(
                        instance = slug,
                        ok = outcome.ok,
                        "recorded a provider health result"
                    );
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    instance = slug,
                    %error,
                    "could not record a provider health result"
                ),
            }
        }
        if checked > 0 {
            tracing::info!(instances = checked, at = now, "checked provider health");
        }
        checked
    }
}

#[async_trait]
impl JobHandler for PluginHealthJobHandler {
    /// Runs one check when one is due.
    ///
    /// The job never fails: an instance whose probe fails records the failure
    /// where the admin pages read it, and the next run tries again.
    ///
    /// # Parameters
    ///
    /// - `job`: the claimed job, used for its kind in the log line.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let now = unix_millis();
        if !self.check_if_due(now) {
            tracing::debug!(kind = %job.kind, "no provider health check is due");
            return Ok(());
        }
        self.run(now).await;
        Ok(())
    }
}

/// Returns whether a periodic health check is due.
///
/// # Parameters
///
/// - `minutes`: the configured interval, `0` disabling the periodic checks.
/// - `last`: when the previous check ran, in unix milliseconds.
/// - `now`: the current time in unix milliseconds.
///
/// # Returns
///
/// Whether a check should run.
pub(crate) fn health_check_due(minutes: i64, last: Option<i64>, now: i64) -> bool {
    if minutes <= 0 {
        return false;
    }
    let interval = minutes.saturating_mul(60_000);
    match last {
        Some(last) => now.saturating_sub(last) >= interval,
        None => true,
    }
}

/// Returns the current time in unix milliseconds.
fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Locks a mutex, ignoring poisoning.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_interval_disables_the_periodic_check() {
        assert!(!health_check_due(0, None, 1_000));
        assert!(!health_check_due(0, Some(1), 1_000_000_000));
        assert!(!health_check_due(-5, None, 1_000));
    }

    #[test]
    fn a_check_is_due_when_the_interval_has_passed() {
        // The first run of a process is due.
        assert!(health_check_due(15, None, 1_000));
        // Fifteen minutes later it is due again, one millisecond earlier not.
        assert!(!health_check_due(15, Some(1_000), 1_000 + 900_000 - 1));
        assert!(health_check_due(15, Some(1_000), 1_000 + 900_000));
    }
}
