//! The scrobble outbox: durable delivery to the services a listener linked.
//!
//! Recording a play, reporting now playing, and liking, disliking or clearing
//! an opinion all produce scrobbles for the services the listener linked their
//! account on. A service can be down, slow, or rate limiting, and a play must
//! not be lost because of it, so every scrobble is written to
//! `scrobble_outbox` first and delivered later by [`ScrobbleDeliverer`].
//!
//! # What is enqueued
//!
//! [`ScrobbleOutbox::listen`] writes one row per linked scrobble instance for
//! a recorded play and marks the play as scrobbled;
//! [`ScrobbleOutbox::now_playing`] replaces the listener's pending now-playing
//! row per instance, because only the newest one is worth sending; and
//! [`ScrobbleOutbox::feedback`] writes a love, hate or clear report for the
//! instances whose limits say they accept feedback. A listener with no link to
//! an instance produces no row for it.
//!
//! # How rows are delivered
//!
//! [`ScrobbleDeliverer::deliver_due`] takes every pending row whose
//! `next_attempt_at` has passed and delivers it through the instance's
//! [`ScrobbleProvider`]: listens in batches up to the service's maximum, and
//! now-playing reports and feedback one call each, with at least the service's
//! minimum interval between two calls to the same instance. A delivered row is
//! deleted. A failed one is retried after one minute, five minutes, thirty
//! minutes, two hours, then twelve hours, and becomes `failed` once it is seven
//! days old. A now-playing report that is more than ten minutes old is deleted
//! instead of retried, because reporting what played ten minutes ago is worse
//! than not reporting it, and a rate-limited answer reschedules the row for the
//! delay the service asked for without counting a failed attempt.
//!
//! # What is retried and what is parked
//!
//! A delivery outcome has two classes. `Unavailable` and a rate limit are
//! transient: the row keeps its place in the queue and is tried again on the
//! backoff schedule. `NotFound`, `Unauthorized`, `InvalidConfig` and `Internal`
//! are terminal - a service that does not know the listen, a revoked token, a
//! configuration it rejects, or a plugin that failed internally will answer the
//! same way next time - so the row is parked at once: its state is `failed`,
//! its `last_error` names the class, and it is never retried. The counters
//! [`ScrobbleOutbox::counters`] returns and the link view an administrator
//! reads show a parked row, so a revoked token is visible immediately instead
//! of after a week of silent retries.
//!
//! A service that rejects a whole batch can still accept most of it, so a
//! terminal failure of a batch of more than one listen is re-submitted one row
//! at a time: the rows the service accepts are delivered and only the ones it
//! rejects are parked. A queued row this host cannot read at all (a payload
//! that is not valid JSON) is parked on its own, so one bad row never blocks
//! the rows behind it.
//!
//! The server runs one delivery pass a minute through the `scrobble.deliver`
//! job and starts another pass whenever an enqueue rings the outbox's
//! [`ScrobbleOutbox::kick`], so the common case is a scrobble that reaches the
//! service within a second and a retried one that waits out its backoff.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hearthsound_core::providers::{
    ExternalId, FeedbackValue, Listen, ScrobbleLimits, ScrobbleProvider, UserContext,
};
use hearthsound_core::{Layer, new_uuid_v7};
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler, Shutdown};
use hearthsound_plugin_host::{ProviderRegistry, UserConfigService};
use sqlx::SqlitePool;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::error::ActivityError;
use crate::{READ_THEN_WRITE, now_millis};

/// The longest a now-playing row is worth sending, in milliseconds.
///
/// A report that has waited longer than this describes playback that has
/// certainly moved on, so it is dropped instead of being delivered late.
pub const NOW_PLAYING_TTL_MS: i64 = 10 * 60 * 1000;

/// How old a row may become before it is given up on, in milliseconds.
///
/// The backoff schedule ends after a day, so the deadline is what eventually
/// moves a row that keeps failing out of the way.
pub const OUTBOX_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The delays between two attempts at a row that keeps failing, in
/// milliseconds.
///
/// The fifth delay repeats for every further attempt, so a service that is
/// down for hours is still retried twice an hour.
const BACKOFF_MS: [i64; 5] = [
    60 * 1000,
    5 * 60 * 1000,
    30 * 60 * 1000,
    2 * 60 * 60 * 1000,
    12 * 60 * 60 * 1000,
];

/// How often the delivery worker runs a pass when nothing rings its kick.
const IDLE_PASS_INTERVAL: Duration = Duration::from_secs(60);

/// How long a service's limits are reused, in milliseconds.
///
/// Ten minutes. The limits are a property of the service's configuration and
/// change at most when an administrator reconfigures the instance, so asking
/// again on every like would spend a plugin call - and up to the instance's
/// whole call deadline - to learn the same answer.
const LIMITS_TTL_MS: i64 = 10 * 60 * 1000;

/// The stored reason of a listen whose queued row this host cannot read.
///
/// The payload is written by this host, so a row that does not parse was
/// damaged; it will not parse on a second attempt either, which is why the row
/// is parked instead of retried.
const MALFORMED_LISTEN: &str = "a queued listen is not valid JSON";

/// The job kind of the scrobble delivery pass.
pub const SCROBBLE_DELIVER_JOB_KIND: &str = "scrobble.deliver";

/// The cron expression of the scrobble delivery pass: once a minute.
///
/// Every due row is retried at most once a minute, and the backoff decides how
/// long a row waits in between, so a per-minute pass is what keeps a recovered
/// service draining promptly without waking the process more often.
pub const SCROBBLE_DELIVER_SCHEDULE: &str = "* * * * *";

/// One scrobble instance deliveries can go through.
///
/// The instance's slug names it in logs; the provider is the trait object its
/// plugin was loaded as.
#[derive(Clone)]
pub struct ScrobbleTarget {
    /// The provider instance's id.
    pub instance_id: Uuid,
    /// The provider instance's slug.
    pub slug: String,
    /// The instance's scrobble provider.
    pub provider: Arc<dyn ScrobbleProvider>,
}

/// The scrobble instances the outbox delivers through.
///
/// The server implements this over [`ProviderRegistry`], so a rebuilt registry
/// is what the next enqueue and the next delivery pass see. A test implements
/// it over a fake provider to decide what every call answers.
pub trait ScrobbleTargets: Send + Sync {
    /// Returns every instance that currently provides scrobbling, in the
    /// scrobble layer's order.
    ///
    /// # Returns
    ///
    /// One entry per enabled instance assigned to the scrobble layer.
    fn scrobbling(&self) -> Vec<ScrobbleTarget>;

    /// Returns the instance with this id, while it still provides scrobbling.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the provider instance to look up.
    ///
    /// # Returns
    ///
    /// The target, or `None` when the instance is gone, disabled, unassigned
    /// from the scrobble layer, or its plugin no longer provides it.
    fn target(&self, instance_id: Uuid) -> Option<ScrobbleTarget>;
}

impl ScrobbleTargets for ProviderRegistry {
    fn scrobbling(&self) -> Vec<ScrobbleTarget> {
        self.layers(Layer::Scrobble)
            .into_iter()
            .filter_map(|instance| {
                let provider = instance.scrobble()?.clone();
                Some(ScrobbleTarget {
                    instance_id: instance.id(),
                    slug: instance.slug().as_str().to_owned(),
                    provider,
                })
            })
            .collect()
    }

    fn target(&self, instance_id: Uuid) -> Option<ScrobbleTarget> {
        let instance = self
            .instances()
            .into_iter()
            .find(|instance| instance.id() == instance_id)?;
        let provider = instance.scrobble()?.clone();
        Some(ScrobbleTarget {
            instance_id,
            slug: instance.slug().as_str().to_owned(),
            provider,
        })
    }
}

/// How many scrobbles are waiting, and the last delivery error.
///
/// This is what the account menu reports for one service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxCounters {
    /// The rows still waiting to be delivered.
    pub pending: u32,
    /// The rows the outbox gave up on.
    pub failed: u32,
    /// The most recent delivery error, when there is one.
    pub last_error: Option<String>,
}

/// The durable queue of scrobbles waiting for their service.
///
/// The outbox owns no connection of its own: it borrows the database's pools,
/// so cloning it is cheap and every clone writes to the same queue.
#[derive(Clone)]
pub struct ScrobbleOutbox {
    /// The writer pool every enqueue and delivery update goes through.
    writer: SqlitePool,
    /// The reader pool the track, artist and album rows are read on.
    readers: SqlitePool,
    /// The instances deliveries go through.
    targets: Arc<dyn ScrobbleTargets>,
    /// The links that decide who may be scrobbled for.
    user_configs: UserConfigService,
    /// The limits of each instance, with the instant they were read.
    ///
    /// Filled by the delivery pass and reused by the feedback enqueue, so a
    /// like never waits on a plugin call for an answer the pass already has.
    limits: Arc<tokio::sync::Mutex<HashMap<Uuid, (ScrobbleLimits, i64)>>>,
    /// Rung after every enqueue, so the delivery worker starts a pass.
    kick: Arc<Notify>,
}

impl std::fmt::Debug for ScrobbleOutbox {
    /// Renders the outbox without the pools or the providers.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrobbleOutbox")
            .finish_non_exhaustive()
    }
}

impl ScrobbleOutbox {
    /// Builds the outbox over a database and the instances it delivers to.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    /// - `targets`: the scrobble instances and their providers.
    /// - `user_configs`: the per-user links the outbox reads.
    ///
    /// # Returns
    ///
    /// The outbox.
    pub fn new(
        db: &Database,
        targets: Arc<dyn ScrobbleTargets>,
        user_configs: UserConfigService,
    ) -> ScrobbleOutbox {
        ScrobbleOutbox {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            targets,
            user_configs,
            limits: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            kick: Arc::new(Notify::new()),
        }
    }

    /// Returns one instance's limits, reading them at most once per
    /// [`LIMITS_TTL_MS`].
    ///
    /// # Parameters
    ///
    /// - `target`: the instance as the registry knows it.
    /// - `now`: the instant to compare the stored read against, in unix
    ///   milliseconds.
    ///
    /// # Returns
    ///
    /// The limits.
    ///
    /// # Errors
    ///
    /// Returns the provider's error, which is not cached: a plugin that could
    /// not answer is asked again on the next call.
    async fn limits_of(
        &self,
        target: &ScrobbleTarget,
        now: i64,
    ) -> Result<ScrobbleLimits, hearthsound_core::ProviderError> {
        {
            let limits = self.limits.lock().await;
            if let Some((limits, at)) = limits.get(&target.instance_id)
                && now - *at < LIMITS_TTL_MS
            {
                return Ok(*limits);
            }
        }
        let limits = target.provider.get_limits().await?;
        self.limits
            .lock()
            .await
            .insert(target.instance_id, (limits, now));
        Ok(limits)
    }

    /// Returns a handle that is rung after every enqueue.
    ///
    /// The server's delivery worker waits on it, so a scrobble reaches its
    /// service without waiting for the next scheduled pass.
    ///
    /// # Returns
    ///
    /// The wake handle.
    pub fn kick(&self) -> Arc<Notify> {
        Arc::clone(&self.kick)
    }

    /// Queues one finished listen for every instance the listener linked.
    ///
    /// The play is marked scrobbled only when at least one row is written.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener who played the track.
    /// - `play_id`: the `plays` row the listen came from.
    /// - `track_id`: the track that was played.
    /// - `client`: the client that played it, when known.
    /// - `started_at`: when playback started, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// How many rows were queued, `0` when the listener linked nothing.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when a row cannot be read or
    /// written, [`ActivityError::ProviderConfig`] when a link cannot be read,
    /// and [`ActivityError::Json`] when the payload cannot be serialized.
    pub async fn listen(
        &self,
        user_id: Uuid,
        play_id: Uuid,
        track_id: Uuid,
        client: Option<&str>,
        started_at: i64,
    ) -> Result<usize, ActivityError> {
        let targets = self.linked_targets(user_id).await?;
        if targets.is_empty() {
            return Ok(0);
        }
        let Some(listen) = self.listen_of(track_id, client, started_at).await? else {
            return Ok(0);
        };
        let payload = serde_json::to_string(&ListenPayload {
            listen: &listen,
            play_id: play_id.to_string(),
        })?;
        let at = now_millis();
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        for target in &targets {
            insert_row(
                &mut tx,
                user_id,
                target.instance_id,
                OutboxKind::Listen,
                &payload,
                at,
            )
            .await?;
        }
        sqlx::query!(
            "UPDATE plays SET scrobbled = 1 WHERE id = ?",
            play_id.as_bytes().as_slice(),
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.kick.notify_one();
        Ok(targets.len())
    }

    /// Queues a now-playing report for every instance the listener linked,
    /// replacing the pending one.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener who started playing.
    /// - `track_id`: the track being played.
    /// - `client`: the client that is playing it, when known.
    /// - `started_at`: when playback started, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// How many rows were queued, `0` when the listener linked nothing.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`ScrobbleOutbox::listen`].
    pub async fn now_playing(
        &self,
        user_id: Uuid,
        track_id: Uuid,
        client: Option<&str>,
        started_at: i64,
    ) -> Result<usize, ActivityError> {
        let targets = self.linked_targets(user_id).await?;
        if targets.is_empty() {
            return Ok(0);
        }
        let Some(listen) = self.listen_of(track_id, client, started_at).await? else {
            return Ok(0);
        };
        let payload = serde_json::to_string(&NowPlayingPayload { listen: &listen })?;
        let at = now_millis();
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        for target in &targets {
            sqlx::query!(
                "DELETE FROM scrobble_outbox
                 WHERE user_id = ? AND instance_id = ? AND kind = 'now-playing'
                   AND state = 'pending'",
                user_id.as_bytes().as_slice(),
                target.instance_id.as_bytes().as_slice(),
            )
            .execute(&mut *tx)
            .await?;
            insert_row(
                &mut tx,
                user_id,
                target.instance_id,
                OutboxKind::NowPlaying,
                &payload,
                at,
            )
            .await?;
        }
        tx.commit().await?;
        self.kick.notify_one();
        Ok(targets.len())
    }

    /// Queues one feedback report for every linked instance that accepts it.
    ///
    /// An instance whose limits cannot be read, or that does not accept
    /// feedback, is skipped. The limits are the cached ones when the delivery
    /// pass read them recently, so the caller - a like on the request path -
    /// usually does not wait on a plugin call at all.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener whose opinion changed.
    /// - `track_id`: the track the opinion is about.
    /// - `value`: the opinion.
    ///
    /// # Returns
    ///
    /// How many rows were queued.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`ScrobbleOutbox::listen`].
    pub async fn feedback(
        &self,
        user_id: Uuid,
        track_id: Uuid,
        value: FeedbackValue,
    ) -> Result<usize, ActivityError> {
        let targets = self.linked_targets(user_id).await?;
        if targets.is_empty() {
            return Ok(0);
        }
        let Some(listen) = self.listen_of(track_id, None, now_millis()).await? else {
            return Ok(0);
        };
        let payload = serde_json::to_string(&FeedbackPayload {
            listen: &listen,
            value: feedback_name(value),
        })?;
        let mut accepting = Vec::new();
        let at = now_millis();
        for target in targets {
            match self.limits_of(&target, at).await {
                Ok(limits) if limits.feedback => accepting.push(target),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        instance = %target.slug,
                        %error,
                        "could not ask a service whether it accepts feedback"
                    );
                }
            }
        }
        if accepting.is_empty() {
            return Ok(0);
        }
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        for target in &accepting {
            insert_row(
                &mut tx,
                user_id,
                target.instance_id,
                OutboxKind::Feedback,
                &payload,
                at,
            )
            .await?;
        }
        tx.commit().await?;
        self.kick.notify_one();
        Ok(accepting.len())
    }

    /// Deletes every scrobble queued for one listener and one instance.
    ///
    /// The unlink route calls this when a listener disconnects a service, so
    /// nothing is delivered for an account that no longer exists.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener.
    /// - `instance_id`: the provider instance.
    ///
    /// # Returns
    ///
    /// How many rows were removed.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the delete fails.
    pub async fn delete_for_instance(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<u64, ActivityError> {
        let outcome = sqlx::query!(
            "DELETE FROM scrobble_outbox WHERE user_id = ? AND instance_id = ?",
            user_id.as_bytes().as_slice(),
            instance_id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?;
        Ok(outcome.rows_affected())
    }

    /// Returns how many scrobbles are waiting for one listener and instance,
    /// and the most recent delivery error.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener.
    /// - `instance_id`: the provider instance.
    ///
    /// # Returns
    ///
    /// The counters.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read.
    pub async fn counters(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<OutboxCounters, ActivityError> {
        let pending: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM scrobble_outbox
             WHERE user_id = ? AND instance_id = ? AND state = 'pending'",
            user_id.as_bytes().as_slice(),
            instance_id.as_bytes().as_slice(),
        )
        .fetch_one(&self.readers)
        .await?;
        let failed: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM scrobble_outbox
             WHERE user_id = ? AND instance_id = ? AND state = 'failed'",
            user_id.as_bytes().as_slice(),
            instance_id.as_bytes().as_slice(),
        )
        .fetch_one(&self.readers)
        .await?;
        let last_error: Option<String> = sqlx::query_scalar!(
            "SELECT last_error FROM scrobble_outbox
             WHERE user_id = ? AND instance_id = ? AND last_error IS NOT NULL
             ORDER BY id DESC LIMIT 1",
            user_id.as_bytes().as_slice(),
            instance_id.as_bytes().as_slice(),
        )
        .fetch_optional(&self.readers)
        .await?
        .flatten();
        Ok(OutboxCounters {
            pending: u32::try_from(pending).unwrap_or(0),
            failed: u32::try_from(failed).unwrap_or(0),
            last_error,
        })
    }

    /// Returns the instances the listener linked, in layer order.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::ProviderConfig`] when a link cannot be read.
    async fn linked_targets(&self, user_id: Uuid) -> Result<Vec<ScrobbleTarget>, ActivityError> {
        let mut linked = Vec::new();
        for target in self.targets.scrobbling() {
            if self
                .user_configs
                .get(target.instance_id, user_id)
                .await?
                .is_some()
            {
                linked.push(target);
            }
        }
        Ok(linked)
    }

    /// Builds the listen a row carries from the track's stored metadata.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track to describe.
    /// - `client`: the client that played it, when known.
    /// - `listened_at`: when playback started, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// The listen, or `None` when the track has no row.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when a query fails.
    async fn listen_of(
        &self,
        track_id: Uuid,
        client: Option<&str>,
        listened_at: i64,
    ) -> Result<Option<Listen>, ActivityError> {
        let Some(track) = sqlx::query!(
            "SELECT title, duration_ms, track_number, mb_recording_id, isrc, album_id
             FROM tracks WHERE id = ?",
            track_id.as_bytes().as_slice(),
        )
        .fetch_optional(&self.readers)
        .await?
        else {
            return Ok(None);
        };
        let credits = sqlx::query!(
            "SELECT a.name AS name, a.mb_artist_id AS mb_artist_id
             FROM track_artists ta JOIN artists a ON a.id = ta.artist_id
             WHERE ta.track_id = ? ORDER BY ta.position",
            track_id.as_bytes().as_slice(),
        )
        .fetch_all(&self.readers)
        .await?;
        let mut ids = Vec::new();
        let mut artists = Vec::with_capacity(credits.len());
        for credit in credits {
            push_id(&mut ids, "mb-artist", credit.mb_artist_id);
            artists.push(credit.name);
        }
        push_id(&mut ids, "mb-recording", track.mb_recording_id.clone());
        push_id(&mut ids, "isrc", track.isrc.clone());

        let mut album = None;
        let mut album_artist = None;
        if let Some(album_id) = track.album_id.as_deref() {
            let album_row = sqlx::query!(
                "SELECT title, mb_release_id FROM albums WHERE id = ?",
                album_id,
            )
            .fetch_optional(&self.readers)
            .await?;
            if let Some(album_row) = album_row {
                album = Some(album_row.title);
                push_id(&mut ids, "mb-release", album_row.mb_release_id);
            }
            album_artist = sqlx::query_scalar!(
                "SELECT a.name FROM album_artists aa JOIN artists a ON a.id = aa.artist_id
                 WHERE aa.album_id = ? ORDER BY aa.position LIMIT 1",
                album_id,
            )
            .fetch_optional(&self.readers)
            .await?;
            let aliases = sqlx::query!(
                "SELECT namespace, value FROM external_ids
                 WHERE entity_kind = 'album' AND entity_id = ?
                   AND namespace IN ('mb-release', 'upc')",
                album_id,
            )
            .fetch_all(&self.readers)
            .await?;
            for alias in aliases {
                push_id(&mut ids, &alias.namespace, Some(alias.value));
            }
        }
        let track_aliases = sqlx::query!(
            "SELECT namespace, value FROM external_ids
             WHERE entity_kind = 'track' AND entity_id = ?
               AND namespace IN ('mb-recording', 'isrc')",
            track_id.as_bytes().as_slice(),
        )
        .fetch_all(&self.readers)
        .await?;
        for alias in track_aliases {
            push_id(&mut ids, &alias.namespace, Some(alias.value));
        }

        Ok(Some(Listen {
            title: track.title,
            artists,
            album,
            album_artist,
            duration_ms: track.duration_ms.and_then(|ms| u32::try_from(ms).ok()),
            track_number: track
                .track_number
                .and_then(|number| u16::try_from(number).ok()),
            ids,
            listened_at: u64::try_from(listened_at).unwrap_or(0),
            client: client.map(str::to_owned),
        }))
    }
}

/// The delivery side of the outbox: turns due rows into provider calls.
///
/// One deliverer is shared by the scheduled job and the worker the enqueue
/// kick wakes. Both run the same pass, and passes never overlap, so one row is
/// not delivered twice by two passes in the same process.
pub struct ScrobbleDeliverer {
    /// The outbox rows are read from and deleted through.
    outbox: Arc<ScrobbleOutbox>,
    /// Held for the whole of one pass.
    pass: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for ScrobbleDeliverer {
    /// Renders the deliverer without the outbox's pools.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScrobbleDeliverer")
            .finish_non_exhaustive()
    }
}

impl ScrobbleDeliverer {
    /// Builds the deliverer over an outbox.
    ///
    /// # Parameters
    ///
    /// - `outbox`: the queue to drain.
    ///
    /// # Returns
    ///
    /// The deliverer.
    pub fn new(outbox: Arc<ScrobbleOutbox>) -> ScrobbleDeliverer {
        ScrobbleDeliverer {
            outbox,
            pass: tokio::sync::Mutex::new(()),
        }
    }

    /// Delivers every pending row that is due at `now`.
    ///
    /// Rows are grouped by instance, and one instance is drained before the
    /// next so the service's pacing applies where it is meant to. Within an
    /// instance, rows are delivered in the order they were queued: a run of
    /// consecutive listens for one listener becomes one call of at most the
    /// service's batch size, and every other row is one call.
    ///
    /// # Parameters
    ///
    /// - `now`: the instant the pass considers current, in unix milliseconds.
    ///   Tests drive the clock through it.
    ///
    /// # Returns
    ///
    /// What the pass did, for logging and tests.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read or
    /// updated.
    pub async fn deliver_due(&self, now: i64) -> Result<DeliveryReport, ActivityError> {
        let _pass = self.pass.lock().await;
        let rows = sqlx::query!(
            "SELECT id, user_id, instance_id, kind, payload, attempts
             FROM scrobble_outbox
             WHERE state = 'pending' AND next_attempt_at <= ?
             ORDER BY instance_id, id",
            now,
        )
        .fetch_all(&self.outbox.readers)
        .await?;
        let mut grouped: HashMap<Uuid, Vec<DueRow>> = HashMap::new();
        let mut order: Vec<Uuid> = Vec::new();
        for row in rows {
            let row = DueRow {
                id: row.id,
                user_id: Uuid::from_slice(&row.user_id)?,
                instance_id: Uuid::from_slice(&row.instance_id)?,
                kind: OutboxKind::parse(&row.kind)?,
                payload: row.payload,
                attempts: row.attempts,
            };
            match grouped.entry(row.instance_id) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    order.push(row.instance_id);
                    slot.insert(vec![row]);
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().push(row);
                }
            }
        }
        let mut report = DeliveryReport::default();
        for instance_id in order {
            let rows = grouped.remove(&instance_id).unwrap_or_default();
            self.deliver_instance(instance_id, rows, now, &mut report)
                .await?;
        }
        Ok(report)
    }

    /// Delivers the due rows of one instance.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the instance the rows were queued for.
    /// - `rows`: its due rows, in queue order.
    /// - `now`: the instant the pass considers current.
    /// - `report`: the running totals.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when a row cannot be updated.
    async fn deliver_instance(
        &self,
        instance_id: Uuid,
        rows: Vec<DueRow>,
        now: i64,
        report: &mut DeliveryReport,
    ) -> Result<(), ActivityError> {
        let Some(target) = self.outbox.targets.target(instance_id) else {
            for row in &rows {
                self.fail(
                    row,
                    "the instance no longer provides scrobbling",
                    now,
                    report,
                )
                .await?;
            }
            return Ok(());
        };
        let limits = match self.outbox.limits_of(&target, now).await {
            Ok(limits) => limits,
            Err(error) => {
                for row in &rows {
                    self.fail(row, &error.to_string(), now, report).await?;
                }
                return Ok(());
            }
        };
        let mut pacer = Pacer::new(limits.min_interval_ms);
        let mut contexts: HashMap<Uuid, Option<UserContext>> = HashMap::new();
        let mut index = 0;
        while index < rows.len() {
            let row = &rows[index];
            if row.kind == OutboxKind::NowPlaying && now - row.created_at() > NOW_PLAYING_TTL_MS {
                self.delete(row).await?;
                report.purged += 1;
                index += 1;
                continue;
            }
            let context = match self.context(&target, row.user_id, &mut contexts).await {
                Ok(Some(context)) => context,
                Ok(None) => {
                    self.purge_user(instance_id, row.user_id).await?;
                    report.purged += 1;
                    index += 1;
                    continue;
                }
                Err(error) => {
                    self.fail(row, &error.to_string(), now, report).await?;
                    index += 1;
                    continue;
                }
            };
            if row.kind != OutboxKind::Listen {
                let outcome = match row.kind {
                    OutboxKind::NowPlaying => {
                        self.now_playing(&target, &context, row, &mut pacer).await
                    }
                    OutboxKind::Feedback => self.feedback(&target, &context, row, &mut pacer).await,
                    OutboxKind::Listen => unreachable!(),
                };
                self.apply(outcome, std::slice::from_ref(&row), now, report)
                    .await?;
                index += 1;
                continue;
            }
            let mut batch = vec![row];
            let mut end = index + 1;
            while end < rows.len()
                && batch.len() < limits.max_batch.max(1) as usize
                && rows[end].kind == OutboxKind::Listen
                && rows[end].user_id == row.user_id
            {
                batch.push(&rows[end]);
                end += 1;
            }
            index = end;
            // A row this host cannot read is parked on its own; the rest of the
            // batch is still submitted, so one bad row never blocks the rows
            // behind it.
            let (valid, malformed) = Self::parse_listens(&batch);
            for row in &malformed {
                self.park(row, MALFORMED_LISTEN, now, report).await?;
            }
            let listens: Vec<Listen> = valid.iter().map(|(_, listen)| listen.clone()).collect();
            if listens.is_empty() {
                continue;
            }
            let outcome = self.listens(&target, &context, &listens, &mut pacer).await;
            if matches!(outcome, Outcome::Permanent(_)) && valid.len() > 1 {
                // A service rejects a batch as a whole even when it accepts most
                // of it, so a terminal answer is retried one row at a time: the
                // rows the service takes are delivered and only the ones it
                // rejects are parked.
                for (row, listen) in &valid {
                    let single = self
                        .listens(&target, &context, std::slice::from_ref(listen), &mut pacer)
                        .await;
                    self.apply(single, std::slice::from_ref(row), now, report)
                        .await?;
                }
                continue;
            }
            let submitted: Vec<&DueRow> = valid.iter().map(|(row, _)| *row).collect();
            self.apply(outcome, &submitted, now, report).await?;
        }
        Ok(())
    }

    /// Returns the user context of a listener, cached for one pass.
    ///
    /// # Parameters
    ///
    /// - `target`: the instance the call is for.
    /// - `user_id`: the listener.
    /// - `contexts`: the contexts already resolved in this pass.
    ///
    /// # Returns
    ///
    /// `None` when the listener no longer has a link to the instance.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::ProviderConfig`] when the link cannot be read.
    async fn context(
        &self,
        target: &ScrobbleTarget,
        user_id: Uuid,
        contexts: &mut HashMap<Uuid, Option<UserContext>>,
    ) -> Result<Option<UserContext>, ActivityError> {
        if let Some(context) = contexts.get(&user_id) {
            return Ok(context.clone());
        }
        let context = self
            .outbox
            .user_configs
            .get(target.instance_id, user_id)
            .await?
            .map(|link| link.context(user_id));
        contexts.insert(user_id, context.clone());
        Ok(context)
    }

    /// Submits a batch of listens.
    ///
    /// # Parameters
    ///
    /// - `target`: the instance.
    /// - `context`: the listener's context.
    /// - `listens`: the parsed listens of the rows to submit.
    /// - `pacer`: the instance's pacing.
    ///
    /// # Returns
    ///
    /// The outcome every listen of the batch shares.
    async fn listens(
        &self,
        target: &ScrobbleTarget,
        context: &UserContext,
        listens: &[Listen],
        pacer: &mut Pacer,
    ) -> Outcome {
        pacer.wait().await;
        match target.provider.submit_listens(context, listens).await {
            Ok(()) => Outcome::Delivered,
            Err(error) => Outcome::from_error(error),
        }
    }

    /// Sends one now-playing report.
    ///
    /// # Parameters
    ///
    /// - `target`: the instance.
    /// - `context`: the listener's context.
    /// - `row`: the queued report.
    /// - `pacer`: the instance's pacing.
    ///
    /// # Returns
    ///
    /// The outcome of the call.
    async fn now_playing(
        &self,
        target: &ScrobbleTarget,
        context: &UserContext,
        row: &DueRow,
        pacer: &mut Pacer,
    ) -> Outcome {
        let Ok(payload) = serde_json::from_str::<NowPlayingPayloadOwned>(&row.payload) else {
            return Outcome::Malformed("a queued now-playing report is not valid JSON");
        };
        pacer.wait().await;
        match target.provider.now_playing(context, &payload.listen).await {
            Ok(()) => Outcome::Delivered,
            Err(error) => Outcome::from_error(error),
        }
    }

    /// Sends one feedback report.
    ///
    /// # Parameters
    ///
    /// - `target`: the instance.
    /// - `context`: the listener's context.
    /// - `row`: the queued report.
    /// - `pacer`: the instance's pacing.
    ///
    /// # Returns
    ///
    /// The outcome of the call.
    async fn feedback(
        &self,
        target: &ScrobbleTarget,
        context: &UserContext,
        row: &DueRow,
        pacer: &mut Pacer,
    ) -> Outcome {
        let Ok(payload) = serde_json::from_str::<FeedbackPayloadOwned>(&row.payload) else {
            return Outcome::Malformed("a queued feedback report is not valid JSON");
        };
        let Some(value) = parse_feedback(&payload.value) else {
            return Outcome::Malformed("a queued feedback report names no known opinion");
        };
        pacer.wait().await;
        match target
            .provider
            .send_feedback(context, &payload.listen, value)
            .await
        {
            Ok(()) => Outcome::Delivered,
            Err(error) => Outcome::from_error(error),
        }
    }

    /// Applies the outcome of a call to every row it covered.
    ///
    /// A delivered row is deleted, a deferred one is rescheduled, a transient
    /// failure walks the backoff, and a permanent one is parked.
    ///
    /// # Parameters
    ///
    /// - `outcome`: what the call did.
    /// - `rows`: the rows the call carried.
    /// - `now`: the instant the pass considers current.
    /// - `report`: the running totals.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when a row cannot be updated.
    async fn apply(
        &self,
        outcome: Outcome,
        rows: &[&DueRow],
        now: i64,
        report: &mut DeliveryReport,
    ) -> Result<(), ActivityError> {
        match outcome {
            Outcome::Delivered => {
                for row in rows {
                    self.delete(row).await?;
                    report.delivered += 1;
                }
            }
            Outcome::RateLimited(retry_after_ms) => {
                for row in rows {
                    self.defer(row, now.saturating_add(retry_after_ms)).await?;
                    report.deferred += 1;
                }
            }
            Outcome::Failed(error) => {
                for row in rows {
                    self.fail(row, &error, now, report).await?;
                }
            }
            Outcome::Permanent(error) => {
                for row in rows {
                    self.park(row, &error, now, report).await?;
                }
            }
            Outcome::Malformed(error) => {
                for row in rows {
                    self.park(row, error, now, report).await?;
                }
            }
        }
        Ok(())
    }

    /// Parks a row a service rejected for good.
    ///
    /// The row is moved to `failed` at once, with the rejection in
    /// `last_error`, and is never picked up again: the counters and the link
    /// view show it to the listener instead of a retry schedule that cannot
    /// succeed.
    ///
    /// # Parameters
    ///
    /// - `row`: the row the service rejected.
    /// - `error`: what the service rejected it for.
    /// - `now`: the instant the pass considers current.
    /// - `report`: the running totals.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be updated.
    async fn park(
        &self,
        row: &DueRow,
        error: &str,
        now: i64,
        report: &mut DeliveryReport,
    ) -> Result<(), ActivityError> {
        sqlx::query!(
            "UPDATE scrobble_outbox
             SET attempts = attempts + 1, last_error = ?, state = 'failed', next_attempt_at = ?
             WHERE id = ?",
            error,
            now,
            row.id.as_slice(),
        )
        .execute(&self.outbox.writer)
        .await?;
        report.gave_up += 1;
        Ok(())
    }

    /// Records a failed attempt and schedules the next one.
    ///
    /// A row whose UUIDv7 id says it is older than [`OUTBOX_MAX_AGE_MS`] is
    /// moved to `failed` instead of being retried again.
    ///
    /// # Parameters
    ///
    /// - `row`: the row that failed.
    /// - `error`: what went wrong.
    /// - `now`: the instant the pass considers current.
    /// - `report`: the running totals.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be updated.
    async fn fail(
        &self,
        row: &DueRow,
        error: &str,
        now: i64,
        report: &mut DeliveryReport,
    ) -> Result<(), ActivityError> {
        let attempts = row.attempts.saturating_add(1);
        report.failed += 1;
        if now - row.created_at() > OUTBOX_MAX_AGE_MS {
            sqlx::query!(
                "UPDATE scrobble_outbox
                 SET attempts = ?, last_error = ?, state = 'failed', next_attempt_at = ?
                 WHERE id = ?",
                attempts,
                error,
                now,
                row.id.as_slice(),
            )
            .execute(&self.outbox.writer)
            .await?;
            report.gave_up += 1;
        } else {
            let delay = backoff_ms(attempts);
            sqlx::query!(
                "UPDATE scrobble_outbox
                 SET attempts = ?, last_error = ?, next_attempt_at = ?
                 WHERE id = ?",
                attempts,
                error,
                now.saturating_add(delay),
                row.id.as_slice(),
            )
            .execute(&self.outbox.writer)
            .await?;
        }
        Ok(())
    }

    /// Reschedules a row without counting a failed attempt.
    ///
    /// # Parameters
    ///
    /// - `row`: the row the service asked to delay.
    /// - `at`: when the row becomes due again, in unix milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be updated.
    async fn defer(&self, row: &DueRow, at: i64) -> Result<(), ActivityError> {
        sqlx::query!(
            "UPDATE scrobble_outbox SET next_attempt_at = ? WHERE id = ?",
            at,
            row.id.as_slice(),
        )
        .execute(&self.outbox.writer)
        .await?;
        Ok(())
    }

    /// Deletes one delivered row.
    ///
    /// # Parameters
    ///
    /// - `row`: the row to delete.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the delete fails.
    async fn delete(&self, row: &DueRow) -> Result<(), ActivityError> {
        sqlx::query!(
            "DELETE FROM scrobble_outbox WHERE id = ?",
            row.id.as_slice(),
        )
        .execute(&self.outbox.writer)
        .await?;
        Ok(())
    }

    /// Deletes every row of a listener who no longer has a link.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the instance.
    /// - `user_id`: the listener.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the delete fails.
    async fn purge_user(&self, instance_id: Uuid, user_id: Uuid) -> Result<(), ActivityError> {
        sqlx::query!(
            "DELETE FROM scrobble_outbox WHERE user_id = ? AND instance_id = ?",
            user_id.as_bytes().as_slice(),
            instance_id.as_bytes().as_slice(),
        )
        .execute(&self.outbox.writer)
        .await?;
        Ok(())
    }

    /// Parses the listens a batch carries.
    ///
    /// A row whose payload is not a valid listen is reported separately instead
    /// of failing the batch, so the rows around it are still delivered.
    ///
    /// # Parameters
    ///
    /// - `batch`: the rows to parse.
    ///
    /// # Returns
    ///
    /// The parseable rows with their listens, in queue order, and the rows this
    /// host cannot read.
    fn parse_listens<'a>(batch: &[&'a DueRow]) -> (Vec<(&'a DueRow, Listen)>, Vec<&'a DueRow>) {
        let mut valid = Vec::with_capacity(batch.len());
        let mut malformed = Vec::new();
        for row in batch {
            match serde_json::from_str::<ListenPayloadOwned>(&row.payload) {
                Ok(payload) => valid.push((*row, payload.listen)),
                Err(_) => malformed.push(*row),
            }
        }
        (valid, malformed)
    }

    /// Runs delivery passes until the server shuts down.
    ///
    /// A pass runs when the outbox's kick is rung and at least every
    /// [`IDLE_PASS_INTERVAL`], so an enqueue reaches its service promptly and a
    /// row whose backoff has passed is retried without another enqueue.
    ///
    /// # Parameters
    ///
    /// - `self`: the shared deliverer.
    /// - `shutdown`: ends the loop.
    pub async fn run(self: Arc<Self>, shutdown: Shutdown) {
        let kick = self.outbox.kick();
        let mut idle = tokio::time::interval(IDLE_PASS_INTERVAL);
        idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.wait() => return,
                _ = idle.tick() => {}
                () = kick.notified() => {}
            }
            if let Err(error) = self.deliver_due(now_millis()).await {
                tracing::warn!(%error, "could not deliver queued scrobbles");
            }
        }
    }
}

/// What one delivery pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeliveryReport {
    /// The rows the services accepted.
    pub delivered: u32,
    /// The rows whose attempt failed.
    pub failed: u32,
    /// The rows that were given up on: the ones older than the outbox's
    /// deadline and the ones a service rejected permanently.
    pub gave_up: u32,
    /// The rows a service asked to delay.
    pub deferred: u32,
    /// The rows deleted without a call: stale now-playing reports and rows of
    /// listeners who unlinked the service.
    pub purged: u32,
}

/// The job kind and schedule of the delivery pass, as the server registers it.
///
/// The handler delivers every due row once; the scheduled pass is what retries
/// a row after its backoff even when nothing new was queued.
#[derive(Debug, Clone)]
pub struct ScrobbleDeliverJobHandler {
    /// The deliverer the pass runs through.
    deliverer: Arc<ScrobbleDeliverer>,
}

impl ScrobbleDeliverJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `deliverer`: the deliverer one pass runs through.
    ///
    /// # Returns
    ///
    /// The handler.
    pub fn new(deliverer: Arc<ScrobbleDeliverer>) -> ScrobbleDeliverJobHandler {
        ScrobbleDeliverJobHandler { deliverer }
    }
}

#[async_trait]
impl JobHandler for ScrobbleDeliverJobHandler {
    /// Runs one delivery pass.
    ///
    /// # Parameters
    ///
    /// - `job`: the claimed job, used for its kind in the log line.
    ///
    /// # Errors
    ///
    /// Returns [`JobError`] when the pass cannot read or update the outbox, so
    /// the queue retries the job on its retry schedule.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        match self.deliverer.deliver_due(now_millis()).await {
            Ok(report) => {
                tracing::debug!(
                    kind = %job.kind,
                    delivered = report.delivered,
                    failed = report.failed,
                    deferred = report.deferred,
                    "delivered queued scrobbles"
                );
                Ok(())
            }
            Err(error) => Err(JobError::new(error.to_string())),
        }
    }
}

/// A row that is due for delivery.
struct DueRow {
    /// The row's UUIDv7 primary key, which also carries its creation instant.
    id: Vec<u8>,
    /// The listener the scrobble belongs to.
    user_id: Uuid,
    /// The instance the row is queued for.
    instance_id: Uuid,
    /// What the row carries.
    kind: OutboxKind,
    /// The JSON body for the service.
    payload: String,
    /// How many attempts already failed.
    attempts: i64,
}

impl DueRow {
    /// Returns the instant the row was created.
    ///
    /// # Returns
    ///
    /// The unix-millisecond timestamp in the UUIDv7 primary key.
    fn created_at(&self) -> i64 {
        created_at_ms(&self.id)
    }
}

/// What a row carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboxKind {
    /// A report of what the listener is playing.
    NowPlaying,
    /// A finished listen.
    Listen,
    /// An opinion of a track.
    Feedback,
}

impl OutboxKind {
    /// Returns the kind's storage name.
    ///
    /// # Returns
    ///
    /// `now-playing`, `listen` or `feedback`.
    fn as_str(self) -> &'static str {
        match self {
            OutboxKind::NowPlaying => "now-playing",
            OutboxKind::Listen => "listen",
            OutboxKind::Feedback => "feedback",
        }
    }

    /// Parses a stored kind.
    ///
    /// # Parameters
    ///
    /// - `kind`: the stored text.
    ///
    /// # Returns
    ///
    /// The kind.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the text is not one the schema
    /// allows.
    fn parse(kind: &str) -> Result<OutboxKind, ActivityError> {
        match kind {
            "now-playing" => Ok(OutboxKind::NowPlaying),
            "listen" => Ok(OutboxKind::Listen),
            "feedback" => Ok(OutboxKind::Feedback),
            other => Err(ActivityError::Database(sqlx::Error::Protocol(format!(
                "unknown scrobble outbox kind `{other}`"
            )))),
        }
    }
}

/// What one provider call decided.
enum Outcome {
    /// The call succeeded and the rows can be deleted.
    Delivered,
    /// The service asked for a delay of this many milliseconds.
    RateLimited(i64),
    /// The call failed and may succeed later, with this message.
    Failed(String),
    /// The call failed in a way that will not change, with this message.
    ///
    /// A parked row is never retried: the service rejected it for a reason a
    /// second attempt cannot fix.
    Permanent(String),
    /// The queued payload or opinion cannot be delivered at all.
    Malformed(&'static str),
}

impl Outcome {
    /// Maps a provider error onto an outcome.
    ///
    /// A rate limit and an unavailable service are transient; every other
    /// variant is terminal, which is the contract
    /// [`hearthsound_core::ProviderError`] states.
    ///
    /// # Parameters
    ///
    /// - `error`: what the provider reported.
    ///
    /// # Returns
    ///
    /// The outcome.
    fn from_error(error: hearthsound_core::ProviderError) -> Outcome {
        use hearthsound_core::ProviderError;
        match error {
            ProviderError::RateLimited { retry_after_ms } => {
                Outcome::RateLimited(i64::try_from(retry_after_ms).unwrap_or(i64::MAX))
            }
            // The stored text is the class the error names, so a parked row's
            // `last_error` says why it is not being retried.
            ProviderError::Unavailable => Outcome::Failed("unavailable".to_owned()),
            ProviderError::NotFound => Outcome::Permanent("not found".to_owned()),
            ProviderError::Unauthorized => Outcome::Permanent("unauthorized".to_owned()),
            ProviderError::InvalidConfig => {
                Outcome::Permanent("invalid configuration".to_owned())
            }
            ProviderError::Internal => Outcome::Permanent("internal provider error".to_owned()),
        }
    }
}

/// Paces calls to one instance.
struct Pacer {
    /// The least time between two calls, in milliseconds.
    interval_ms: u32,
    /// When the previous call started.
    last: Option<Instant>,
}

impl Pacer {
    /// Builds a pacer for one instance.
    ///
    /// # Parameters
    ///
    /// - `interval_ms`: the service's minimum interval between two calls.
    ///
    /// # Returns
    ///
    /// The pacer, with no call made yet.
    fn new(interval_ms: u32) -> Pacer {
        Pacer {
            interval_ms,
            last: None,
        }
    }

    /// Waits until the next call may start, then records it.
    async fn wait(&mut self) {
        if let Some(last) = self.last {
            let interval = Duration::from_millis(u64::from(self.interval_ms));
            if let Some(remaining) = (last + interval).checked_duration_since(Instant::now()) {
                tokio::time::sleep(remaining).await;
            }
        }
        self.last = Some(Instant::now());
    }
}

/// The stored body of a finished listen.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ListenPayload<'a> {
    /// The listen itself.
    listen: &'a Listen,
    /// The play the listen came from.
    play_id: String,
}

/// A stored body of a finished listen, for delivery.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListenPayloadOwned {
    /// The listen itself.
    listen: Listen,
}

/// The stored body of a now-playing report.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct NowPlayingPayload<'a> {
    /// The listen that describes the track.
    listen: &'a Listen,
}

/// A stored body of a now-playing report, for delivery.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NowPlayingPayloadOwned {
    /// The listen that describes the track.
    listen: Listen,
}

/// The stored body of a feedback report.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackPayload<'a> {
    /// The listen that describes the track.
    listen: &'a Listen,
    /// The opinion: `love`, `hate` or `clear`.
    value: &'a str,
}

/// A stored body of a feedback report, for delivery.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackPayloadOwned {
    /// The listen that describes the track.
    listen: Listen,
    /// The opinion: `love`, `hate` or `clear`.
    value: String,
}

/// Returns the stored name of an opinion.
///
/// # Parameters
///
/// - `value`: the opinion.
///
/// # Returns
///
/// `love`, `hate` or `clear`.
fn feedback_name(value: FeedbackValue) -> &'static str {
    match value {
        FeedbackValue::Love => "love",
        FeedbackValue::Hate => "hate",
        FeedbackValue::Clear => "clear",
    }
}

/// Parses a stored opinion.
///
/// # Parameters
///
/// - `value`: the stored text.
///
/// # Returns
///
/// The opinion, or `None` when the text names none.
fn parse_feedback(value: &str) -> Option<FeedbackValue> {
    match value {
        "love" => Some(FeedbackValue::Love),
        "hate" => Some(FeedbackValue::Hate),
        "clear" => Some(FeedbackValue::Clear),
        _ => None,
    }
}

/// Inserts one pending row.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `user_id`: the listener.
/// - `instance_id`: the instance.
/// - `kind`: what the row carries.
/// - `payload`: the JSON body for the service.
/// - `at`: when the row may first be delivered, in unix milliseconds.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the insert fails.
async fn insert_row(
    conn: &mut sqlx::SqliteConnection,
    user_id: Uuid,
    instance_id: Uuid,
    kind: OutboxKind,
    payload: &str,
    at: i64,
) -> Result<(), ActivityError> {
    let id = new_uuid_v7();
    sqlx::query!(
        "INSERT INTO scrobble_outbox (
             id, user_id, instance_id, kind, payload, attempts, next_attempt_at, state
         ) VALUES (?, ?, ?, ?, ?, 0, ?, 'pending')",
        id.as_bytes().as_slice(),
        user_id.as_bytes().as_slice(),
        instance_id.as_bytes().as_slice(),
        kind.as_str(),
        payload,
        at,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Adds a canonical identifier when it has a value and is not known yet.
///
/// # Parameters
///
/// - `ids`: the identifiers collected so far.
/// - `namespace`: the identifier namespace.
/// - `value`: the identifier's value, when known.
fn push_id(ids: &mut Vec<ExternalId>, namespace: &str, value: Option<String>) {
    let Some(value) = value else {
        return;
    };
    if value.is_empty()
        || ids
            .iter()
            .any(|id| id.namespace == namespace && id.value == value)
    {
        return;
    }
    ids.push(ExternalId {
        namespace: namespace.to_owned(),
        value,
    });
}

/// Returns the instant a UUIDv7 row was created, in unix milliseconds.
///
/// # Parameters
///
/// - `id`: the stored bytes.
///
/// # Returns
///
/// The millisecond timestamp in the first 48 bits, or `0` when the stored key is
/// too short to hold one.
fn created_at_ms(id: &[u8]) -> i64 {
    let Some(head) = id.get(..6) else {
        return 0;
    };
    let mut bytes = [0u8; 8];
    bytes[2..].copy_from_slice(head);
    i64::from_be_bytes(bytes)
}

/// Returns the delay before the next attempt.
///
/// # Parameters
///
/// - `attempts`: how many attempts have failed, including the latest one.
///
/// # Returns
///
/// The delay in milliseconds: one minute, five minutes, thirty minutes, two
/// hours, then twelve hours for every attempt after the fifth.
fn backoff_ms(attempts: i64) -> i64 {
    let index = attempts.clamp(1, 5) as usize - 1;
    BACKOFF_MS[index]
}
