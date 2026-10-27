//! The `discovery.sync` job: the generated playlists of every linked service.
//!
//! A discovery service generates playlists for one listener - a weekly
//! selection, a mix built from what they played - and the server keeps them as
//! the listener's own temporary playlists. This module is the job that reads
//! them: for one listener or for every listener with a link, it asks the
//! discovery layer for the generated playlists, resolves the suggested tracks
//! into stored rows through the catalog, and writes the playlist into the slot
//! its (owner, instance, reference) names.
//!
//! Three rules follow from the design:
//!
//! - a generated playlist is temporary and pins nothing, so its tracks stay
//!   evictable and a listener who never plays it costs no storage;
//! - a refresh replaces the tracks of the existing playlist in one transaction,
//!   so the playlist id a client holds keeps working and a reader never sees a
//!   half-written playlist;
//! - a track the catalog cannot name is dropped rather than stored as a
//!   placeholder, and the rest of the playlist is still written;
//! - a refresh that cannot resolve or store every suggestion it has is skipped
//!   instead of written short, so a transient provider failure never deletes
//!   tracks from a playlist.
//!
//! A playlist the service already retired is not written again: the service
//! may keep listing it for a while, and writing it would recreate a playlist
//! the expiry job deletes on its next pass. The sync skips it and leaves any
//! copy an earlier sync stored to that job.
//!
//! The system schedule runs the job for every linked listener once a day; a
//! listener who links a discovery instance gets the same job immediately
//! through the account-linking route, which is what makes a freshly linked
//! account show its playlists without waiting for the next morning.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use hearthsound_catalog::DiscoveryService;
use hearthsound_core::Layer;
use hearthsound_core::events::{Audience, EventBus, PlaylistChanged, ServerEvent};
use hearthsound_jobs::{Job, JobError, JobHandler};
use hearthsound_plugin_host::{ProviderRegistry, UserConfigService};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ActivityError;
use crate::now_millis;
use crate::playlists::{Playlists, SyncedPlaylist};

/// The job kind of the generated-playlist sync.
pub const DISCOVERY_SYNC_JOB_KIND: &str = "discovery.sync";

/// The cron expression of the daily generated-playlist sync: four in the
/// morning.
///
/// A generated playlist changes at the pace of the services that produce it -
/// weekly charts, daily mixes - so one pass a day is enough, and an early
/// morning hour keeps the work out of the evening the listener is most likely
/// to browse in.
pub const DISCOVERY_SYNC_SCHEDULE: &str = "0 4 * * *";

/// How long a generated playlist lives when the service names no expiry.
///
/// Eight days: long enough that a weekly playlist survives one missed sync, and
/// short enough that a playlist the service stopped generating disappears on
/// its own instead of staying in the listener's list forever.
const DEFAULT_PLAYLIST_LIFETIME_MS: i64 = 8 * 24 * 60 * 60 * 1_000;

/// How many recommended albums the sync resolves ahead of a Home read.
///
/// Twenty, the shelf's default size: the same work the endpoint does, moved
/// into the background job that already runs when a discovery link is stored.
const WARM_ALBUMS: u32 = 20;

/// The payload of a `discovery.sync` job.
///
/// A job without a listener syncs every linked listener, which is what the
/// system schedule enqueues. A job with one syncs just that listener, which is
/// what a freshly stored link enqueues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverySyncPayload {
    /// The listener to sync, or `None` for every linked listener.
    pub user_id: Option<Uuid>,
}

/// What one sync pass wrote, and which listeners it could not sync.
///
/// One listener's failure is not another's: a service that rejects one account's
/// link must not keep the rest of the household's playlists from refreshing, so
/// the pass collects the failures and carries on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// How many playlists were written.
    pub written: usize,
    /// The listeners whose sync failed, in the order they were tried.
    pub failed_users: Vec<Uuid>,
}

/// The generated-playlist sync.
///
/// One handler is built by the composition root and registered for the job
/// kind; it shares the discovery layer, the provider registry and the playlist
/// repository with the rest of the process.
#[derive(Clone)]
pub struct DiscoverySyncJobHandler {
    /// The discovery layer the generated playlists come from.
    discovery: Arc<DiscoveryService>,
    /// The provider registry the linked listeners are read from.
    registry: Arc<ProviderRegistry>,
    /// The stored per-user links.
    user_configs: UserConfigService,
    /// The playlist repository the slots are written through.
    playlists: Playlists,
    /// The event bus a written playlist is published on.
    events: Arc<EventBus>,
}

impl std::fmt::Debug for DiscoverySyncJobHandler {
    /// Reports the handler without the registry or the stored links, which
    /// hold plugin configuration and tokens.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DiscoverySyncJobHandler")
    }
}

impl DiscoverySyncJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `discovery`: the discovery layer the generated playlists come from.
    /// - `registry`: the provider registry, whose discovery layer decides
    ///   which instances have linked listeners.
    /// - `user_configs`: the stored per-user links.
    /// - `playlists`: the playlist repository.
    /// - `events`: the event bus a written playlist is published on.
    ///
    /// # Returns
    ///
    /// The handler.
    pub fn new(
        discovery: Arc<DiscoveryService>,
        registry: Arc<ProviderRegistry>,
        user_configs: UserConfigService,
        playlists: Playlists,
        events: Arc<EventBus>,
    ) -> DiscoverySyncJobHandler {
        DiscoverySyncJobHandler {
            discovery,
            registry,
            user_configs,
            playlists,
            events,
        }
    }

    /// Returns every listener linked to a discovery instance.
    ///
    /// # Returns
    ///
    /// The listener ids in ascending order, each once even when they linked
    /// several discovery instances.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::ProviderConfig`] when a link table cannot be read.
    pub async fn linked_users(&self) -> Result<Vec<Uuid>, ActivityError> {
        let mut users = BTreeSet::new();
        for instance in self.registry.layers(Layer::Discovery) {
            if instance.discovery().is_none() {
                continue;
            }
            for linked in self.user_configs.linked_users(instance.id()).await? {
                users.insert(linked.user_id);
            }
        }
        Ok(users.into_iter().collect())
    }

    /// Syncs the generated playlists of every linked listener.
    ///
    /// A listener whose sync fails is counted and logged, and the pass moves on
    /// to the next one, so one service's rejected link cannot stop the rest of
    /// the household from refreshing. The failure resurfaces at the next
    /// scheduled pass.
    ///
    /// # Returns
    ///
    /// What the pass wrote and which listeners it could not sync.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError`] only when the linked listeners themselves
    /// cannot be read.
    pub async fn sync_all(&self) -> Result<SyncReport, ActivityError> {
        let mut report = SyncReport::default();
        for user in self.linked_users().await? {
            match self.sync_user(user).await {
                Ok(written) => report.written += written,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        user = %user,
                        "could not sync a listener's generated playlists"
                    );
                    report.failed_users.push(user);
                }
            }
        }
        Ok(report)
    }

    /// Syncs the generated playlists of one listener.
    ///
    /// Every playlist the listener's linked services generated is written to
    /// its slot; a playlist that already exists keeps its id and has its fields
    /// and tracks replaced. A playlist whose service expiry has already passed
    /// is skipped, so a service that keeps listing a retired playlist does not
    /// recreate it after the expiry job removed it. A suggested track no
    /// catalog can name is dropped, and the rest of the playlist is written; a
    /// catalog search that fails skips the whole playlist instead, because
    /// writing the shorter list would delete the entries the failure merely hid.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener to sync.
    ///
    /// # Returns
    ///
    /// How many playlists were written.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError`] when a discovery answer, a catalog search or a
    /// playlist write fails.
    pub async fn sync_user(&self, user: Uuid) -> Result<usize, ActivityError> {
        let now = now_millis();
        let generated = self.discovery.playlists(user).await?;
        let mut written = 0;
        for instance_playlist in generated {
            let summary = instance_playlist.playlist.summary;
            // The playlist's slot names the instance by its row, and the
            // discovery layer answers with its slug: an instance that left the
            // registry while the pass ran has no slot to write any more.
            let Some(source_instance_id) = self
                .registry
                .instance(instance_playlist.instance.as_str())
                .map(|instance| instance.id())
            else {
                tracing::debug!(
                    instance = instance_playlist.instance.as_str(),
                    "skipped a generated playlist of an instance that is gone"
                );
                continue;
            };
            // A service keeps listing a playlist for a while after it retires
            // it. Writing such a slot would store a playlist the expiry job
            // deletes again, and every sync would repeat that cycle, so a
            // playlist whose service expiry has passed is left to the expiry
            // job, which removes any copy an earlier sync stored.
            if summary
                .expires_at
                .is_some_and(|expires_at| i64::try_from(expires_at).unwrap_or(i64::MAX) <= now)
            {
                tracing::debug!(
                    reference = summary.reference.as_str(),
                    "skipped a generated playlist the service already retired"
                );
                continue;
            }
            let suggestions = instance_playlist.playlist.tracks;
            let limit = u32::try_from(suggestions.len()).unwrap_or(u32::MAX);
            // A catalog that cannot answer is not a playlist whose tracks were
            // renamed: writing what it did answer would delete the entries the
            // failure hid, so the whole playlist is skipped until the next pass.
            let resolved = match self.discovery.resolve_tracks(&suggestions, limit).await {
                Ok(resolved) => resolved,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        playlist = summary.reference.as_str(),
                        user = %user,
                        "kept a generated playlist because the catalog could not resolve it"
                    );
                    continue;
                }
            };
            // The resolver stored every row it named, so a suggested track no
            // catalog could name is the only thing dropped here.
            let tracks: Vec<_> = resolved.into_iter().flatten().collect();
            let expires_at = summary
                .expires_at
                .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX))
                .unwrap_or(now + DEFAULT_PLAYLIST_LIFETIME_MS);
            let write = SyncedPlaylist {
                owner_user_id: user,
                source_instance_id,
                source_ref: &summary.reference,
                name: &summary.name,
                comment: summary.description.as_deref(),
                expires_at,
                tracks: &tracks,
            };
            // A playlist the listener has edited is no longer the service's:
            // the slot is recognised and skipped, with nothing written and
            // nothing published.
            let stored = match self.playlists.upsert_generated(&write, now).await {
                Ok(written) => written,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        playlist = summary.reference.as_str(),
                        user = %user,
                        "a generated playlist could not be written"
                    );
                    continue;
                }
            };
            let Some(playlist) = stored else {
                continue;
            };
            self.events.publish_for(
                Audience::User(user),
                ServerEvent::PlaylistChanged(PlaylistChanged {
                    playlist_id: playlist.id.to_string(),
                    change: "synced".to_owned(),
                }),
            );
            written += 1;
        }
        // Warm the Home shelf with the work the endpoint would do anyway. The
        // sync runs in the background and on link, so a listener's first Home
        // read finds the albums already resolved and stored instead of waiting
        // on a throttled catalog.
        if let Err(error) = self.discovery.recommended_albums(user, WARM_ALBUMS).await {
            tracing::debug!(
                %error,
                user = %user,
                "could not warm the recommended albums"
            );
        }
        Ok(written)
    }
}

#[async_trait]
impl JobHandler for DiscoverySyncJobHandler {
    /// Syncs the listener the payload names, or every linked listener.
    ///
    /// # Parameters
    ///
    /// - `job`: the claimed job, whose payload is a [`DiscoverySyncPayload`].
    ///
    /// # Errors
    ///
    /// Returns [`JobError`] when the payload is not the JSON this job writes or
    /// when a sync fails, so the queue retries it on its retry schedule.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let payload = match job.payload.as_deref() {
            Some(text) => serde_json::from_str::<DiscoverySyncPayload>(text).map_err(|error| {
                JobError::new(format!("invalid discovery sync payload: {error}"))
            })?,
            None => DiscoverySyncPayload { user_id: None },
        };
        let report = match payload.user_id {
            Some(user) => SyncReport {
                written: self
                    .sync_user(user)
                    .await
                    .map_err(|error| JobError::new(error.to_string()))?,
                failed_users: Vec::new(),
            },
            // A pass that synced most listeners must not be retried wholesale
            // because one of them failed; its failure resurfaces tomorrow.
            None => self
                .sync_all()
                .await
                .map_err(|error| JobError::new(error.to_string()))?,
        };
        tracing::debug!(
            kind = %job.kind,
            written = report.written,
            failed = report.failed_users.len(),
            "synced generated playlists"
        );
        Ok(())
    }
}
