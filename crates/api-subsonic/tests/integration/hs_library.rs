//! Integration tests for the personal-library vendor endpoints.
//!
//! `hsSave` and `hsUnsave` write and remove the accounts' own library
//! membership, and `hsSetFeedback` records a like exactly as `star` does, a
//! dislike in place of a like, or neither. These tests cover the pins the three
//! leave behind, the adoption a save and a like perform, the catalog ids they
//! target, and the errors of the vendor gate and of the parameters.
//!
//! `hsGetLibraryCounts` reports the size of each list, which is what a client
//! sizes its placeholders with: the counts are compared with the listings they
//! stand for, and both encodings are snapshotted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use hearthsound_activity::{disliked_ids, load_annotations};
use hearthsound_core::{EntityId, EntityKind, IdPrefix};
use hearthsound_library::LibraryRepository;
use hearthsound_library::query::{personal_album_ids, personal_artist_ids, personal_track_ids};

use crate::common::seed::library;
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// The filters a snapshot of a body naming entity ids needs.
///
/// The ids of a seeded library carry a time-based payload, so a snapshot that
/// names one has to mask it; the timestamps are masked by the shared filters.
const ID_FILTERS: [(&str, &str); 2] = [SNAPSHOT_FILTERS[0], (r"(?:tr|al|ar|pl)_[0-9a-z]+", "ID")];

/// Reads the `libraryCounts` members out of a JSON body.
///
/// # Parameters
///
/// - `body`: the response body.
///
/// # Returns
///
/// The counts, in the order the endpoint emits them.
///
/// # Panics
///
/// Panics when the body carries no `libraryCounts`, which is a test's own
/// arrangement mistake.
fn library_counts(body: &str) -> Vec<(String, i64)> {
    let marker = "\"libraryCounts\":{";
    let start = body.find(marker).expect("the response carries the counts");
    let rest = &body[start + marker.len()..];
    let end = rest.find('}').expect("the counts object closes");
    rest[..end]
        .split(',')
        .map(|member| {
            let (name, value) = member.split_once(':').expect("a member has a value");
            (
                name.trim_matches('"').to_owned(),
                value.trim().parse().expect("a count is a number"),
            )
        })
        .collect()
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
async fn saving_a_track_pins_it_and_answers_its_result() {
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
    insta::with_settings!({filters => ID_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-save-json", &probe.body);
    });
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
    insta::with_settings!({filters => ID_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-save-xml", &probe.body);
    });
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
    insta::with_settings!({filters => ID_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-unsave-json", &probe.body);
    });
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
    insta::with_settings!({filters => ID_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-unsave-xml", &probe.body);
    });
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
    insta::with_settings!({filters => ID_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-set-feedback-json", &probe.body);
    });
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
    insta::with_settings!({filters => ID_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-set-feedback-xml", &probe.body);
    });
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

/// A library write applies each id on its own and reports the ones it could
/// not apply, so a bulk action tells the truth about what it did.
#[tokio::test]
async fn a_library_write_reports_the_ids_it_could_not_apply() {
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
        assert_ok(&probe);
        assert!(
            probe
                .body
                .contains("\"bulkResult\":{\"applied\":[],\"failed\":[{"),
            "{endpoint} did not report the refused id: {}",
            probe.body
        );
        assert!(
            probe.body.contains(&format!("\"id\":\"{unknown}\""))
                && probe.body.contains("\"reason\":\"not found\""),
            "{endpoint} named no reason: {}",
            probe.body
        );
    }

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={unknown}&value=like",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains("\"reason\":\"not found\""),
        "{}",
        probe.body
    );

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
    assert_ok(&probe);
    assert!(
        probe.body.contains(&format!("\"id\":\"{playlist}\"")),
        "{}",
        probe.body
    );
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

/// Creates a playlist of `tracks` owned by the fixture's account.
async fn playlist(fixture: &Fixture, name: &str, tracks: &[EntityId]) -> EntityId {
    fixture
        .state
        .activity()
        .playlists()
        .create(fixture.user_id.uuid(), name, None, false, tracks)
        .await
        .unwrap()
        .id
}

/// Converting a playlist to temporary releases its pins and converting it back
/// to permanent restores them.
#[tokio::test]
async fn converting_a_playlist_pins_and_releases_its_tracks() {
    let (fixture, _, tracks) = fixture(2).await;
    let app = fixture.app();
    let id = playlist(&fixture, "Synced", &tracks).await;
    for track in &tracks {
        assert!(is_pinned(&fixture, *track).await);
    }

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={}&hearthsound=1&f=json&playlistId={id}&temporary=true&expiresAt=4000",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("hs-playlist-lifetime-json", &probe.body);
    for track in &tracks {
        assert!(
            !is_pinned(&fixture, *track).await,
            "a temporary playlist pins nothing"
        );
    }
    let listed = fixture
        .state
        .activity()
        .playlists()
        .get(id, fixture.user_id.uuid())
        .await
        .unwrap()
        .unwrap();
    assert!(listed.is_temporary);
    assert_eq!(listed.expires_at, Some(4_000));

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={}&hearthsound=1&f=json&playlistId={id}&temporary=false",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    for track in &tracks {
        assert!(is_pinned(&fixture, *track).await);
    }
}

/// The conversion answers the XML encoding as well.
#[tokio::test]
async fn converting_a_playlist_answers_the_xml_encoding_as_well() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();
    let id = playlist(&fixture, "Synced", &tracks).await;

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={}&hearthsound=1&playlistId={id}&temporary=true",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("hs-playlist-lifetime-xml", &probe.body);
}

/// Only the playlist's owner may convert it.
#[tokio::test]
async fn converting_another_accounts_playlist_is_not_authorized() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();
    let id = playlist(&fixture, "Mine", &tracks).await;
    let (_, other_key) = fixture.account(&[]).await;

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={}&hearthsound=1&f=json&playlistId={id}&temporary=true",
            other_key
        ),
    )
    .await;
    assert_error(&probe, 70);
    assert!(
        is_pinned(&fixture, tracks[0]).await,
        "the refused conversion changed nothing"
    );
}

/// An unknown or malformed playlist id is refused as not found.
#[tokio::test]
async fn a_lifetime_request_naming_an_unknown_playlist_is_refused() {
    let (fixture, _, _) = fixture(1).await;
    let app = fixture.app();
    let unknown = EntityId::new(IdPrefix::Playlist);

    for id in [unknown.to_string(), "nonsense".to_owned()] {
        let probe = get(
            &app,
            &format!(
                "/rest/hsSetPlaylistLifetime?apiKey={}&hearthsound=1&f=json&playlistId={id}&temporary=true",
                fixture.api_key
            ),
        )
        .await;
        assert_error(&probe, 70);
    }
}

/// The playlist and the temporary flag are required.
#[tokio::test]
async fn a_lifetime_request_without_its_parameters_is_refused() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();
    let id = playlist(&fixture, "Mine", &tracks).await;

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={}&hearthsound=1&f=json&temporary=true",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 10);

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={}&hearthsound=1&f=json&playlistId={id}",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 10);
}

/// A flag that is not a boolean and an expiry that is not a time are refused.
#[tokio::test]
async fn a_malformed_lifetime_value_is_refused() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();
    let id = playlist(&fixture, "Mine", &tracks).await;

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={}&hearthsound=1&f=json&playlistId={id}&temporary=maybe",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 0);

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={}&hearthsound=1&f=json&playlistId={id}&temporary=true&expiresAt=soon",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 0);
    assert!(
        is_pinned(&fixture, tracks[0]).await,
        "a refused request changed nothing"
    );
}

/// The endpoint is answered only for a request that opted into the extension.
#[tokio::test]
async fn a_lifetime_change_answers_error_70_without_the_vendor_flag() {
    let (fixture, _, tracks) = fixture(1).await;
    let app = fixture.app();
    let id = playlist(&fixture, "Mine", &tracks).await;

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={}&f=json&playlistId={id}&temporary=true",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);
    assert!(is_pinned(&fixture, tracks[0]).await);
}

/// The counts match the listings they stand for, and both encodings carry them.
#[tokio::test]
async fn the_counts_match_the_listings_they_size() {
    let fixture = Fixture::new().await;
    let seeded = library(
        &fixture.database,
        &fixture.subdir("owned"),
        &fixture.subdir("generic"),
        fixture.user_id,
    )
    .await;
    let app = fixture.app();
    let key = fixture.api_key.clone();
    let account = fixture.user_id.uuid();

    // A like on one of each kind and one playlist, so the counts that are the
    // account's own are not all zero.
    get(
        &app,
        &format!(
            "/rest/star?id={}&albumId={}&artistId={}&apiKey={key}&f=json",
            seeded.alpha_tracks[0], seeded.alpha, seeded.cardinals
        ),
    )
    .await;
    get(
        &app,
        &format!(
            "/rest/createPlaylist?name=Evening&songId={}&apiKey={key}&f=json",
            seeded.alpha_tracks[0]
        ),
    )
    .await;

    let json = get(
        &app,
        &format!("/rest/hsGetLibraryCounts?musicFolderId=2&hearthsound=1&apiKey={key}&f=json"),
    )
    .await;
    assert_ok(&json);
    let counts = library_counts(&json.body);
    assert_eq!(
        counts
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "artists",
            "albums",
            "songs",
            "playlists",
            "likedArtists",
            "likedAlbums",
            "likedSongs",
            "uploads"
        ],
        "the members are emitted in a fixed order"
    );
    // The server view holds every stored track, so the cache album and its
    // artist are inside it along with the other four albums.
    let counted = |name: &str| {
        counts
            .iter()
            .find(|(member, _)| member == name)
            .map(|(_, value)| *value)
            .unwrap()
    };
    assert_eq!(counted("albums"), 4, "{}", json.body);
    assert_eq!(counted("songs"), 5, "{}", json.body);
    assert_eq!(counted("artists"), 4, "{}", json.body);
    assert_eq!(counted("playlists"), 1);
    assert_eq!(counted("likedArtists"), 1);
    assert_eq!(counted("likedAlbums"), 1);
    assert_eq!(counted("likedSongs"), 1);
    assert_eq!(counted("uploads"), 0);

    let mut conn = fixture.database.readers().acquire().await.unwrap();
    assert_eq!(
        counted("songs"),
        hearthsound_library::query::count_tracks(
            &mut conn,
            hearthsound_library::query::Scope::Server(
                hearthsound_library::query::ServerBrowse::All
            )
        )
        .await
        .unwrap(),
        "the count is the one the songs listing returns"
    );
    assert_eq!(
        hearthsound_library::query::starred_counts(&mut conn, account)
            .await
            .unwrap()
            .tracks,
        counted("likedSongs")
    );
    drop(conn);

    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-library-counts-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!("/rest/hsGetLibraryCounts?musicFolderId=2&hearthsound=1&apiKey={key}"),
    )
    .await;
    assert_ok(&xml);
    assert!(
        xml.body.contains("<libraryCounts"),
        "the XML carries the counts: {}",
        xml.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-library-counts-xml", seeded.stabilize(&xml.body));
    });

    // Without the vendor flag the endpoint is not there at all.
    let standard = get(
        &app,
        &format!("/rest/hsGetLibraryCounts?musicFolderId=2&apiKey={key}&f=json"),
    )
    .await;
    assert_error(&standard, 70);
}

/// A bulk like answers the ids it applied and the ones it did not, and applies
/// the good ones.
#[tokio::test]
async fn a_bulk_like_answers_which_ids_it_applied() {
    let (fixture, album, tracks) = fixture(2).await;
    let app = fixture.app();
    let gone = EntityId::new(IdPrefix::Track);

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetFeedback?apiKey={}&hearthsound=1&f=json&id={}&id={gone}&id={}&value=like",
            fixture.api_key, tracks[0], album
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe
            .body
            .contains(&format!("\"applied\":[\"{}\",\"{album}\"]", tracks[0])),
        "the applied ids are named in request order: {}",
        probe.body
    );
    assert!(
        probe.body.contains(&format!(
            "\"failed\":[{{\"id\":\"{gone}\",\"reason\":\"not found\"}}]"
        )),
        "the refused id and its reason are named: {}",
        probe.body
    );

    assert!(is_liked(&fixture, EntityKind::Track, tracks[0]).await);
    assert!(is_liked(&fixture, EntityKind::Album, album).await);
    assert!(
        !is_liked(&fixture, EntityKind::Track, gone).await,
        "no like is stored for an id that does not resolve"
    );
}

/// A bulk save and a bulk unsave answer the same shape.
#[tokio::test]
async fn a_bulk_save_and_unsave_answer_the_same_shape() {
    let (fixture, album, tracks) = fixture(2).await;
    let app = fixture.app();
    let gone = EntityId::new(IdPrefix::Track);

    let saved = get(
        &app,
        &format!(
            "/rest/hsSave?apiKey={}&hearthsound=1&f=json&id={}&id={gone}&id={album}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&saved);
    assert!(
        saved
            .body
            .contains(&format!("\"applied\":[\"{}\",\"{album}\"]", tracks[0])),
        "{}",
        saved.body
    );
    assert!(
        saved.body.contains(&format!(
            "\"failed\":[{{\"id\":\"{gone}\",\"reason\":\"not found\"}}]"
        )),
        "{}",
        saved.body
    );
    assert!(in_personal_library(&fixture, EntityKind::Track, tracks[0]).await);
    assert!(in_personal_library(&fixture, EntityKind::Album, album).await);

    let unsaved = get(
        &app,
        &format!(
            "/rest/hsUnsave?apiKey={}&hearthsound=1&f=json&id={}&id={gone}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&unsaved);
    assert!(
        unsaved
            .body
            .contains(&format!("\"applied\":[\"{}\"]", tracks[0])),
        "{}",
        unsaved.body
    );
    assert!(
        unsaved.body.contains(&format!(
            "\"failed\":[{{\"id\":\"{gone}\",\"reason\":\"not found\"}}]"
        )),
        "{}",
        unsaved.body
    );
    // The album is still saved and its tracks follow it into the library, so
    // the track is only out of the library once the album is unsaved too.
    assert!(
        in_personal_library(&fixture, EntityKind::Track, tracks[0]).await,
        "a saved album keeps its member tracks in the library"
    );
    let album_unsaved = get(
        &app,
        &format!(
            "/rest/hsUnsave?apiKey={}&hearthsound=1&f=json&id={album}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&album_unsaved);
    assert!(!in_personal_library(&fixture, EntityKind::Track, tracks[0]).await);
    assert!(!in_personal_library(&fixture, EntityKind::Album, album).await);
}

/// A bulk write that names no id is still a malformed request.
#[tokio::test]
async fn a_bulk_write_without_ids_is_error_10() {
    let (fixture, _, _) = fixture(1).await;
    let app = fixture.app();

    for endpoint in ["hsSave", "hsUnsave", "hsSetFeedback"] {
        let probe = get(
            &app,
            &format!(
                "/rest/{endpoint}?apiKey={}&hearthsound=1&f=json&value=like",
                fixture.api_key
            ),
        )
        .await;
        assert_error(&probe, 10);
    }
}

/// Starring a track that has no row answers not found.
#[tokio::test]
async fn starring_an_unknown_track_is_not_found() {
    let (fixture, _, _) = fixture(1).await;
    let app = fixture.app();
    let missing = EntityId::new(IdPrefix::Track);

    let probe = get(
        &app,
        &format!("/rest/star?id={missing}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&probe, 70);
    assert!(
        !is_liked(&fixture, EntityKind::Track, missing).await,
        "no like is stored for a track that does not exist"
    );
}
