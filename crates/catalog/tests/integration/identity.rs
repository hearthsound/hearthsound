//! Integration tests for identity resolution through the metadata layer.
//!
//! The tests drive the service over the `metadata-fixture` plugin, whose
//! recordings, scores and MusicBrainz ids come from its server-scoped
//! configuration, so a test can build any confidence and any instance order it
//! wants to exercise the layer policy with. What identity resolution promises is
//! a state - the track row's MusicBrainz recording id and the aliases in
//! `external_ids` - so that is what is asserted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use crate::common::{
    Env, HELLO_CATALOG_ID, METADATA_ID, alias_target, cache_row_lifetime, catalog_id, count_rows,
    insert_local_track, metadata_config, stored_artist, stored_track, track_artist_ids,
    write_cache_row,
};
use hearthsound_catalog::{
    ENTITY_CACHE_TTL, IDENTITY_MATCH_THRESHOLD, IdentityRequest, RequestKey, ResolvedRecording,
};
use hearthsound_core::providers::RecordingHint;
use hearthsound_core::{EntityKind, Layer};
use hearthsound_test_support::{HELLO_CATALOG, METADATA_FIXTURE};
use serde_json::json;

/// The recording every identity test resolves, as the fixture is configured
/// with it.
const RECORDING: &str = r#"[
  {
    "title": "Cafe del Mar",
    "artists": ["Energy 52"],
    "artistMbid": "hello-artist-1",
    "isrc": "USRC17607839",
    "mbid": "rec-energy-52",
    "score": 0.98,
    "releases": [{"mbid": "release-energy-52", "title": "Cafe del Mar"}]
  }
]"#;

/// Builds an environment with one metadata instance.
///
/// # Parameters
///
/// - `slug`: the instance slug.
/// - `recordings`: the recordings the fixture knows, as its configuration.
async fn metadata_env(slug: &str, recordings: &str) -> Env {
    let env = Env::new().await;
    env.instance(
        slug,
        METADATA_FIXTURE,
        METADATA_ID,
        metadata_config(serde_json::from_str(recordings).unwrap()),
    )
    .await;
    env.assign(Layer::Metadata, &[slug]).await;
    env
}

/// Builds a hint for the recording the fixture knows.
fn hint() -> RecordingHint {
    RecordingHint {
        title: "Cafe del Mar".to_owned(),
        artists: vec!["Energy 52".to_owned()],
        album: None,
        duration_ms: Some(180_000),
        isrc: None,
    }
}

/// Returns the slug of the instance that resolved a recording.
fn instance_of(resolved: &Option<ResolvedRecording>) -> String {
    resolved.as_ref().unwrap().instance.as_str().to_owned()
}

#[tokio::test]
async fn an_isrc_resolves_to_the_recording_the_instance_knows() {
    let env = metadata_env("meta", RECORDING).await;

    let resolved = env
        .service()
        .resolve_recording(&IdentityRequest::Isrc("USRC17607839".to_owned()))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(resolved.instance.as_str(), "meta");
    assert_eq!(resolved.recording.title, "Cafe del Mar");
    assert_eq!(
        resolved
            .recording
            .ids
            .iter()
            .map(|id| (id.namespace.as_str(), id.value.as_str()))
            .collect::<Vec<_>>(),
        vec![("mb-recording", "rec-energy-52")]
    );
    assert_eq!(resolved.recording.score, 0.98);
    assert_eq!(
        resolved.recording.releases[0].ids[0].value,
        "release-energy-52"
    );
}

#[tokio::test]
async fn an_isrc_no_instance_knows_is_none() {
    let env = metadata_env("meta", RECORDING).await;

    let resolved = env
        .service()
        .resolve_recording(&IdentityRequest::Isrc("GBAYE0000000".to_owned()))
        .await
        .unwrap();

    assert!(resolved.is_none());
}

#[tokio::test]
async fn a_confident_instance_later_in_the_order_still_wins() {
    let env = Env::new().await;
    env.instance(
        "weak",
        METADATA_FIXTURE,
        METADATA_ID,
        metadata_config(json!([{
            "title": "Cafe del Mar",
            "artists": ["Energy 52"],
            "mbid": "rec-weak",
            "score": 0.5,
        }])),
    )
    .await;
    env.instance(
        "strong",
        METADATA_FIXTURE,
        METADATA_ID,
        metadata_config(json!([{
            "title": "Cafe del Mar",
            "artists": ["Energy 52"],
            "mbid": "rec-strong",
            "score": 0.95,
        }])),
    )
    .await;
    env.assign(Layer::Metadata, &["weak", "strong"]).await;

    let resolved = env
        .service()
        .resolve_recording(&IdentityRequest::Hint(hint()))
        .await
        .unwrap();

    assert_eq!(instance_of(&resolved), "strong");
    assert_eq!(resolved.unwrap().recording.ids[0].value, "rec-strong");
}

#[tokio::test]
async fn the_first_confident_instance_wins_over_a_better_later_one() {
    let env = Env::new().await;
    env.instance(
        "first",
        METADATA_FIXTURE,
        METADATA_ID,
        metadata_config(json!([{
            "title": "Cafe del Mar",
            "artists": ["Energy 52"],
            "mbid": "rec-first",
            "score": 0.95,
        }])),
    )
    .await;
    env.instance(
        "second",
        METADATA_FIXTURE,
        METADATA_ID,
        metadata_config(json!([{
            "title": "Cafe del Mar",
            "artists": ["Energy 52"],
            "mbid": "rec-second",
            "score": 0.99,
        }])),
    )
    .await;
    env.assign(Layer::Metadata, &["first", "second"]).await;

    let resolved = env
        .service()
        .resolve_recording(&IdentityRequest::Hint(hint()))
        .await
        .unwrap();

    assert_eq!(
        instance_of(&resolved),
        "first",
        "results of two instances are never merged"
    );
    assert_eq!(resolved.unwrap().recording.ids[0].value, "rec-first");
}

#[tokio::test]
async fn the_best_score_decides_when_no_instance_is_confident() {
    let env = Env::new().await;
    env.instance(
        "weak",
        METADATA_FIXTURE,
        METADATA_ID,
        metadata_config(json!([{
            "title": "Cafe del Mar",
            "artists": ["Energy 52"],
            "mbid": "rec-weak",
            "score": 0.4,
        }])),
    )
    .await;
    env.instance(
        "better",
        METADATA_FIXTURE,
        METADATA_ID,
        metadata_config(json!([{
            "title": "Cafe del Mar",
            "artists": ["Energy 52"],
            "mbid": "rec-better",
            "score": 0.75,
        }])),
    )
    .await;
    env.assign(Layer::Metadata, &["weak", "better"]).await;

    let resolved = env
        .service()
        .resolve_recording(&IdentityRequest::Hint(hint()))
        .await
        .unwrap();

    assert_eq!(instance_of(&resolved), "better");
    let resolved = resolved.unwrap();
    assert!(
        resolved.recording.score < IDENTITY_MATCH_THRESHOLD,
        "neither instance was confident, so the best score decided"
    );
    assert_eq!(resolved.recording.ids[0].value, "rec-better");
}

#[tokio::test]
async fn a_resolution_is_cached_per_instance_and_request() {
    let env = metadata_env("meta", RECORDING).await;

    env.service()
        .resolve_recording(&IdentityRequest::Isrc("USRC17607839".to_owned()))
        .await
        .unwrap();

    assert_eq!(count_rows(&env.database, "catalog_cache").await, 1);
    let lifetime = cache_row_lifetime(
        &env.database,
        env.instance_id("meta"),
        RequestKey::recording_isrc("USRC17607839").as_str(),
    )
    .await;
    assert_eq!(lifetime, Some(millis(ENTITY_CACHE_TTL)));
}

#[tokio::test]
async fn a_cached_resolution_is_answered_without_asking_the_instance() {
    let env = metadata_env("meta", "[]").await;
    let stored = r#"[{"ids":[{"namespace":"mb-recording","value":"cached-rec"}],
        "title":"Cached Song","artists":[],"duration_ms":null,"releases":[],"score":0.99}]"#;
    write_cache_row(
        &env.database,
        env.instance_id("meta"),
        RequestKey::recording_isrc("USRC17607839").as_str(),
        stored,
        None,
    )
    .await;

    let resolved = env
        .service()
        .resolve_recording(&IdentityRequest::Isrc("USRC17607839".to_owned()))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(resolved.recording.title, "Cached Song");
    assert_eq!(resolved.recording.ids[0].value, "cached-rec");
}

#[tokio::test]
async fn resolving_a_track_writes_its_musicbrainz_identity() {
    let env = Env::new().await;
    env.instance(
        "meta",
        METADATA_FIXTURE,
        METADATA_ID,
        metadata_config(json!([{
            "title": "Hello, world",
            "artists": ["Hello Artist"],
            "artistMbid": "hello-artist-1",
            "mbid": "rec-hello",
            "score": 0.97,
            "releases": [{"mbid": "release-hello", "title": "Hello Album"}],
        }])),
    )
    .await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, json!({}))
        .await;
    env.assign(Layer::Metadata, &["meta"]).await;
    env.assign(Layer::Catalog, &["hello"]).await;
    let service = env.service();

    let album = service
        .materialize_album(&catalog_id("hello", EntityKind::Album, "hello-album-1"))
        .await
        .unwrap();
    let track = album.tracks[0];

    let resolved = service
        .resolve_track_identity(
            track,
            &IdentityRequest::Hint(RecordingHint {
                title: "Hello, world".to_owned(),
                artists: vec!["Hello Artist".to_owned()],
                album: None,
                duration_ms: Some(180_000),
                isrc: None,
            }),
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(resolved.instance.as_str(), "meta");
    assert_eq!(
        stored_track(&env.database, track.uuid())
            .await
            .unwrap()
            .mb_recording_id
            .as_deref(),
        Some("rec-hello")
    );
    assert_eq!(
        alias_target(
            &env.database,
            "mb-recording",
            EntityKind::Track,
            "rec-hello"
        )
        .await,
        Some(track.uuid().as_bytes().to_vec())
    );
    assert_eq!(
        alias_target(
            &env.database,
            "mb-release",
            EntityKind::Album,
            "release-hello"
        )
        .await,
        Some(album.album.uuid().as_bytes().to_vec()),
        "a resolved release names the album the track belongs to"
    );

    let artists = track_artist_ids(&env.database, track.uuid()).await;
    assert_eq!(artists.len(), 1, "the resolution reuses the stored artist");
    assert_eq!(
        alias_target(
            &env.database,
            "mb-artist",
            EntityKind::Artist,
            "hello-artist-1"
        )
        .await,
        Some(artists[0].clone())
    );
    assert_eq!(count_rows(&env.database, "artists").await, 1);
}

#[tokio::test]
async fn an_isrc_resolution_applies_to_a_local_track() {
    let env = metadata_env("meta", RECORDING).await;
    let local = insert_local_track(
        &env.database,
        "Cafe del Mar",
        Some("USRC17607839"),
        Some(180_000),
    )
    .await;

    let resolved = env
        .service()
        .resolve_track_identity(local, &IdentityRequest::Isrc("USRC17607839".to_owned()))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(resolved.recording.ids[0].value, "rec-energy-52");
    let row = stored_track(&env.database, local.uuid()).await.unwrap();
    assert_eq!(row.mb_recording_id.as_deref(), Some("rec-energy-52"));
    assert_eq!(
        row.origin, "local",
        "identity resolution is not materialization"
    );
    assert_eq!(
        alias_target(
            &env.database,
            "mb-recording",
            EntityKind::Track,
            "rec-energy-52"
        )
        .await,
        Some(local.uuid().as_bytes().to_vec())
    );

    // A track that belongs to no album has nowhere to hang a release alias, so
    // the resolved release is dropped rather than aliased to the track.
    assert_eq!(
        alias_target(
            &env.database,
            "mb-release",
            EntityKind::Album,
            "release-energy-52"
        )
        .await,
        None
    );

    assert_eq!(count_rows(&env.database, "artists").await, 1);
    let artist = stored_artist(
        &env.database,
        uuid::Uuid::from_slice(
            &alias_target(
                &env.database,
                "mb-artist",
                EntityKind::Artist,
                "hello-artist-1",
            )
            .await
            .unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        artist,
        ("catalog".to_owned(), Some("hello-artist-1".to_owned()))
    );
}

#[tokio::test]
async fn a_recording_no_instance_knows_leaves_the_track_alone() {
    let env = metadata_env("meta", "[]").await;
    let local = insert_local_track(&env.database, "Unknown Song", None, None).await;

    let resolved = env
        .service()
        .resolve_track_identity(local, &IdentityRequest::Hint(hint()))
        .await
        .unwrap();

    assert!(resolved.is_none());
    assert_eq!(
        stored_track(&env.database, local.uuid())
            .await
            .unwrap()
            .mb_recording_id,
        None
    );
    assert_eq!(count_rows(&env.database, "external_ids").await, 0);
}

#[tokio::test]
async fn resolution_without_a_metadata_instance_is_none() {
    let env = Env::new().await;

    assert!(
        env.service()
            .resolve_recording(&IdentityRequest::Hint(hint()))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn resolving_an_unknown_track_is_not_found() {
    let env = metadata_env("meta", RECORDING).await;
    let missing = hearthsound_core::EntityId::new(hearthsound_core::IdPrefix::Track);

    let error = env
        .service()
        .resolve_track_identity(missing, &IdentityRequest::Isrc("USRC17607839".to_owned()))
        .await
        .unwrap_err();

    assert!(
        matches!(error, hearthsound_catalog::CatalogError::NotFound),
        "{error}"
    );
}

/// Returns a duration in whole milliseconds.
///
/// # Parameters
///
/// - `duration`: the duration to convert.
fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap()
}
