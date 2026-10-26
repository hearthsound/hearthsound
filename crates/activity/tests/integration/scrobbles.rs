//! The scrobble outbox: what is queued, and how it is delivered.
//!
//! Every test drives a real outbox over a temporary migrated database with a
//! fake provider, so the rows, the attempts, the backoff and the batches are the
//! ones the server writes, and the clock is the `now` argument the deliverer
//! takes. Only the pacing test waits on the real clock, because pacing is the
//! one rule the pass cannot express in database time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hearthsound_activity::{
    ActivityService, Feedback, NOW_PLAYING_TTL_MS, OUTBOX_MAX_AGE_MS, Scrobble, ScrobbleDeliverer,
    ScrobbleOutbox, ScrobbleOutcome, ScrobbleTarget, ScrobbleTargets,
};
use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_core::providers::{
    FeedbackValue, Listen, ProviderLifecycle, ScrobbleLimits, ScrobbleProvider, UserContext,
};
use hearthsound_core::{EntityKind, ProviderError};
use hearthsound_db::Database;
use hearthsound_library::LibraryRepository;
use hearthsound_plugin_host::UserConfigService;
use tempfile::TempDir;
use uuid::Uuid;

use crate::common::{insert_album, insert_track, insert_user, open_database};

/// The backoff ladder every failed attempt walks, in milliseconds.
const BACKOFF: [i64; 5] = [60_000, 300_000, 1_800_000, 7_200_000, 43_200_000];

/// One queued row as the tests read it.
#[derive(Debug)]
struct StoredRow {
    /// The listener.
    user_id: Uuid,
    /// The instance.
    instance_id: Uuid,
    /// The kind.
    kind: String,
    /// The JSON body.
    payload: String,
    /// How many attempts failed.
    attempts: i64,
    /// When the row is due again.
    next_attempt_at: i64,
    /// Whether the row is still pending.
    state: String,
    /// The most recent error.
    last_error: Option<String>,
}

/// What one fake provider recorded.
#[derive(Debug, Default)]
struct Recorded {
    /// The batches of listens the provider received.
    listens: Vec<Vec<Listen>>,
    /// The now-playing reports the provider received.
    now_playing: Vec<Listen>,
    /// The feedback the provider received.
    feedback: Vec<(Listen, FeedbackValue)>,
    /// The titles of every listen submission the provider was asked for, in
    /// order, so a test can tell a batch apart from single-row submissions.
    submissions: Vec<Vec<String>>,
    /// When each call started.
    calls: Vec<Instant>,
}

/// A provider that records what it was asked and answers what the test decides.
#[derive(Clone)]
struct FakeProvider {
    /// The limits it reports.
    limits: ScrobbleLimits,
    /// What its calls answer, when the test set a failure.
    error: Arc<Mutex<Option<ProviderError>>>,
    /// A title the provider rejects permanently, and any batch of more than one
    /// listen with it, the way a service that refuses one item rejects the
    /// call that carries it.
    rejects: Arc<Mutex<Option<String>>>,
    /// How long reading the limits takes, so a test can tell whether a caller
    /// waited on it.
    limits_delay: Arc<Mutex<Option<Duration>>>,
    /// How many times the limits were asked for.
    limits_calls: Arc<AtomicUsize>,
    /// What it was asked.
    recorded: Arc<Mutex<Recorded>>,
}

impl FakeProvider {
    /// Builds a provider with the given limits that answers successfully.
    fn new(limits: ScrobbleLimits) -> FakeProvider {
        FakeProvider {
            limits,
            error: Arc::new(Mutex::new(None)),
            rejects: Arc::new(Mutex::new(None)),
            limits_delay: Arc::new(Mutex::new(None)),
            limits_calls: Arc::new(AtomicUsize::new(0)),
            recorded: Arc::new(Mutex::new(Recorded::default())),
        }
    }

    /// Makes reading the limits take `delay`.
    ///
    /// # Parameters
    ///
    /// - `delay`: how long the call stalls.
    fn stall_limits(&self, delay: Duration) {
        *self.limits_delay.lock().unwrap() = Some(delay);
    }

    /// Returns how many times the limits were asked for.
    fn limits_calls(&self) -> usize {
        self.limits_calls.load(Ordering::SeqCst)
    }

    /// Makes the provider reject any submission of more than one listen, and
    /// any submission that names `title`, with `NotFound`.
    ///
    /// # Parameters
    ///
    /// - `title`: the track title the provider refuses.
    fn reject_batches_and(&self, title: &str) {
        *self.rejects.lock().unwrap() = Some(title.to_owned());
    }

    /// Makes every later call fail with `error`.
    fn fail(&self, error: ProviderError) {
        *self.error.lock().unwrap() = Some(error);
    }

    /// Makes every later call succeed.
    fn recover(&self) {
        *self.error.lock().unwrap() = None;
    }

    /// Records the start of a call and returns the configured failure.
    fn begin(&self) -> Option<ProviderError> {
        let error = self.error.lock().unwrap().clone();
        self.recorded.lock().unwrap().calls.push(Instant::now());
        error
    }

    /// Returns what the provider recorded.
    fn recorded(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.recorded.lock().unwrap()
    }
}

#[async_trait]
impl ProviderLifecycle for FakeProvider {
    async fn validate_config(&self, _server_config: &str) -> Result<Vec<String>, ProviderError> {
        Ok(Vec::new())
    }

    async fn health(&self) -> Result<String, ProviderError> {
        Ok("ok".to_owned())
    }
}

#[async_trait]
impl ScrobbleProvider for FakeProvider {
    async fn get_limits(&self) -> Result<ScrobbleLimits, ProviderError> {
        self.limits_calls.fetch_add(1, Ordering::SeqCst);
        let delay = *self.limits_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        Ok(self.limits)
    }

    async fn now_playing(&self, _user: &UserContext, track: &Listen) -> Result<(), ProviderError> {
        if let Some(error) = self.begin() {
            return Err(error);
        }
        self.recorded
            .lock()
            .unwrap()
            .now_playing
            .push(track.clone());
        Ok(())
    }

    async fn submit_listens(
        &self,
        _user: &UserContext,
        listens: &[Listen],
    ) -> Result<(), ProviderError> {
        let titles: Vec<String> = listens.iter().map(|listen| listen.title.clone()).collect();
        let rejection = self.rejects.lock().unwrap().clone();
        self.recorded.lock().unwrap().submissions.push(titles.clone());
        if let Some(error) = self.begin() {
            return Err(error);
        }
        if let Some(rejected) = rejection
            && (listens.len() > 1 || titles.iter().any(|title| *title == rejected))
        {
            return Err(ProviderError::NotFound);
        }
        self.recorded.lock().unwrap().listens.push(listens.to_vec());
        Ok(())
    }

    async fn send_feedback(
        &self,
        _user: &UserContext,
        track: &Listen,
        value: FeedbackValue,
    ) -> Result<(), ProviderError> {
        if let Some(error) = self.begin() {
            return Err(error);
        }
        self.recorded
            .lock()
            .unwrap()
            .feedback
            .push((track.clone(), value));
        Ok(())
    }
}

/// The instances a test delivers through.
struct FakeTargets {
    /// One target per instance, in layer order.
    targets: Vec<ScrobbleTarget>,
}

impl ScrobbleTargets for FakeTargets {
    fn scrobbling(&self) -> Vec<ScrobbleTarget> {
        self.targets.clone()
    }

    fn target(&self, instance_id: Uuid) -> Option<ScrobbleTarget> {
        self.targets
            .iter()
            .find(|target| target.instance_id == instance_id)
            .cloned()
    }
}

/// One instance a test set up.
struct Instance {
    /// The instance id.
    id: Uuid,
    /// The provider behind it.
    provider: FakeProvider,
}

/// A database, a listener, some instances and the outbox over them.
struct Harness {
    /// Keeps the database's directory alive.
    _dir: TempDir,
    /// The open database.
    database: Database,
    /// The listener.
    user: Uuid,
    /// A track of the listener's library.
    track: Uuid,
    /// The instances, in layer order.
    instances: Vec<Instance>,
    /// The links the outbox reads.
    user_configs: UserConfigService,
    /// The outbox rows are queued in.
    outbox: ScrobbleOutbox,
    /// The deliverer the tests drive with their own clock.
    deliverer: ScrobbleDeliverer,
}

impl Harness {
    /// Builds a harness with one instance per limit the test passes.
    async fn new(limits: &[ScrobbleLimits]) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let database = open_database(dir.path()).await;
        let user = insert_user(&database).await;
        let library = LibraryRepository::new(&database);
        let (_, tracks) = insert_album(&library).await;
        let track = tracks[0];
        let user_configs = UserConfigService::new(&database, SecretBox::new(SecretKey::generate()));
        let mut instances = Vec::new();
        let mut targets = Vec::new();
        for (index, limits) in limits.iter().enumerate() {
            let slug = format!("service-{index}");
            let id = insert_instance(&database, &slug).await;
            let provider = FakeProvider::new(*limits);
            targets.push(ScrobbleTarget {
                instance_id: id,
                slug,
                provider: Arc::new(provider.clone()),
            });
            instances.push(Instance { id, provider });
        }
        let targets = Arc::new(FakeTargets { targets });
        let outbox = ScrobbleOutbox::new(&database, targets, user_configs.clone());
        let deliverer = ScrobbleDeliverer::new(Arc::new(outbox.clone()));
        Harness {
            _dir: dir,
            database,
            user,
            track,
            instances,
            user_configs,
            outbox,
            deliverer,
        }
    }

    /// Returns the last instance the harness built.
    fn instance(&self) -> &Instance {
        self.instances.last().unwrap()
    }

    /// Links the listener to every instance.
    async fn link_all(&self) {
        for instance in &self.instances {
            self.user_configs
                .put(
                    instance.id,
                    self.user,
                    Some("listener".to_owned()),
                    &serde_json::json!({"token": "good"}),
                )
                .await
                .unwrap();
        }
    }

    /// Links the listener to one instance.
    async fn link(&self, instance: &Instance) {
        self.user_configs
            .put(
                instance.id,
                self.user,
                Some("listener".to_owned()),
                &serde_json::json!({"token": "good"}),
            )
            .await
            .unwrap();
    }

    /// Returns every queued row, oldest first.
    async fn rows(&self) -> Vec<StoredRow> {
        let rows = sqlx::query(
            "SELECT id, user_id, instance_id, kind, payload, attempts, next_attempt_at, state,
                    last_error
             FROM scrobble_outbox ORDER BY id",
        )
        .fetch_all(self.database.readers())
        .await
        .unwrap();
        rows.iter()
            .map(|row| StoredRow {
                user_id: Uuid::from_slice(sqlx::Row::get::<Vec<u8>, _>(row, "user_id").as_slice())
                    .unwrap(),
                instance_id: Uuid::from_slice(
                    sqlx::Row::get::<Vec<u8>, _>(row, "instance_id").as_slice(),
                )
                .unwrap(),
                kind: sqlx::Row::get(row, "kind"),
                payload: sqlx::Row::get(row, "payload"),
                attempts: sqlx::Row::get(row, "attempts"),
                next_attempt_at: sqlx::Row::get(row, "next_attempt_at"),
                state: sqlx::Row::get(row, "state"),
                last_error: sqlx::Row::get(row, "last_error"),
            })
            .collect()
    }

    /// Returns the outbox's row count.
    async fn row_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM scrobble_outbox")
            .fetch_one(self.database.readers())
            .await
            .unwrap()
    }
}

/// Waits until the outbox holds at least `count` rows and returns them.
///
/// The feedback enqueue runs in a task of its own, so a test that liked or
/// starred something waits for the row instead of reading it immediately.
///
/// # Parameters
///
/// - `harness`: the harness whose outbox is read.
/// - `count`: the number of rows to wait for.
///
/// # Panics
///
/// When the rows do not appear within ten seconds.
async fn wait_for_rows(harness: &Harness, count: usize) -> Vec<StoredRow> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let rows = harness.rows().await;
        if rows.len() >= count {
            return rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the enqueue did not write {count} rows in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Inserts one enabled provider instance with its plugin row.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `slug`: the instance slug.
///
/// # Returns
///
/// The instance id.
async fn insert_instance(database: &Database, slug: &str) -> Uuid {
    let plugin_id = format!("com.example.{slug}");
    sqlx::query(
        "INSERT INTO plugins (
             id, version, api, manifest, sha256, source, enabled, installed_at
         ) VALUES (?, '0.1.0', 'hearthsound:plugin@0.3.0', '{}', ?, 'sideload', 1, 0)",
    )
    .bind(&plugin_id)
    .bind(vec![0u8; 32])
    .execute(database.writer())
    .await
    .unwrap();
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO provider_instances (
             id, plugin_id, slug, display_name, enabled, created_at, updated_at
         ) VALUES (?, ?, ?, ?, 1, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(&plugin_id)
    .bind(slug)
    .bind(slug)
    .execute(database.writer())
    .await
    .unwrap();
    id
}

/// Returns the current time in unix milliseconds.
///
/// The deliverer takes the pass instant as an argument, so the tests use the
/// real clock as their starting point and advance it by the delays they expect.
fn current_time() -> i64 {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()) as i64
}

/// Returns limits that batch at `max_batch` and do not wait between calls.
///
/// # Parameters
///
/// - `max_batch`: the largest batch the fake service accepts.
/// - `feedback`: whether it accepts feedback.
fn limits(max_batch: u32, feedback: bool) -> ScrobbleLimits {
    ScrobbleLimits {
        max_batch,
        min_interval_ms: 0,
        feedback,
    }
}

/// Builds a submission for a track.
///
/// # Parameters
///
/// - `user`: the listener.
/// - `track`: the track.
fn submission(user: Uuid, track: Uuid) -> Scrobble<'static> {
    Scrobble {
        user_id: user,
        track_id: track,
        client: Some("dsub"),
        submission: true,
        started_at: 1_700_000_000_000,
        listened_ms: 180_000,
    }
}

/// Parses a stored payload as the listen body the plugin receives.
///
/// # Parameters
///
/// - `payload`: the stored JSON.
fn payload_listen(payload: &str) -> Listen {
    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
    serde_json::from_value(value["listen"].clone()).unwrap()
}

#[tokio::test]
async fn a_listen_is_queued_for_each_linked_instance_and_marked_scrobbled() {
    let harness = Harness::new(&[limits(10, false), limits(10, false)]).await;
    harness.link(&harness.instances[0]).await;
    let activity = ActivityService::new(&harness.database)
        .with_scrobble_outbox(Arc::new(harness.outbox.clone()));

    let outcome = activity
        .scrobble(&submission(harness.user, harness.track))
        .await
        .unwrap();
    assert!(matches!(outcome, ScrobbleOutcome::Recorded(_)));

    let rows = harness.rows().await;
    assert_eq!(rows.len(), 1, "only the linked instance gets a row");
    assert_eq!(rows[0].kind, "listen");
    assert_eq!(rows[0].user_id, harness.user);
    assert_eq!(rows[0].instance_id, harness.instances[0].id);
    let listen = payload_listen(&rows[0].payload);
    assert_eq!(listen.title, "First");
    assert_eq!(listen.artists, vec!["Remote Artist".to_owned()]);
    assert_eq!(listen.album.as_deref(), Some("Remote Album"));
    assert_eq!(listen.album_artist.as_deref(), Some("Remote Artist"));
    assert_eq!(listen.track_number, Some(1));
    let payload: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
    assert!(payload["playId"].as_str().is_some(), "{payload}");

    let scrobbled: i64 = sqlx::query_scalar("SELECT scrobbled FROM plays")
        .fetch_one(harness.database.readers())
        .await
        .unwrap();
    assert_eq!(scrobbled, 1);

    let report = harness.deliverer.deliver_due(current_time()).await.unwrap();
    assert_eq!(report.delivered, 1);
    let batches = harness.instances[0].provider.recorded().listens.clone();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0][0].title, "First");
    assert_eq!(harness.row_count().await, 0);
}

#[tokio::test]
async fn a_listener_without_a_link_writes_nothing() {
    let harness = Harness::new(&[limits(10, false)]).await;
    let activity = ActivityService::new(&harness.database)
        .with_scrobble_outbox(Arc::new(harness.outbox.clone()));

    let outcome = activity
        .scrobble(&submission(harness.user, harness.track))
        .await
        .unwrap();
    assert!(matches!(outcome, ScrobbleOutcome::Recorded(_)));
    assert_eq!(harness.row_count().await, 0);
    let scrobbled: i64 = sqlx::query_scalar("SELECT scrobbled FROM plays")
        .fetch_one(harness.database.readers())
        .await
        .unwrap();
    assert_eq!(scrobbled, 0, "an unscrobbled play stays unmarked");
}

#[tokio::test]
async fn a_now_playing_report_replaces_the_pending_one() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    let activity = ActivityService::new(&harness.database)
        .with_scrobble_outbox(Arc::new(harness.outbox.clone()));

    let mut report = submission(harness.user, harness.track);
    report.submission = false;
    activity.scrobble(&report).await.unwrap();
    activity.scrobble(&report).await.unwrap();

    let rows = harness.rows().await;
    assert_eq!(rows.len(), 1, "the second report replaces the first");
    assert_eq!(rows[0].kind, "now-playing");
    assert_eq!(payload_listen(&rows[0].payload).title, "First");

    harness.deliverer.deliver_due(current_time()).await.unwrap();
    let reported = harness.instances[0].provider.recorded().now_playing.clone();
    assert_eq!(reported.len(), 1);
    assert_eq!(harness.row_count().await, 0);
}

#[tokio::test]
async fn feedback_is_queued_only_for_services_that_accept_it() {
    let harness = Harness::new(&[limits(10, true), limits(10, false)]).await;
    harness.link_all().await;
    let activity = ActivityService::new(&harness.database)
        .with_scrobble_outbox(Arc::new(harness.outbox.clone()));

    activity
        .set_feedback(
            harness.user,
            EntityKind::Track,
            harness.track,
            Feedback::Like,
        )
        .await
        .unwrap();
    let rows = wait_for_rows(&harness, 1).await;
    assert_eq!(rows.len(), 1, "only the accepting service gets feedback");
    assert_eq!(rows[0].instance_id, harness.instances[0].id);
    assert_eq!(rows[0].kind, "feedback");
    let payload: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
    assert_eq!(payload["value"], "love");

    harness.deliverer.deliver_due(current_time()).await.unwrap();
    let sent = harness.instances[0].provider.recorded().feedback.clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, FeedbackValue::Love);
}

#[tokio::test]
async fn a_starred_track_reports_love_and_an_unstarred_one_clear() {
    let harness = Harness::new(&[limits(10, true)]).await;
    harness.link_all().await;
    let activity = ActivityService::new(&harness.database)
        .with_scrobble_outbox(Arc::new(harness.outbox.clone()));

    activity
        .star(harness.user, &[(EntityKind::Track, harness.track)])
        .await
        .unwrap();
    let rows = wait_for_rows(&harness, 1).await;
    assert_eq!(rows.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
    assert_eq!(payload["value"], "love");

    activity
        .unstar(harness.user, &[(EntityKind::Track, harness.track)])
        .await
        .unwrap();
    let values: Vec<String> = wait_for_rows(&harness, 2)
        .await
        .iter()
        .map(|row| {
            let payload: serde_json::Value = serde_json::from_str(&row.payload).unwrap();
            payload["value"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(values, vec!["love", "clear"]);
}

#[tokio::test]
async fn a_failed_delivery_walks_the_backoff_ladder() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    let provider = harness.instance().provider.clone();
    provider.fail(ProviderError::Unavailable);
    let activity = ActivityService::new(&harness.database)
        .with_scrobble_outbox(Arc::new(harness.outbox.clone()));
    activity
        .scrobble(&submission(harness.user, harness.track))
        .await
        .unwrap();

    let start = current_time();
    let mut now = start;
    for (attempt, delay) in BACKOFF.iter().enumerate() {
        let report = harness.deliverer.deliver_due(now).await.unwrap();
        assert_eq!(report.failed, 1, "attempt {}", attempt + 1);
        let rows = harness.rows().await;
        assert_eq!(rows.len(), 1, "a failed row is kept");
        assert_eq!(rows[0].attempts, attempt as i64 + 1);
        assert_eq!(rows[0].next_attempt_at, now + delay);
        assert_eq!(rows[0].state, "pending");
        assert!(rows[0].last_error.is_some());

        // The row is not due one millisecond early.
        let early = harness
            .deliverer
            .deliver_due(now + delay - 1)
            .await
            .unwrap();
        assert_eq!(early.failed, 0);
        assert_eq!(early.delivered, 0);
        now += delay;
    }

    // The fifth delay repeats for every further attempt.
    harness.deliverer.deliver_due(now).await.unwrap();
    let rows = harness.rows().await;
    assert_eq!(rows[0].attempts, 6);
    assert_eq!(rows[0].next_attempt_at, now + BACKOFF[4]);
}

#[tokio::test]
async fn recovery_delivers_everything_that_waited() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    let provider = harness.instance().provider.clone();
    provider.fail(ProviderError::Unavailable);
    harness
        .outbox
        .listen(
            harness.user,
            Uuid::now_v7(),
            harness.track,
            Some("dsub"),
            1_700_000_000_000,
        )
        .await
        .unwrap();
    harness
        .outbox
        .listen(
            harness.user,
            Uuid::now_v7(),
            harness.track,
            Some("dsub"),
            1_700_000_100_000,
        )
        .await
        .unwrap();

    let start = current_time();
    harness.deliverer.deliver_due(start).await.unwrap();
    assert_eq!(harness.row_count().await, 2);

    provider.recover();
    let report = harness
        .deliverer
        .deliver_due(start + BACKOFF[0])
        .await
        .unwrap();
    assert_eq!(report.delivered, 2);
    assert_eq!(harness.row_count().await, 0);
    assert_eq!(harness.instances[0].provider.recorded().listens.len(), 1);
}

#[tokio::test]
async fn listens_are_batched_up_to_the_services_maximum() {
    let harness = Harness::new(&[limits(50, false)]).await;
    harness.link_all().await;
    for _ in 0..120 {
        harness
            .outbox
            .listen(
                harness.user,
                Uuid::now_v7(),
                harness.track,
                Some("dsub"),
                1_700_000_000_000,
            )
            .await
            .unwrap();
    }
    assert_eq!(harness.row_count().await, 120);

    harness.deliverer.deliver_due(current_time()).await.unwrap();
    let sizes: Vec<usize> = harness.instances[0]
        .provider
        .recorded()
        .listens
        .iter()
        .map(Vec::len)
        .collect();
    assert_eq!(sizes, vec![50, 50, 20]);
    assert_eq!(harness.row_count().await, 0);
}

#[tokio::test]
async fn calls_to_one_instance_are_paced_by_its_minimum_interval() {
    let harness = Harness::new(&[ScrobbleLimits {
        max_batch: 1,
        min_interval_ms: 1000,
        feedback: false,
    }])
    .await;
    harness.link_all().await;
    for _ in 0..3 {
        harness
            .outbox
            .listen(
                harness.user,
                Uuid::now_v7(),
                harness.track,
                Some("dsub"),
                1_700_000_000_000,
            )
            .await
            .unwrap();
    }
    harness.deliverer.deliver_due(current_time()).await.unwrap();
    let calls = harness.instances[0].provider.recorded().calls.clone();
    assert_eq!(calls.len(), 3);
    for pair in calls.windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        assert!(
            gap >= Duration::from_secs(1),
            "calls were {gap:?} apart, less than the service's minimum"
        );
    }
}

#[tokio::test]
async fn a_rate_limited_answer_reschedules_without_counting_an_attempt() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    harness
        .instance()
        .provider
        .fail(ProviderError::RateLimited {
            retry_after_ms: 2500,
        });
    harness
        .outbox
        .listen(
            harness.user,
            Uuid::now_v7(),
            harness.track,
            Some("dsub"),
            1_700_000_000_000,
        )
        .await
        .unwrap();

    let start = current_time();
    let report = harness.deliverer.deliver_due(start).await.unwrap();
    assert_eq!(report.deferred, 1);
    let rows = harness.rows().await;
    assert_eq!(rows[0].attempts, 0);
    assert_eq!(rows[0].next_attempt_at, start + 2500);
    assert_eq!(rows[0].state, "pending");
    assert!(rows[0].last_error.is_none());
}

#[tokio::test]
async fn a_row_older_than_a_week_is_given_up_on() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    let start = current_time();
    let created = start - OUTBOX_MAX_AGE_MS - 1;
    insert_raw_row(&harness, created, start).await;
    harness.instance().provider.fail(ProviderError::Unavailable);

    let report = harness.deliverer.deliver_due(start).await.unwrap();
    assert_eq!(report.gave_up, 1);
    let rows = harness.rows().await;
    assert_eq!(rows[0].state, "failed");
    assert_eq!(rows[0].attempts, 1);
    assert!(rows[0].last_error.is_some());

    // A failed row is not picked up again.
    harness.instance().provider.recover();
    harness
        .deliverer
        .deliver_due(start + OUTBOX_MAX_AGE_MS)
        .await
        .unwrap();
    assert_eq!(harness.row_count().await, 1);
}

#[tokio::test]
async fn a_stale_now_playing_report_is_dropped_instead_of_retried() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    let now = current_time();
    insert_raw_row(&harness, now - NOW_PLAYING_TTL_MS - 1, now).await;
    // The row is a pending now-playing report; make its kind match.
    sqlx::query("UPDATE scrobble_outbox SET kind = 'now-playing'")
        .execute(harness.database.writer())
        .await
        .unwrap();

    let report = harness.deliverer.deliver_due(now).await.unwrap();
    assert_eq!(report.purged, 1);
    assert_eq!(harness.row_count().await, 0);
    assert!(
        harness.instances[0]
            .provider
            .recorded()
            .now_playing
            .is_empty()
    );
}

#[tokio::test]
async fn counters_report_what_waits_and_what_was_given_up_on() {
    let harness = Harness::new(&[limits(10, false), limits(10, false)]).await;
    harness.link_all().await;
    harness
        .outbox
        .listen(
            harness.user,
            Uuid::now_v7(),
            harness.track,
            Some("dsub"),
            1_700_000_000_000,
        )
        .await
        .unwrap();
    let instance = harness.instances[0].id;
    let counters = harness
        .outbox
        .counters(harness.user, instance)
        .await
        .unwrap();
    assert_eq!(counters.pending, 1);
    assert_eq!(counters.failed, 0);
    assert!(counters.last_error.is_none());

    for instance in &harness.instances {
        instance.provider.fail(ProviderError::Unavailable);
    }
    harness.deliverer.deliver_due(current_time()).await.unwrap();
    // Both instances failed; the counters are per instance.
    let counters = harness
        .outbox
        .counters(harness.user, instance)
        .await
        .unwrap();
    assert_eq!(counters.pending, 1);
    assert_eq!(counters.failed, 0);
    assert!(counters.last_error.is_some());
}

#[tokio::test]
async fn unlinking_deletes_the_listeners_rows_for_the_instance() {
    let harness = Harness::new(&[limits(10, false), limits(10, false)]).await;
    harness.link_all().await;
    harness
        .outbox
        .listen(
            harness.user,
            Uuid::now_v7(),
            harness.track,
            Some("dsub"),
            1_700_000_000_000,
        )
        .await
        .unwrap();
    assert_eq!(harness.row_count().await, 2);

    let removed = harness
        .outbox
        .delete_for_instance(harness.user, harness.instances[0].id)
        .await
        .unwrap();
    assert_eq!(removed, 1);
    let rows = harness.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].instance_id, harness.instances[1].id);
}

/// Inserts a pending listen row with a chosen creation instant.
///
/// # Parameters
///
/// - `harness`: the harness whose first instance the row targets.
/// - `created`: the instant encoded in the row's UUIDv7 id.
/// - `due`: when the row may be delivered.
async fn insert_raw_row(harness: &Harness, created: i64, due: i64) {
    let mut id = [0u8; 16];
    id[..6].copy_from_slice(&created.to_be_bytes()[2..]);
    let payload = serde_json::json!({
        "listen": {
            "title": "First",
            "artists": ["Remote Artist"],
            "album": "Remote Album",
            "albumArtist": "Remote Artist",
            "durationMs": null,
            "trackNumber": 1,
            "ids": [],
            "listenedAt": created,
            "client": "dsub",
        },
        "playId": Uuid::now_v7().to_string(),
    })
    .to_string();
    sqlx::query(
        "INSERT INTO scrobble_outbox (
             id, user_id, instance_id, kind, payload, attempts, next_attempt_at, state
         ) VALUES (?, ?, ?, 'listen', ?, 0, ?, 'pending')",
    )
    .bind(id.as_slice())
    .bind(harness.user.as_bytes().as_slice())
    .bind(harness.instances[0].id.as_bytes().as_slice())
    .bind(payload)
    .bind(due)
    .execute(harness.database.writer())
    .await
    .unwrap();
}

#[tokio::test]
async fn a_revoked_token_parks_a_listen() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    harness
        .instance()
        .provider
        .fail(ProviderError::Unauthorized);
    harness
        .outbox
        .listen(
            harness.user,
            Uuid::now_v7(),
            harness.track,
            Some("dsub"),
            1_700_000_000_000,
        )
        .await
        .unwrap();

    let start = current_time();
    let report = harness.deliverer.deliver_due(start).await.unwrap();

    assert_eq!(report.gave_up, 1, "a rejected credential is not retried");
    assert_eq!(report.failed, 0);
    let rows = harness.rows().await;
    assert_eq!(rows[0].state, "failed");
    assert_eq!(rows[0].attempts, 1, "the pass counted the one attempt");
    assert_eq!(
        rows[0].next_attempt_at, start,
        "a parked row is not scheduled again"
    );
    assert_eq!(rows[0].last_error.as_deref(), Some("unauthorized"));

    // No later pass touches a parked row, even once the service would accept
    // it again.
    harness.instance().provider.recover();
    let later = harness
        .deliverer
        .deliver_due(start + OUTBOX_MAX_AGE_MS)
        .await
        .unwrap();
    assert_eq!(
        (later.delivered, later.failed, later.gave_up),
        (0, 0, 0)
    );
    assert_eq!(harness.row_count().await, 1);
}

#[tokio::test]
async fn a_lastfm_rejection_parks_the_listen_and_shows_in_the_counters() {
    // `NotFound` is what the Last.fm plugin answers for a batch the service
    // ignored, so this is the shape a rejected listen arrives in.
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    harness.instance().provider.fail(ProviderError::NotFound);
    let instance = harness.instances[0].id;
    harness
        .outbox
        .listen(
            harness.user,
            Uuid::now_v7(),
            harness.track,
            Some("dsub"),
            1_700_000_000_000,
        )
        .await
        .unwrap();

    harness
        .deliverer
        .deliver_due(current_time())
        .await
        .unwrap();

    let rows = harness.rows().await;
    assert_eq!(rows[0].state, "failed");
    assert_eq!(rows[0].last_error.as_deref(), Some("not found"));
    let counters = harness.outbox.counters(harness.user, instance).await.unwrap();
    assert_eq!(counters.pending, 0);
    assert_eq!(counters.failed, 1, "the rejection is visible to the listener");
    assert_eq!(counters.last_error.as_deref(), Some("not found"));
}

#[tokio::test]
async fn an_unavailable_service_keeps_the_backoff() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    harness
        .instance()
        .provider
        .fail(ProviderError::Unavailable);
    harness
        .outbox
        .listen(
            harness.user,
            Uuid::now_v7(),
            harness.track,
            Some("dsub"),
            1_700_000_000_000,
        )
        .await
        .unwrap();

    let start = current_time();
    let report = harness.deliverer.deliver_due(start).await.unwrap();

    assert_eq!(report.failed, 1);
    assert_eq!(report.gave_up, 0, "a transient failure is not parked");
    let rows = harness.rows().await;
    assert_eq!(rows[0].state, "pending");
    assert_eq!(rows[0].attempts, 1);
    assert_eq!(rows[0].next_attempt_at, start + BACKOFF[0]);
    assert_eq!(rows[0].last_error.as_deref(), Some("unavailable"));
}

#[tokio::test]
async fn a_permanent_batch_rejection_isolates_the_bad_row() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    let library = LibraryRepository::new(&harness.database);
    let (album, _) = insert_album(&library).await;
    let bad = insert_track(&library, album, "Bad", 3).await;
    harness.instance().provider.reject_batches_and("Bad");
    for track in [harness.track, harness.track, bad] {
        harness
            .outbox
            .listen(
                harness.user,
                Uuid::now_v7(),
                track,
                Some("dsub"),
                1_700_000_000_000,
            )
            .await
            .unwrap();
    }

    let report = harness.deliverer.deliver_due(current_time()).await.unwrap();

    assert_eq!(report.delivered, 2, "the accepted rows were delivered");
    assert_eq!(report.gave_up, 1, "only the rejected row was parked");
    let rows = harness.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "failed");
    assert_eq!(rows[0].last_error.as_deref(), Some("not found"));

    let submissions = harness.instances[0].provider.recorded().submissions.clone();
    assert_eq!(submissions.len(), 4, "the batch and one call per row");
    assert_eq!(submissions[0].len(), 3, "the whole batch was tried first");
    assert_eq!(
        submissions[3],
        vec!["Bad".to_owned()],
        "the rejected row was re-submitted on its own"
    );
}

#[tokio::test]
async fn one_malformed_row_does_not_block_the_rest() {
    let harness = Harness::new(&[limits(10, false)]).await;
    harness.link_all().await;
    let queued = current_time();
    insert_listen_row(&harness, queued, queued, "{").await;
    for _ in 0..2 {
        harness
            .outbox
            .listen(
                harness.user,
                Uuid::now_v7(),
                harness.track,
                Some("dsub"),
                1_700_000_000_000,
            )
            .await
            .unwrap();
    }

    let report = harness
        .deliverer
        .deliver_due(current_time())
        .await
        .unwrap();

    assert_eq!(report.delivered, 2);
    assert_eq!(report.gave_up, 1);
    let rows = harness.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "failed");
    assert_eq!(
        rows[0].last_error.as_deref(),
        Some("a queued listen is not valid JSON")
    );
    let recorded = harness.instances[0].provider.recorded();
    assert_eq!(recorded.submissions.len(), 1, "one call carried both rows");
    assert_eq!(recorded.submissions[0].len(), 2);
}

/// Inserts a listen row with a chosen payload.
///
/// # Parameters
///
/// - `harness`: the harness whose first instance the row targets.
/// - `created`: the instant encoded in the row's UUIDv7 id.
/// - `due`: when the row may be delivered.
/// - `payload`: the body to store.
async fn insert_listen_row(harness: &Harness, created: i64, due: i64, payload: &str) {
    let mut id = [0u8; 16];
    id[..6].copy_from_slice(&created.to_be_bytes()[2..]);
    sqlx::query(
        "INSERT INTO scrobble_outbox (
             id, user_id, instance_id, kind, payload, attempts, next_attempt_at, state
         ) VALUES (?, ?, ?, 'listen', ?, 0, ?, 'pending')",
    )
    .bind(id.as_slice())
    .bind(harness.user.as_bytes().as_slice())
    .bind(harness.instances[0].id.as_bytes().as_slice())
    .bind(payload)
    .bind(due)
    .execute(harness.database.writer())
    .await
    .unwrap();
}

#[tokio::test]
async fn a_stalled_plugin_does_not_delay_a_like() {
    let harness = Harness::new(&[limits(10, true)]).await;
    harness.link_all().await;
    harness.instance().provider.stall_limits(Duration::from_secs(3));
    let activity = ActivityService::new(&harness.database)
        .with_scrobble_outbox(Arc::new(harness.outbox.clone()));

    // The like answers without waiting for the plugin: the enqueue that has to
    // ask it runs in its own task.
    let started = Instant::now();
    activity
        .star(harness.user, &[(EntityKind::Track, harness.track)])
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the like waited {:?} on a plugin",
        started.elapsed()
    );

    let rows = wait_for_rows(&harness, 1).await;
    assert_eq!(rows[0].kind, "feedback");
}

#[tokio::test]
async fn limits_are_read_once_within_their_ttl() {
    let harness = Harness::new(&[limits(10, true)]).await;
    harness.link_all().await;
    let provider = harness.instance().provider.clone();
    let activity = ActivityService::new(&harness.database)
        .with_scrobble_outbox(Arc::new(harness.outbox.clone()));

    activity
        .star(harness.user, &[(EntityKind::Track, harness.track)])
        .await
        .unwrap();
    wait_for_rows(&harness, 1).await;
    activity
        .set_feedback(
            harness.user,
            EntityKind::Track,
            harness.track,
            Feedback::Dislike,
        )
        .await
        .unwrap();
    wait_for_rows(&harness, 2).await;

    assert_eq!(
        provider.limits_calls(),
        1,
        "the second enqueue reused the cached limits"
    );
}
