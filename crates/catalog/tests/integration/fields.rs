//! Fields: the administrator's precedence and layer order decide which stored
//! source states an album's title and release details.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_catalog::sources::{self, Operation, Status};
use hearthsound_catalog::{LayerOrder, Precedence};
use hearthsound_core::EntityKind;
use hearthsound_library::{AlbumInput, ArtistCredit};
use hearthsound_test_support::METADATA_FIXTURE;
use serde_json::json;

use crate::common::{self, Env, METADATA_ID};

#[tokio::test]
async fn precedence_and_layer_order_choose_the_stored_title() {
    let env = Env::new().await;
    let first = env
        .instance(
            "metaa",
            METADATA_FIXTURE,
            METADATA_ID,
            common::metadata_config(json!([])),
        )
        .await;
    let second = env
        .instance(
            "metab",
            METADATA_FIXTURE,
            METADATA_ID,
            common::metadata_config(json!([])),
        )
        .await;

    let library = env.library();
    let mut album = AlbumInput::new("Tag Title");
    album.display_artist = Some("Tag Artist".to_owned());
    album.release_date = Some("1999-01-01".to_owned());
    album.album_type = Some("album".to_owned());
    album.year = Some(1999);
    album.file_tags = Some(json!({
        "title": "Tag Title",
        "displayArtist": "Tag Artist",
        "releaseDate": "1999-01-01",
        "year": 1999,
        "albumType": "album",
    }));
    let id = library
        .upsert_album(&album, &[ArtistCredit::new("Tag Artist", "main")])
        .await
        .unwrap();

    store_release(&env, id, first.id, "Provider A").await;
    store_release(&env, id, second.id, "Provider B").await;

    // `metadata.precedence = tags`: the file's own title wins over both.
    let tag_order = LayerOrder {
        metadata: vec![first.id, second.id],
        ..LayerOrder::default()
    };
    env.recompute(EntityKind::Album, id, Precedence::Tags, &tag_order)
        .await;
    let stored = library.get_album(id).await.unwrap().unwrap();
    assert_eq!(stored.title, "Tag Title");
    assert_eq!(stored.display_artist.as_deref(), Some("Tag Artist"));
    assert_eq!(stored.release_date.as_deref(), Some("1999-01-01"));
    assert_eq!(stored.album_type.as_deref(), Some("album"));

    // `plugins`: the first instance in the metadata layer states the title.
    let plugins_first = LayerOrder {
        metadata: vec![first.id, second.id],
        ..LayerOrder::default()
    };
    env.recompute(EntityKind::Album, id, Precedence::Plugins, &plugins_first)
        .await;
    let stored = library.get_album(id).await.unwrap().unwrap();
    assert_eq!(stored.title, "Provider A");
    assert_eq!(stored.display_artist.as_deref(), Some("Provider Artist"));
    assert_eq!(stored.release_date.as_deref(), Some("2001-02-03"));
    assert_eq!(stored.album_type.as_deref(), Some("ep"));
    assert_eq!(stored.year, Some(2001));
    assert!(
        env.ids_of(EntityKind::Album, id).await.is_empty(),
        "recomputing the fields never writes identifiers"
    );
    assert_eq!(
        album_genres(&env, id).await,
        vec!["electronic".to_owned()],
        "the provider's genres are stored"
    );

    // The layer order put second first: the same sources answer differently.
    let plugins_second = LayerOrder {
        metadata: vec![second.id, first.id],
        ..LayerOrder::default()
    };
    env.recompute(EntityKind::Album, id, Precedence::Plugins, &plugins_second)
        .await;
    let stored = library.get_album(id).await.unwrap().unwrap();
    assert_eq!(stored.title, "Provider B");
}

/// Stores one metadata release answer for an album.
async fn store_release(
    env: &Env,
    album: hearthsound_core::EntityId,
    instance: uuid::Uuid,
    title: &str,
) {
    let payload = json!({
        "id": format!("release-{instance}"),
        "title": title,
        "artists": [{ "name": "Provider Artist", "ids": [] }],
        "release_date": "2001-02-03",
        "album_type": "ep",
        "genres": ["Electronic"],
        "tracks": [],
    });
    let mut connection = env.database.writer().acquire().await.unwrap();
    sources::put(
        &mut connection,
        EntityKind::Album,
        album.uuid(),
        instance,
        Operation::Release,
        Status::Found,
        Some(&payload.to_string()),
    )
    .await
    .unwrap();
}

/// Lists an album's stored genre names.
async fn album_genres(env: &Env, album: hearthsound_core::EntityId) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT g.name FROM album_genres ag JOIN genres g ON g.id = ag.genre_id \
         WHERE ag.album_id = ? ORDER BY g.name",
    )
    .bind(album.uuid().as_bytes().as_slice())
    .fetch_all(env.database.writer())
    .await
    .unwrap()
}
