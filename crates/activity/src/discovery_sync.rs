//! The `discovery.sync` job: the generated playlists of every linked service.
//!
//! A discovery service generates playlists for one listener - a weekly
//! selection, a mix built from what they played - and the server keeps them as
//! the listener's own temporary playlists. This module is the job that reads
//! them: for one listener or for every listener with a link, it asks the
//! discovery layer for the generated playlists, resolves the suggested tracks
//! through the catalog, materializes each resolved entity into a stored row,
//! and writes the playlist into the slot its (owner, instance, reference) names.
//!
//! Three rules follow from the design:
//!
//! - a generated playlist is temporary and pins nothing, so its tracks stay
//!   evictable and a listener who never plays it costs no storage;
//! - a refresh replaces the tracks of the existing playlist in one transaction,
//!   so the playlist id a client holds keeps working and a reader never sees a
//!   half-written playlist;
//! - a track the catalog cannot name is dropped rather than stored as a
//!   placeholder, and the rest of the playlist is still written.
//!
//! The system schedule runs the job for every linked listener once a day; a
//! listener who links a discovery instance gets the same job immediately
//! through the account-linking route, which is what makes a freshly linked
//! account show its playlists without waiting for the next morning.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use hearthsound_catalog::{CatalogService, DiscoveryService, materialize_entity};
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

/// The generated-playlist sync.
///
/// One handler is built by the composition root and registered for the job
/// kind; it shares the discovery layer, the catalog layer, the provider
/// registry and the playlist repository with the rest of the process.
#[derive(Clone)]
pub struct DiscoverySyncJobHandler {
    /// The discovery layer the generated playlists come from.
    discovery: Arc<DiscoveryService>,
    /// The catalog layer a suggested entity is materialized through.
    catalog: Arc<CatalogService>,
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
    /// - `catalog`: the catalog layer a suggested track is materialized
    ///   through.
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
        catalog: Arc<CatalogService>,
        registry: Arc<ProviderRegistry>,
        user_configs: UserConfigService,
        playlists: Playlists,
        events: Arc<EventBus>,
    ) -> DiscoverySyncJobHandler {
        DiscoverySyncJobHandler {
            discovery,
            catalog,
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
    /// # Returns
    ///
    /// How many playlists were written.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError`] when a link, a discovery answer, a catalog
    /// lookup or a playlist write fails.
    pub async fn sync_all(&self) -> Result<usize, ActivityError> {
        let mut written = 0;
        for user in self.linked_users().await? {
            written += self.sync_user(user).await?;
        }
        Ok(written)
    }

    /// Syncs the generated playlists of one listener.
    ///
    /// Every playlist the listener's linked services generated is written to
    /// its slot; a playlist that already exists keeps its id and has its fields
    /// and tracks replaced. A suggested track no catalog can name is dropped
    /// with a debug log line, and the rest of the playlist is written.
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
            let suggestions = instance_playlist.playlist.tracks;
            let limit = u32::try_from(suggestions.len()).unwrap_or(u32::MAX);
            let resolved = self.discovery.resolve_tracks(&suggestions, limit).await?;
            let mut tracks = Vec::with_capacity(resolved.len());
            for result in resolved {
                match materialize_entity(&self.catalog, &result.id).await {
                    Ok(track) => tracks.push(track),
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            playlist = summary.reference.as_str(),
                            "a suggested track of a generated playlist could not be stored"
                        );
                    }
                }
            }
            let expires_at = summary
                .expires_at
                .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX))
                .unwrap_or(now + DEFAULT_PLAYLIST_LIFETIME_MS);
            let write = SyncedPlaylist {
                owner_user_id: user,
                source_instance_id: instance_playlist.instance,
                source_ref: &summary.reference,
                name: &summary.name,
                comment: summary.description.as_deref(),
                expires_at,
                tracks: &tracks,
            };
            let playlist = self.playlists.upsert_generated(&write, now).await?;
            self.events.publish_for(
                Audience::User(user),
                ServerEvent::PlaylistChanged(PlaylistChanged {
                    playlist_id: playlist.id.to_string(),
                    change: "synced".to_owned(),
                }),
            );
            written += 1;
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
        let written = match payload.user_id {
            Some(user) => self
                .sync_user(user)
                .await
                .map_err(|error| JobError::new(error.to_string()))?,
            None => self
                .sync_all()
                .await
                .map_err(|error| JobError::new(error.to_string()))?,
        };
        tracing::debug!(kind = %job.kind, written, "synced generated playlists");
        Ok(())
    }
}
