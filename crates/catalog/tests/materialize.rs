//! Integration tests for materializing catalog entities through the alias
//! table.
//!
//! Every test drives the service over the `hello-catalog` fixture, whose
//! entities carry canonical identifiers, and reads the rows back out of the
//! database: what materialization promises is a state (a row with origin
//! `catalog` and availability `remote`, an alias that keeps the original id
//! resolving), so that is what is asserted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    Env, HELLO_CATALOG_ID, alias_target, catalog_id, count_rows, insert_local_track, stored_album,
    stored_artist, stored_track, track_artist_ids, write_alias,
};
use hearthsound_core::{EntityKind, IdPrefix, Layer};
use hearthsound_test_support::HELLO_CATALOG;
use serde_json::{Value, json};

/// Builds the environment a materialization test starts from.
///
/// # Parameters
///
/// - `config`: the `hello` instance's server-scoped configuration.
async fn hello_env(config: Value) -> Env {
    let env = Env::new().await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, config)
        .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    env
}

#[tokio::test]
async fn a_track_is_materialized_with_its_row_and_its_aliases() {
    let env = hello_env(json!({})).await;
    let service = env.service();

    let track = service
        .materialize_track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();

    assert_eq!(track.kind(), IdPrefix::Track);
    let row = stored_track(&env.database, track.uuid()).await.unwrap();
    assert_eq!(row.title, "Hello, world");
    assert_eq!(row.origin, "catalog");
    assert_eq!(row.availability, "remote");
    assert_eq!(row.isrc, None);
    assert_eq!(row.mb_recording_id.as_deref(), Some("hello-recording-1"));

    let key = track.uuid().as_bytes().to_vec();
    assert_eq!(
        alias_target(&env.database, "p:hello", EntityKind::Track, "hello-track-1").await,
        Some(key.clone()),
        "the provider id must keep resolving to the row"
    );
    assert_eq!(
        alias_target(
            &env.database,
            "mb-recording",
            EntityKind::Track,
            "hello-recording-1"
        )
        .await,
        Some(key.clone()),
        "a canonical identifier of the entity is recorded too"
    );
    assert_eq!(
        alias_target(&env.database, "hello", EntityKind::Track, "hello-track-1").await,
        None,
        "a namespace that is not canonical is not recorded"
    );

    // The credited artist is stored with the origin of a catalog row, and both
    // the MusicBrainz artist id and the provider id of the credit alias it.
    let artists = track_artist_ids(&env.database, track.uuid()).await;
    assert_eq!(artists.len(), 1);
    let artist_id = uuid::Uuid::from_slice(&artists[0]).unwrap();
    assert_eq!(
        stored_artist(&env.database, artist_id).await,
        Some(("catalog".to_owned(), Some("hello-artist-1".to_owned())))
    );
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
    assert_eq!(
        alias_target(
            &env.database,
            "p:hello",
            EntityKind::Artist,
            "hello-artist-1"
        )
        .await,
        Some(artists[0].clone()),
        "the provider id of the credit resolves to the stored artist"
    );
    assert_eq!(count_rows(&env.database, "tracks").await, 1);
}

#[tokio::test]
async fn a_credited_artist_resolves_through_its_provider_id() {
    let env = hello_env(json!({})).await;
    let service = env.service();
    let track = service
        .materialize_track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();
    let credited = track_artist_ids(&env.database, track.uuid()).await;
    let credited = hearthsound_core::EntityId::from_uuid(
        IdPrefix::Artist,
        uuid::Uuid::from_slice(&credited[0]).unwrap(),
    );

    let resolved = service
        .materialize_artist(&catalog_id("hello", EntityKind::Artist, "hello-artist-1"))
        .await
        .unwrap();

    assert_eq!(
        resolved, credited,
        "the artist id of a credit names the row the track credits"
    );
    assert_eq!(
        count_rows(&env.database, "artists").await,
        1,
        "resolving the credited artist does not store a second row"
    );
}

#[tokio::test]
async fn materializing_an_entity_twice_returns_one_row() {
    let env = hello_env(json!({})).await;
    let service = env.service();
    let id = catalog_id("hello", EntityKind::Track, "hello-track-1");

    let first = service.materialize_track(&id).await.unwrap();
    let second = service.materialize_track(&id).await.unwrap();

    assert_eq!(first, second);
    assert_eq!(count_rows(&env.database, "tracks").await, 1);
    // The track's provider id and MusicBrainz recording id, the credited
    // artist's MusicBrainz artist id and provider id: one alias each, written
    // once.
    assert_eq!(count_rows(&env.database, "external_ids").await, 4);
    assert!(
        service.provider_aliases(first).await.unwrap().len() == 1,
        "the provider alias is recorded once"
    );
}

#[tokio::test]
async fn a_materialized_id_resolves_without_the_instance() {
    let env = hello_env(json!({})).await;
    let id = catalog_id("hello", EntityKind::Track, "hello-track-1");
    let stored = env.service().materialize_track(&id).await.unwrap();

    env.assign(Layer::Catalog, &[]).await;

    assert_eq!(
        env.service().materialize_track(&id).await.unwrap(),
        stored,
        "the alias is what keeps the id resolving, not the instance"
    );
}

#[tokio::test]
async fn a_track_whose_isrc_is_already_stored_is_reused() {
    let env = hello_env(json!({"isrc": "USRC17607839"})).await;
    let local = insert_local_track(
        &env.database,
        "Local Song",
        Some("USRC17607839"),
        Some(1_000),
    )
    .await;

    let stored = env
        .service()
        .materialize_track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();

    assert_eq!(
        stored, local,
        "the stored recording is reused, not duplicated"
    );
    assert_eq!(count_rows(&env.database, "tracks").await, 1);
    let row = stored_track(&env.database, local.uuid()).await.unwrap();
    assert_eq!(row.origin, "local", "an existing row keeps its origin");
    assert_eq!(row.title, "Local Song");

    let key = local.uuid().as_bytes().to_vec();
    assert_eq!(
        alias_target(&env.database, "p:hello", EntityKind::Track, "hello-track-1").await,
        Some(key.clone()),
        "the provider id of the reused recording is recorded"
    );
    assert_eq!(
        alias_target(&env.database, "isrc", EntityKind::Track, "USRC17607839").await,
        Some(key),
        "the ISRC the two entities share is recorded"
    );
}

#[tokio::test]
async fn an_album_is_materialized_with_its_tracks_in_order() {
    let env = hello_env(json!({})).await;
    let service = env.service();

    let album = service
        .materialize_album(&catalog_id("hello", EntityKind::Album, "hello-album-1"))
        .await
        .unwrap();

    assert_eq!(album.tracks.len(), 2, "every track is materialized");
    assert_eq!(
        stored_track(&env.database, album.tracks[0].uuid())
            .await
            .unwrap()
            .title,
        "Hello, world"
    );
    assert_eq!(
        stored_track(&env.database, album.tracks[1].uuid())
            .await
            .unwrap()
            .title,
        "Hello, again"
    );
    for track in &album.tracks {
        let row = stored_track(&env.database, track.uuid()).await.unwrap();
        assert_eq!(row.album_id, Some(album.album.uuid().as_bytes().to_vec()));
        assert_eq!(row.availability, "remote");
    }

    assert_eq!(
        stored_album(&env.database, album.album.uuid()).await,
        Some((
            "catalog".to_owned(),
            Some("hello-release-1".to_owned()),
            None,
            Some("0088888888888".to_owned())
        ))
    );
    assert_eq!(
        alias_target(&env.database, "p:hello", EntityKind::Album, "hello-album-1").await,
        Some(album.album.uuid().as_bytes().to_vec())
    );
    assert_eq!(
        alias_target(&env.database, "upc", EntityKind::Album, "0088888888888").await,
        Some(album.album.uuid().as_bytes().to_vec())
    );
    assert_eq!(
        alias_target(&env.database, "p:hello", EntityKind::Track, "hello-track-2").await,
        Some(album.tracks[1].uuid().as_bytes().to_vec()),
        "every track of the album keeps resolving through its provider id"
    );
}

#[tokio::test]
async fn an_album_that_is_already_materialized_is_reported_from_its_rows() {
    let env = hello_env(json!({})).await;
    let service = env.service();
    let id = catalog_id("hello", EntityKind::Album, "hello-album-1");

    let first = service.materialize_album(&id).await.unwrap();
    let second = service.materialize_album(&id).await.unwrap();

    assert_eq!(first, second);
    assert_eq!(count_rows(&env.database, "albums").await, 1);
    assert_eq!(count_rows(&env.database, "tracks").await, 2);
    assert_eq!(count_rows(&env.database, "artists").await, 1);

    assert_eq!(
        service
            .materialize_track(&catalog_id("hello", EntityKind::Track, "hello-track-2"))
            .await
            .unwrap(),
        first.tracks[1],
        "a track materialized with its album keeps resolving to the same row"
    );
    assert_eq!(count_rows(&env.database, "tracks").await, 2);
}

#[tokio::test]
async fn an_artist_is_materialized_with_its_musicbrainz_alias() {
    let env = hello_env(json!({})).await;
    let service = env.service();
    let id = catalog_id("hello", EntityKind::Artist, "hello-artist-1");

    let artist = service.materialize_artist(&id).await.unwrap();
    let again = service.materialize_artist(&id).await.unwrap();

    assert_eq!(artist, again);
    assert_eq!(artist.kind(), IdPrefix::Artist);
    assert_eq!(count_rows(&env.database, "artists").await, 1);
    assert_eq!(
        stored_artist(&env.database, artist.uuid()).await,
        Some(("catalog".to_owned(), Some("hello-artist-1".to_owned())))
    );
    assert_eq!(
        alias_target(
            &env.database,
            "p:hello",
            EntityKind::Artist,
            "hello-artist-1"
        )
        .await,
        Some(artist.uuid().as_bytes().to_vec())
    );
    assert_eq!(
        alias_target(
            &env.database,
            "mb-artist",
            EntityKind::Artist,
            "hello-artist-1"
        )
        .await,
        Some(artist.uuid().as_bytes().to_vec())
    );
}

#[tokio::test]
async fn provider_aliases_report_the_instance_that_issued_the_entity() {
    let env = hello_env(json!({})).await;
    let service = env.service();
    let track = service
        .materialize_track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();
    write_alias(
        &env.database,
        "p:deezer",
        EntityKind::Track,
        "3135556",
        track,
    )
    .await;

    let aliases = service.provider_aliases(track).await.unwrap();

    assert_eq!(
        aliases
            .iter()
            .map(|alias| (alias.slug.as_str(), alias.provider_id.as_str()))
            .collect::<Vec<_>>(),
        vec![("deezer", "3135556"), ("hello", "hello-track-1")]
    );

    let local = insert_local_track(&env.database, "Untouched", None, None).await;
    assert!(
        service.provider_aliases(local).await.unwrap().is_empty(),
        "a row no catalog entity named has no provider alias"
    );
}

#[tokio::test]
async fn entity_by_alias_reads_the_stored_row() {
    let env = hello_env(json!({})).await;
    let service = env.service();
    let track = service
        .materialize_track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();

    assert_eq!(
        service
            .entity_by_alias("p:hello", EntityKind::Track, "hello-track-1")
            .await
            .unwrap(),
        Some(track)
    );
    assert_eq!(
        service
            .entity_by_alias("mb-recording", EntityKind::Track, "hello-recording-1")
            .await
            .unwrap(),
        Some(track)
    );
    assert_eq!(
        service
            .entity_by_alias("p:hello", EntityKind::Album, "hello-track-1")
            .await
            .unwrap(),
        None,
        "an alias of another entity kind does not match"
    );
    assert_eq!(
        service
            .entity_by_alias("p:hello", EntityKind::Track, "nowhere")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn an_id_of_the_wrong_kind_is_not_found() {
    let env = hello_env(json!({})).await;
    let service = env.service();

    let error = service
        .materialize_track(&catalog_id("hello", EntityKind::Album, "hello-album-1"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, hearthsound_catalog::CatalogError::NotFound),
        "{error}"
    );

    let error = service
        .materialize_album(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, hearthsound_catalog::CatalogError::NotFound),
        "{error}"
    );

    assert_eq!(count_rows(&env.database, "tracks").await, 0);
}
