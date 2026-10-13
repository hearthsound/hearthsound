//! The background jobs the composition root registers.
//!
//! A job handler in this module is the server's side of work whose owner crate
//! has no dependency on `hearthsound-jobs`, or whose kind belongs to no single
//! domain crate. The library scanner's handler lives in `hearthsound-library`
//! because scanning is that crate's work; the plugin key-value sweep lives here
//! because the host owns the `plugin_kv` table but may not depend on the queue.

use std::sync::Arc;

use async_trait::async_trait;
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler};
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
