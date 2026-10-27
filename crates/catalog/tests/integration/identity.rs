//! Identity: a scanned recording and a provider's track with the same ISRC are
//! one row, so a search answers with the row the scan wrote.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use hearthsound_core::{EntityId, EntityKind, IdPrefix, Layer};
use hearthsound_library::LibraryRepository;
use hearthsound_library::locks::FolderLocks;
use hearthsound_library::scan::LibraryScanner;
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, Env, HTTP_CATALOG_ID};

/// The ISRC both the scanned file and the provider's track carry.
const ISRC: &str = "XSCAN0000001";

/// The MusicBrainz recording id the provider reports, which the scan does not
/// know: the two rows can only be one through the ISRC.
const REMOTE_IDS: &str = "remote-recording-1";

#[tokio::test]
async fn a_provider_track_with_a_scanned_isrc_resolves_to_the_scanned_row() {
    let env = Env::new().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": REMOTE_IDS,
            "title": "Remote Song",
            "artists": ["Remote Artist"],
            "isrc": ISRC,
            "durationMs": 180_000,
        }])))
        .mount(&server)
        .await;
    env.instance(
        "remote",
        hearthsound_test_support::HTTP_CATALOG,
        HTTP_CATALOG_ID,
        common::http_config(&server.uri()),
    )
    .await;
    env.assign(Layer::Catalog, &["remote"]).await;

    let music = env.artwork_dir().parent().unwrap().join("music");
    std::fs::create_dir_all(&music).unwrap();
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("local.flac")
        .tags(
            TrackTags::new("Local Song", "Local Artist")
                .isrc(ISRC)
                .track(1, 1),
        )
        .write_into(&music)
        .await
        .unwrap();

    let library = LibraryRepository::new(&env.database);
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let scanner = LibraryScanner::new(
        &env.database,
        Arc::clone(&env.events),
        env.settings.clone(),
        Arc::new(FolderLocks::new()),
    );
    scanner.scan_folder(folder.id).await.unwrap();

    let scanned = scanned_track(&env).await;
    assert_eq!(env.count_rows("tracks").await, 1, "the scan wrote one row");
    let scanned_ids = env.ids_of(EntityKind::Track, scanned).await;
    assert!(
        scanned_ids
            .iter()
            .any(|id| id.namespace == "isrc" && id.value == ISRC),
        "the scan stored the file's ISRC, got {scanned_ids:?}"
    );

    let user = env.user().await;
    let service = env.service();
    let answer = service
        .search(user, "Remote Song", &[EntityKind::Track], 10, 0)
        .await
        .unwrap();

    assert_eq!(
        answer.tracks.len(),
        1,
        "the provider hit resolves to a row and is returned once"
    );
    assert_eq!(
        answer.tracks[0].track.id, scanned,
        "the hit is the scanned row, matched by its ISRC"
    );
    assert_eq!(
        env.count_rows("tracks").await,
        1,
        "resolving the hit did not mint a second row"
    );
    let isrc = env.ids_of(EntityKind::Track, scanned).await;
    assert!(
        isrc.iter()
            .any(|id| id.namespace == "isrc" && id.value == ISRC),
        "the scanned row still carries its ISRC"
    );
}

/// Returns the only stored track.
async fn scanned_track(env: &Env) -> EntityId {
    let bytes: Vec<u8> = sqlx::query_scalar("SELECT id FROM tracks LIMIT 1")
        .fetch_one(env.database.writer())
        .await
        .unwrap();
    EntityId::from_uuid(IdPrefix::Track, uuid::Uuid::from_slice(&bytes).unwrap())
}

/// A credit that names two MusicBrainz artists resolves to the first one only.
///
/// ListenBrainz reports the recording `Buffalo` with the credit
/// `alt-J feat. Mountain Man` and both artists' MusicBrainz ids. Attaching every
/// id of a credit gave one row two artist ids, so the row's biography and
/// portrait came from whichever was read first - the alt-J page showed Mountain
/// Man's - and neither id's albums were complete. One credit names one artist:
/// the first id names the row and the rest are dropped.
#[tokio::test]
async fn a_credit_with_two_musicbrainz_ids_resolves_to_its_first_artist_only() {
    let env = Env::new().await;
    let credit = hearthsound_core::providers::ArtistCredit {
        name: "alt-J feat. Mountain Man".to_owned(),
        ids: vec![
            hearthsound_core::providers::ExternalId {
                namespace: "mb-artist".to_owned(),
                value: ALT_J.to_owned(),
            },
            hearthsound_core::providers::ExternalId {
                namespace: "mb-artist".to_owned(),
                value: MOUNTAIN_MAN.to_owned(),
            },
        ],
    };
    let mut connection = env.database.writer().acquire().await.unwrap();
    let row = env
        .service()
        .resolver()
        .credit(&mut connection, &credit, None)
        .await
        .unwrap();
    drop(connection);

    let ids = env
        .ids_of(
            EntityKind::Artist,
            EntityId::from_uuid(IdPrefix::Artist, row),
        )
        .await;
    let held: Vec<&str> = ids
        .iter()
        .filter(|id| id.namespace == "mb-artist")
        .map(|id| id.value.as_str())
        .collect();
    assert_eq!(
        held,
        [ALT_J],
        "the row holds the first id of the credit and nothing else, got {ids:?}"
    );
    let stray: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM external_ids WHERE namespace = 'mb-artist' \
                            AND value = ?",
    )
    .bind(MOUNTAIN_MAN)
    .fetch_one(env.database.writer())
    .await
    .unwrap();
    assert_eq!(stray, 0, "the second id belongs to no row");
}

/// The MusicBrainz id of alt-J.
const ALT_J: &str = "fc7bbf00-fbaa-4736-986b-b3ac0266ca9b";

/// The MusicBrainz id of Mountain Man, which one ListenBrainz credit pairs with
/// alt-J's.
const MOUNTAIN_MAN: &str = "ecc6e248-9ec4-4150-bbd2-d2b112f0b365";
