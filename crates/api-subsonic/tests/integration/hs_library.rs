//! Integration tests for the personal-library vendor endpoints.
//!
//! `hsSave` and `hsUnsave` write and remove the accounts' own library
//! membership, and `hsSetFeedback` records a like exactly as `star` does, a
//! dislike in place of a like, or neither. These tests cover the pins the three
//! leave behind, the adoption a save and a like perform, the catalog ids they
//! materialize, and the errors of the vendor gate and of the parameters.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use hearthsound_activity::{disliked_ids, load_annotations};
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix, Slug};
use hearthsound_library::LibraryRepository;
use hearthsound_library::query::{personal_album_ids, personal_artist_ids, personal_track_ids};

use crate::common::{Fixture, assert_error, assert_ok, get};

/// Builds the `x_` id of one hello-catalog entity.
fn catalog_id(kind: EntityKind, provider_id: &str) -> String {
    CatalogId::new(Slug::new("hello").unwrap(), kind, provider_id)
        .unwrap()
        .to_string()
}

/// Builds a fixture with one remote album of `tracks` tracks.
async fn fixture(tracks: usize) -> (Fixture, EntityId, Vec<EntityId>) {
    let fixture = Fixture::new().await;
    let (album, tracks) = fixture.remote_album("Faraway", tracks).await;
    (fixture, album, tracks)
}

/// Returns whether a track carries a pin.
async fn is_pinned(fixture: &Fixture, track: EntityId) -> bool {
    LibraryRepository::new(&fixture.database)
        .track_is_pinned(track)
        .await
        .unwrap()
}

/// Returns whether one account holds a like for one entity.
async fn is_liked(fixture: &Fixture, kind: EntityKind, entity: EntityId) -> bool {
    let mut conn = fixture.database.readers().acquire().await.unwrap();
    load_annotations(&mut conn, fixture.user_id.uuid(), kind, &[entity.uuid()])
        .await
        .unwrap()
        .get(&entity.uuid())
        .is_some_and(|annotation| annotation.starred_at.is_some())
}

/// Returns whether one account holds a dislike for one entity.
async fn is_disliked(fixture: &Fixture, kind: EntityKind, entity: EntityId) -> bool {
    let mut conn = fixture.database.readers().acquire().await.unwrap();
    disliked_ids(&mut conn, fixture.user_id.uuid(), kind, &[entity.uuid()])
        .await
        .unwrap()
        == HashSet::from([entity.uuid()])
}

/// Returns the first value that follows a marker in a response body.
fn id_after(body: &str, marker: &str) -> Option<String> {
    let start = body.find(marker)?;
    let rest = &body[start + marker.len()..];
    let end = rest.find('"').unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

/// Returns the stored entity a client id names.
fn entity_of(raw: &str) -> EntityId {
    match ClientId::parse(raw).unwrap() {
        ClientId::Entity(id) => id,
        ClientId::Catalog(id) => panic!("expected a stored id, got {id}"),
    }
}

/// Returns whether an entity is in the account's personal library.
async fn in_personal_library(fixture: &Fixture, kind: EntityKind, entity: EntityId) -> bool {
    let mut conn = fixture.database.readers().acquire().await.unwrap();
    let user = fixture.user_id.uuid();
    let ids = [entity];
    let members = match kind {
        EntityKind::Track => personal_track_ids(&mut conn, user, &ids).await,
        EntityKind::Album => personal_album_ids(&mut conn, user, &ids).await,
        EntityKind::Artist => personal_artist_ids(&mut conn, user, &ids).await,
        EntityKind::Playlist => return false,
    };
    members.unwrap().contains(&entity)
}

#[tokio::test]
async fn saving_a_track_pins_it_and_answers_an_empty_response() {
    let (fixture, _, tracks) = fixture(2).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSave?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("hs-save-json", &probe.body);
    assert!(in_personal_library(&fixture, EntityKind::Track, tracks[0]).await);
    assert!(is_pinned(&fixture, tracks[0]).await);
    assert!(
        !is_pinned(&fixture, tracks[1]).await,
        "only the saved track is pinned"
    );
}

#[tokio::test]
async fn saving_answers_the_xml_encoding_as_well() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSave?apiKey={}&hearthsound=1&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("hs-save-xml", &probe.body);
}

#[tokio::test]
async fn saving_an_album_pins_its_member_tracks() {
    let (fixture, album, tracks) = fixture(2).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSave?apiKey={}&hearthsound=1&f=json&id={album}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    for track in &tracks {
        assert!(
            is_pinned(&fixture, *track).await,
            "the album save pinned its member tracks"
        );
    }

    let probe = get(
        &app,
        &format!(
            "/rest/hsUnsave?apiKey={}&hearthsound=1&f=json&id={album}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("hs-unsave-json", &probe.body);
    for track in &tracks {
        assert!(
            !is_pinned(&fixture, *track).await,
            "the unsave released the member tracks"
        );
    }
    assert!(!in_personal_library(&fixture, EntityKind::Album, album).await);
}

#[tokio::test]
async fn unsaving_answers_the_xml_encoding_as_well() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();

    get(
        &app,
        &format!(
            "/rest/hsSave?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    let probe = get(
        &app,
        &format!(
            "/rest/hsUnsave?apiKey={}&hearthsound=1&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("hs-unsave-xml", &probe.body);
}

#[tokio::test]
async fn feedback_like_behaves_like_a_star() {
    let (fixture, _, tracks) = fixture(2).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={}&value=like",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("hs-set-feedback-json", &probe.body);
    assert!(is_liked(&fixture, EntityKind::Track, tracks[0]).await);
    assert!(is_pinned(&fixture, tracks[0]).await);

    // The same request through `star` leaves the same rows and pins behind.
    let probe = get(
        &app,
        &format!(
            "/rest/star?apiKey={}&f=json&id={}",
            fixture.api_key, tracks[1]
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(is_liked(&fixture, EntityKind::Track, tracks[1]).await);
    assert!(is_pinned(&fixture, tracks[1]).await);
}

#[tokio::test]
async fn feedback_answers_the_xml_encoding_as_well() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&id={}&value=none",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("hs-set-feedback-xml", &probe.body);
}

#[tokio::test]
async fn a_dislike_replaces_a_like_and_unpins_the_track() {
    let (fixture, _, tracks) = fixture(2).await;
    let app = fixture.app();

    get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={}&value=like",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert!(is_pinned(&fixture, tracks[0]).await);

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={}&value=dislike",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(is_disliked(&fixture, EntityKind::Track, tracks[0]).await);
    assert!(
        !is_pinned(&fixture, tracks[0]).await,
        "a dislike pins nothing"
    );
}

#[tokio::test]
async fn clearing_feedback_removes_the_row_and_the_pin() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();

    get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={}&value=like",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={}&value=none",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(!is_liked(&fixture, EntityKind::Track, tracks[0]).await);
    assert!(!is_disliked(&fixture, EntityKind::Track, tracks[0]).await);
    assert!(!is_pinned(&fixture, tracks[0]).await);
}

#[tokio::test]
async fn saving_and_liking_a_catalog_track_materializes_and_adopts_it() {
    let mut fixture = Fixture::new().await;
    fixture.install_hello_catalog().await;
    fixture.add_acquisition_layer().await;
    let app = fixture.app();
    let id = catalog_id(EntityKind::Track, "hello-track-1");

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={id}&value=like",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);

    // A like materializes the catalog track and adopts it, and the acquisition
    // ledger names the stored track the adoption targeted.
    let listed = get(
        &app,
        &format!(
            "/rest/hsGetAcquisitions?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    let stored = entity_of(&id_after(&listed.body, "\"targetId\":\"").unwrap());
    assert_eq!(stored.kind(), IdPrefix::Track);
    assert!(
        is_pinned(&fixture, stored).await,
        "the like pinned the materialized track"
    );
    assert!(is_liked(&fixture, EntityKind::Track, stored).await);
    assert!(in_personal_library(&fixture, EntityKind::Track, stored).await);

    let saved = catalog_id(EntityKind::Album, "hello-album-1");
    let probe = get(
        &app,
        &format!(
            "/rest/hsSave?apiKey={}&hearthsound=1&f=json&id={saved}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    let album = get(
        &app,
        &format!(
            "/rest/getAlbum?id={saved}&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&album);
    assert!(
        album.body.contains("\"inLibrary\":true"),
        "the saved catalog album is in the personal library: {}",
        album.body
    );
}

#[tokio::test]
async fn a_library_write_without_an_id_is_refused() {
    let (fixture, _, _) = fixture(1).await;
    let app = fixture.app();

    for endpoint in ["hsSave", "hsUnsave"] {
        let probe = get(
            &app,
            &format!(
                "/rest/{endpoint}?apiKey={}&hearthsound=1&f=json",
                fixture.api_key
            ),
        )
        .await;
        assert_error(&probe, 10);
    }

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&value=like",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 10);
}

#[tokio::test]
async fn a_feedback_request_without_a_value_is_refused() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_error(&probe, 10);
}

#[tokio::test]
async fn a_feedback_value_outside_the_three_is_refused() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={}&value=maybe",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_error(&probe, 0);
    assert!(!is_liked(&fixture, EntityKind::Track, tracks[0]).await);
    assert!(!is_disliked(&fixture, EntityKind::Track, tracks[0]).await);
}

#[tokio::test]
async fn a_library_write_naming_an_unknown_id_is_refused() {
    let (fixture, _, _) = fixture(1).await;
    let app = fixture.app();
    let unknown = EntityId::new(hearthsound_core::IdPrefix::Track);

    for endpoint in ["hsSave", "hsUnsave"] {
        let probe = get(
            &app,
            &format!(
                "/rest/{endpoint}?apiKey={}&hearthsound=1&f=json&id={unknown}",
                fixture.api_key
            ),
        )
        .await;
        assert_error(&probe, 70);
    }

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={unknown}&value=like",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);

    // A playlist id is not an entity that carries feedback.
    let playlist = EntityId::new(hearthsound_core::IdPrefix::Playlist);
    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={playlist}&value=like",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn a_library_write_answers_error_70_without_the_vendor_flag() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();

    for endpoint in ["hsSave", "hsUnsave", "hsSetFeedback"] {
        let probe = get(
            &app,
            &format!(
                "/rest/{endpoint}?apiKey={}&f=json&id={}&value=like",
                fixture.api_key, tracks[0]
            ),
        )
        .await;
        assert_error(&probe, 70);
    }
    assert!(!in_personal_library(&fixture, EntityKind::Track, tracks[0]).await);
}
