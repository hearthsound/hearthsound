//! Tests for the library repository: local entity dedupe, credit and genre
//! writes, the preferred-file rule, and library folder management.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_core::providers::{ExternalId, MB_ARTIST, MB_RELEASE};
use hearthsound_core::{Availability, EntityKind, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::{
    AlbumInput, ArtistCredit, ArtistInput, LibraryRepository, ManagedMediaFile, MediaFileInput,
    ScannedMediaFile, TrackInput, sort_key,
};

/// Builds an artist input carrying a MusicBrainz artist id.
fn mbid_artist(name: &str, mbid: &str) -> ArtistInput {
    ArtistInput {
        ids: vec![ExternalId {
            namespace: MB_ARTIST.to_owned(),
            value: mbid.to_owned(),
        }],
        ..ArtistInput::new(name)
    }
}

/// The identifier a MusicBrainz release id writes.
fn release_id(mbid: &str) -> ExternalId {
    ExternalId {
        namespace: MB_RELEASE.to_owned(),
        value: mbid.to_owned(),
    }
}

/// Returns an artist row's identifiers, in namespace order.
async fn artist_ids(db: &Database, artist: uuid::Uuid) -> Vec<ExternalId> {
    let mut conn = db.readers().acquire().await.unwrap();
    hearthsound_library::external_ids::ids_of(&mut conn, EntityKind::Artist, artist)
        .await
        .unwrap()
}

/// Returns an album row's identifiers, in namespace order.
async fn album_ids(db: &Database, album: uuid::Uuid) -> Vec<ExternalId> {
    let mut conn = db.readers().acquire().await.unwrap();
    hearthsound_library::external_ids::ids_of(&mut conn, EntityKind::Album, album)
        .await
        .unwrap()
}

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Inserts a bare user row and returns its id.
async fn insert_user(db: &Database) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, 'x', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("user-{id}"))
    .bind("User")
    .execute(db.writer())
    .await
    .unwrap();
    id
}

fn track_input(title: &str) -> TrackInput {
    TrackInput::new(title, Availability::Local)
}

#[tokio::test]
async fn artists_dedupe_by_mbid_then_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let with_mbid = mbid_artist("Energy 52", "mbid-energy-52");
    let first = library.upsert_artist(&with_mbid).await.unwrap();
    let again = library.upsert_artist(&with_mbid).await.unwrap();
    assert_eq!(first, again, "the same MBID must not insert twice");

    // A differently spelled name with the same MBID still merges.
    let respelled = mbid_artist("Energy52", "mbid-energy-52");
    assert_eq!(library.upsert_artist(&respelled).await.unwrap(), first);

    // Without an MBID the name is compared case-insensitively.
    let plain = library
        .upsert_artist(&ArtistInput::new("Cafe del Mar"))
        .await
        .unwrap();
    let same = library
        .upsert_artist(&ArtistInput::new("cafe DEL mar"))
        .await
        .unwrap();
    assert_eq!(plain, same);

    assert_eq!(library.list_artists().await.unwrap().len(), 2);

    // Two artists that differ only by MBID stay distinct.
    let other = library
        .upsert_artist(&mbid_artist("Energy 52", "mbid-other"))
        .await
        .unwrap();
    assert_ne!(other, first);
    assert_eq!(library.list_artists().await.unwrap().len(), 3);
}

/// Files of one artist tagged with and without a MusicBrainz id credit the
/// same artist, whichever kind is stored first.
#[tokio::test]
async fn an_artist_tagged_with_and_without_an_mbid_is_one_artist() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    // The untagged file first: the tagged one adopts its row and its id.
    let plain = library
        .upsert_artist(&ArtistInput::new("Aphex Twin"))
        .await
        .unwrap();
    let tagged = library
        .upsert_artist(&mbid_artist("Aphex Twin", "mbid-aphex"))
        .await
        .unwrap();
    assert_eq!(tagged, plain);
    assert_eq!(
        artist_ids(&db, tagged.uuid()).await,
        [ExternalId {
            namespace: MB_ARTIST.to_owned(),
            value: "mbid-aphex".to_owned(),
        }],
        "the row adopted the id"
    );

    // The tagged file first: the untagged one joins it.
    let tagged = library
        .upsert_artist(&mbid_artist("Blue Bendy", "mbid-bendy"))
        .await
        .unwrap();
    let plain = library
        .upsert_artist(&ArtistInput::new("blue bendy"))
        .await
        .unwrap();
    assert_eq!(plain, tagged);

    // A second id under a name already identified is a different artist, and
    // an untagged file still credits an identified one rather than a new row.
    let namesake = library
        .upsert_artist(&mbid_artist("Blue Bendy", "mbid-namesake"))
        .await
        .unwrap();
    assert_ne!(namesake, tagged);
    assert_eq!(library.list_artists().await.unwrap().len(), 3);
}

#[tokio::test]
async fn albums_dedupe_by_release_id_then_by_title_and_primary_artist() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let credits = [ArtistCredit::new("Energy 52", "main")];
    let mut album = AlbumInput::new("Cafe del Mar");
    album.ids.push(release_id("mbid-release"));
    album.year = Some(1993);
    let first = library.upsert_album(&album, &credits).await.unwrap();
    assert_eq!(library.upsert_album(&album, &credits).await.unwrap(), first);

    // The same title, primary artist and year, without a release id, merges.
    let mut plain = AlbumInput::new("Cafe del Mar");
    plain.year = Some(1993);
    assert_eq!(library.upsert_album(&plain, &credits).await.unwrap(), first);

    // A different year is a different album.
    let mut later = AlbumInput::new("Cafe del Mar");
    later.year = Some(2010);
    let later = library.upsert_album(&later, &credits).await.unwrap();
    assert_ne!(later, first);

    // A different title by another artist is a different album, and so is one
    // whose year the tags do not carry.
    let other_credits = [ArtistCredit::new("Someone Else", "main")];
    let other = library.upsert_album(&plain, &other_credits).await.unwrap();
    assert_ne!(other, first);
    let unyearly = library
        .upsert_album(&AlbumInput::new("Cafe del Mar"), &credits)
        .await
        .unwrap();
    assert_ne!(unyearly, first);
    assert_ne!(unyearly, later);

    let stored = library.get_album(first).await.unwrap().unwrap();
    assert_eq!(stored.year, Some(1993));
    assert_eq!(
        album_ids(&db, first.uuid()).await,
        [release_id("mbid-release")]
    );
    let artists = library.list_album_artists(first).await.unwrap();
    assert_eq!(artists.len(), 1);
    assert_eq!(artists[0].name, "Energy 52");
}

#[tokio::test]
async fn tracks_store_credits_and_case_folded_genres() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let credits = [
        ArtistCredit::new("Energy 52", "main"),
        ArtistCredit::new("Guest Voice", "featured"),
    ];
    let genres = [
        "Trance".to_owned(),
        "trance".to_owned(),
        "Ambient".to_owned(),
    ];
    let track = library
        .insert_track(&track_input("Cafe del Mar"), &credits, &genres)
        .await
        .unwrap();

    let stored = library.get_track(track).await.unwrap().unwrap();
    assert_eq!(stored.title, "Cafe del Mar");
    assert_eq!(stored.sort_title, "cafe del mar");
    assert_eq!(stored.availability, Availability::Local);

    // An input that names no identifier writes none: the columns that used to
    // hold them are gone, and identity lives in `external_ids` alone.
    let ids = hearthsound_library::external_ids::ids_of(
        &mut db.writer().acquire().await.unwrap(),
        EntityKind::Track,
        track.uuid(),
    )
    .await
    .unwrap();
    assert!(ids.is_empty(), "{ids:?}");

    let artists = library.list_track_artists(track).await.unwrap();
    assert_eq!(
        artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        ["Energy 52", "Guest Voice"]
    );
    let credits = library.list_track_credits(track).await.unwrap();
    assert_eq!(credits[0].role, "main");
    assert_eq!(credits[1].role, "featured");
    assert_eq!(credits[1].position, 1);

    let genres = library.list_track_genres(track).await.unwrap();
    assert_eq!(
        genres.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
        ["ambient", "trance"],
        "case variants must fold into one row"
    );

    // Rewriting replaces the credits and genres rather than appending.
    let mut updated = track_input("Sunset");
    updated.track_number = Some(2);
    let replaced = [ArtistCredit::new("Someone Else", "main")];
    assert!(
        library
            .update_track(track, &updated, &replaced, &["house".to_owned()])
            .await
            .unwrap()
    );
    assert_eq!(
        library.get_track(track).await.unwrap().unwrap().title,
        "Sunset"
    );
    assert_eq!(library.list_track_artists(track).await.unwrap().len(), 1);
    assert_eq!(
        library.list_track_genres(track).await.unwrap()[0].name,
        "house"
    );
    assert_eq!(library.list_artists().await.unwrap().len(), 3);
}

#[tokio::test]
async fn the_preferred_file_is_the_permanent_managed_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let track = library
        .insert_track(&track_input("Cafe del Mar"), &[], &[])
        .await
        .unwrap();

    // A cache copy is preferred while it is the only file.
    let cache = library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Cache,
            rel_path: "cache/track.mp3".to_owned(),
            size: 1,
            mtime_ms: 1,
            format: Some("mpeg".to_owned()),
            codec: Some("mp3".to_owned()),
            bitrate_kbps: Some(320),
            sample_rate: Some(44_100),
            bit_depth: None,
            channels: Some(2),
            duration_ms: Some(1_000),
            lossless: Some(false),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: Some(false),
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        library
            .preferred_media_file(track)
            .await
            .unwrap()
            .unwrap()
            .id,
        cache
    );

    // A folder copy outranks the evictable cache copy.
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let folder_row = library.add_library_folder(&music, None).await.unwrap();
    let folder = library
        .upsert_folder_media_file(
            track,
            &ScannedMediaFile {
                folder_id: folder_row.id,
                rel_path: "Artist/Album/track.flac".to_owned(),
                size: 2,
                mtime_ms: 2,
                format: Some("flac".to_owned()),
                codec: Some("flac".to_owned()),
                bitrate_kbps: None,
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                channels: Some(2),
                duration_ms: Some(1_000),
                lossless: Some(true),
                replaygain: None,
                has_embedded_art: Some(true),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        library
            .preferred_media_file(track)
            .await
            .unwrap()
            .unwrap()
            .id,
        folder
    );

    // A permanent managed copy outranks the folder copy.
    let library_file = library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Library,
            rel_path: "Artist/Album/01 - Cafe del Mar.flac".to_owned(),
            size: 3,
            mtime_ms: 3,
            format: Some("flac".to_owned()),
            codec: Some("flac".to_owned()),
            bitrate_kbps: None,
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            duration_ms: Some(1_000),
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: Some(true),
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        library
            .preferred_media_file(track)
            .await
            .unwrap()
            .unwrap()
            .id,
        library_file
    );

    // Exactly one file is preferred at a time.
    let files = library.list_media_files_for_track(track).await.unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(files.iter().filter(|file| file.is_preferred).count(), 1);

    // Deleting the winner hands the flag to the next rank.
    assert!(library.delete_media_file(library_file).await.unwrap());
    assert_eq!(
        library
            .preferred_media_file(track)
            .await
            .unwrap()
            .unwrap()
            .id,
        folder
    );
}

#[tokio::test]
async fn media_file_input_resolves_paths_by_storage_class() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let track = library
        .insert_track(&track_input("Cafe del Mar"), &[], &[])
        .await
        .unwrap();
    let id = library
        .insert_media_file(&MediaFileInput {
            track_id: track,
            storage: StorageClass::Cache,
            folder_id: None,
            rel_path: "Artist/Album/track.MP3".to_owned(),
            size: 1,
            mtime_ms: 1,
            format: None,
            codec: None,
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            duration_ms: None,
            lossless: None,
            quality_tier: None,
            replaygain: None,
            has_embedded_art: None,
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();

    let file = library.get_media_file(id).await.unwrap().unwrap();
    assert_eq!(file.suffix(), "mp3");
    let roots = hearthsound_library::StorageRoots::new("/srv/library", "/srv/cache");
    assert_eq!(
        file.absolute_path(None, &roots),
        Path::new("/srv/cache/Artist/Album/track.MP3")
    );
}

#[tokio::test]
async fn library_folders_canonicalize_rename_and_reassociate() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let added = library.add_library_folder(&music, None).await.unwrap();
    assert_eq!(added.name, "music");
    assert!(added.path.is_absolute());
    assert_eq!(added.owner_user_id, None);
    assert!(added.watch);
    assert_eq!(added.last_scan_at, None);

    // The same directory through a dotted spelling returns the same row.
    let dotted = music.join(".");
    let owner = insert_user(&db).await;
    let again = library
        .add_library_folder(&dotted, Some(owner))
        .await
        .unwrap();
    assert_eq!(again.id, added.id);
    assert_eq!(
        again.owner_user_id, None,
        "the owner is not applied on a re-add"
    );

    assert!(
        library
            .rename_library_folder(added.id, "Music")
            .await
            .unwrap()
    );
    assert!(
        library
            .set_library_folder_owner(added.id, Some(owner))
            .await
            .unwrap()
    );
    let stored = library.get_library_folder(added.id).await.unwrap().unwrap();
    assert_eq!(stored.name, "Music");
    assert_eq!(stored.owner_user_id, Some(owner));
    assert_eq!(library.list_library_folders().await.unwrap().len(), 1);

    // A file that is not a directory is rejected.
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    assert!(library.add_library_folder(&file, None).await.is_err());
    assert!(
        library
            .add_library_folder(&dir.path().join("missing"), None)
            .await
            .is_err()
    );

    assert!(
        !library
            .rename_library_folder(uuid::Uuid::now_v7(), "x")
            .await
            .unwrap()
    );
}

#[test]
fn sort_key_folds_case_and_whitespace() {
    assert_eq!(sort_key("  The  Beatles "), "the beatles");
    assert_eq!(sort_key("CAFE\u{a0}DEL MAR"), "cafe del mar");
    assert_eq!(sort_key("   "), "");
}

/// Inserts an artist row straight into the table and returns its id.
///
/// The rows a provider alias points at are written by the catalog layer, which
/// resolves an id before the library sees it; a test writes them the same way.
async fn insert_artist_row(db: &Database, name: &str) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO artists (id, name, sort_name, created_at, updated_at) \
         VALUES (?, ?, ?, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(name)
    .bind(hearthsound_library::sort_key(name))
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Inserts a provider alias for an artist.
async fn insert_artist_alias(db: &Database, artist: uuid::Uuid, namespace: &str, value: &str) {
    sqlx::query(
        "INSERT INTO external_ids (namespace, value, entity_kind, entity_id) \
         VALUES (?, ?, 'artist', ?)",
    )
    .bind(namespace)
    .bind(value)
    .bind(artist.as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// An untagged artist stays apart when two identified artists share its name.
///
/// The name alone no longer says which act an untagged file belongs to, so it
/// gets a row of its own instead of joining one of them - and every other
/// untagged file of that name joins the same row.
#[tokio::test]
async fn an_unidentified_artist_stays_apart_when_two_identified_artists_share_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let first = library
        .upsert_artist(&mbid_artist("Alex G", "mbid-a"))
        .await
        .unwrap();
    let second = library
        .upsert_artist(&mbid_artist("Alex G", "mbid-b"))
        .await
        .unwrap();
    assert_ne!(first, second);

    let untagged = library
        .upsert_artist(&ArtistInput::new("Alex G"))
        .await
        .unwrap();
    assert_ne!(untagged, first);
    assert_ne!(untagged, second);
    let again = library
        .upsert_artist(&ArtistInput::new("alex g"))
        .await
        .unwrap();
    assert_eq!(again, untagged, "untagged files of one name share one row");
    assert_eq!(library.list_artists().await.unwrap().len(), 3);
}

/// An artist identified only by a provider alias counts as identified.
#[tokio::test]
async fn an_artist_identified_only_by_a_provider_alias_counts_as_identified() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let aliased = insert_artist_row(&db, "Alex G").await;
    insert_artist_alias(&db, aliased, "p:deezer", "3135556").await;

    // One identified artist of the name exists, so a tagged file is a new act.
    let tagged = library
        .upsert_artist(&mbid_artist("Alex G", "mbid-a"))
        .await
        .unwrap();
    assert_ne!(tagged.uuid(), aliased);

    // Two identified artists share the name, so an untagged file gets its own.
    let untagged = library
        .upsert_artist(&ArtistInput::new("Alex G"))
        .await
        .unwrap();
    assert_ne!(untagged.uuid(), aliased);
    assert_ne!(untagged, tagged);
    assert_eq!(library.list_artists().await.unwrap().len(), 3);
}

/// A MusicBrainz id stored as an alias finds its row.
#[tokio::test]
async fn a_music_brainz_id_stored_as_an_alias_matches_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let aliased = insert_artist_row(&db, "Aphex Twin").await;
    insert_artist_alias(&db, aliased, "mb-artist", "mbid-aphex").await;

    let tagged = library
        .upsert_artist(&mbid_artist("Aphex Twin", "mbid-aphex"))
        .await
        .unwrap();
    assert_eq!(tagged.uuid(), aliased, "the alias is the artist");
    assert_eq!(
        artist_ids(&db, aliased).await,
        [ExternalId {
            namespace: MB_ARTIST.to_owned(),
            value: "mbid-aphex".to_owned(),
        }]
    );
    assert_eq!(library.list_artists().await.unwrap().len(), 1);
}

/// An album's year and release date are written once.
#[tokio::test]
async fn an_album_year_is_not_overwritten_by_a_later_upsert() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let credits = [ArtistCredit::new("Energy 52", "main")];

    let mut first = AlbumInput::new("Cafe del Mar");
    first.year = Some(2001);
    first.release_date = Some("2001-06-04".to_owned());
    let id = library.upsert_album(&first, &credits).await.unwrap();

    let mut later = AlbumInput::new("Cafe del Mar");
    later.year = Some(2010);
    later.release_date = Some("2010-01-01".to_owned());
    later.ids.push(release_id("mbid-release"));
    assert_eq!(library.upsert_album(&later, &credits).await.unwrap(), id);

    let stored = library.get_album(id).await.unwrap().unwrap();
    assert_eq!(stored.year, Some(2001), "the first year wins");
    assert_eq!(stored.release_date.as_deref(), Some("2001-06-04"));
    assert_eq!(
        album_ids(&db, id.uuid()).await,
        [release_id("mbid-release")],
        "the id is adopted"
    );
}

/// An album re-tagged with a release id is the same album.
#[tokio::test]
async fn an_album_gains_its_music_brainz_id_without_duplicating() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let credits = [ArtistCredit::new("Energy 52", "main")];

    let mut untagged = AlbumInput::new("Cafe del Mar");
    untagged.year = Some(1993);
    let id = library.upsert_album(&untagged, &credits).await.unwrap();

    let mut tagged = AlbumInput::new("Cafe del Mar");
    tagged.year = Some(1993);
    tagged.ids.push(release_id("mbid-release"));
    assert_eq!(library.upsert_album(&tagged, &credits).await.unwrap(), id);
    assert_eq!(
        album_ids(&db, id.uuid()).await,
        [release_id("mbid-release")]
    );
    let albums = library
        .list_albums_for_artist(library.list_album_artists(id).await.unwrap()[0].id)
        .await
        .unwrap();
    assert_eq!(albums.len(), 1);
}

/// The removal preview counts the likes and playlist entries a removal loses.
///
/// The counts come from the delete's own predicate, so a track that still has a
/// file in another folder keeps its likes and the preview says so.
#[tokio::test]
async fn removal_impact_counts_the_likes_and_playlist_entries_a_removal_would_lose() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let user = insert_user(&db).await;
    let music = dir.path().join("music");
    let other = dir.path().join("other");
    std::fs::create_dir(&music).unwrap();
    std::fs::create_dir(&other).unwrap();
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let elsewhere = library.add_library_folder(&other, None).await.unwrap();

    let track = library
        .insert_track(
            &track_input("Cafe del Mar"),
            &[ArtistCredit::new("Energy 52", "main")],
            &[],
        )
        .await
        .unwrap();
    library
        .insert_media_file(&MediaFileInput {
            track_id: track,
            storage: StorageClass::Folder,
            folder_id: Some(folder.id),
            rel_path: "Cafe del Mar.flac".to_owned(),
            size: 1,
            mtime_ms: 0,
            format: None,
            codec: None,
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            duration_ms: None,
            lossless: None,
            quality_tier: None,
            replaygain: None,
            has_embedded_art: None,
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, 'track', ?, 'like', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
    let playlist = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO playlists (id, owner_user_id, name, is_public, is_temporary, created_at, \
                                updated_at) VALUES (?, ?, 'Mix', 0, 0, 0, 0)",
    )
    .bind(playlist.as_bytes().as_slice())
    .bind(user.as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO playlist_tracks (playlist_id, position, track_id, added_at) \
         VALUES (?, 0, ?, 0)",
    )
    .bind(playlist.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();

    let impact = library.removal_impact(folder.id).await.unwrap();
    assert_eq!(impact.tracks, 1);
    assert_eq!(impact.likes, 1);
    assert_eq!(impact.playlist_entries, 1);

    // A second file of the same track outside the folder keeps it, so a removal
    // would take nothing.
    library
        .insert_media_file(&MediaFileInput {
            track_id: track,
            storage: StorageClass::Folder,
            folder_id: Some(elsewhere.id),
            rel_path: "Copy.flac".to_owned(),
            size: 1,
            mtime_ms: 0,
            format: None,
            codec: None,
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            duration_ms: None,
            lossless: None,
            quality_tier: None,
            replaygain: None,
            has_embedded_art: None,
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        library.removal_impact(folder.id).await.unwrap(),
        hearthsound_library::FolderRemovalImpact::default()
    );

    library.remove_library_folder(folder.id).await.unwrap();
    assert_eq!(
        library.removal_impact(folder.id).await.unwrap(),
        hearthsound_library::FolderRemovalImpact::default(),
        "a removed folder has nothing left to lose"
    );
}

#[tokio::test]
async fn identifying_an_artist_merges_the_row_that_holds_the_id_into_the_older_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    // The act the library stores under the provider's own id: one row, credited
    // by the file that was scanned, with no MusicBrainz id of its own.
    let track = library
        .insert_track(
            &track_input("Cherry-coloured Funk"),
            &[ArtistCredit::new("Cocteau Twins", "main")],
            &[],
        )
        .await
        .unwrap();
    let credited = library.list_track_artists(track).await.unwrap()[0].id;
    sqlx::query(
        "INSERT INTO external_ids (namespace, value, entity_kind, entity_id) \
         VALUES ('p:deezer', '1041', 'artist', ?)",
    )
    .bind(credited.uuid().as_bytes().to_vec())
    .execute(db.writer())
    .await
    .unwrap();

    // Resolution went by name and stored a second row for the same act.
    let twin = library
        .upsert_artist(&mbid_artist("Cocteau Twins", "mbid-ct"))
        .await
        .unwrap();
    assert_ne!(twin, credited, "the name alone did not resolve it");

    let mut tx = db.writer().begin().await.unwrap();
    let survivor = hearthsound_library::artists::identify_artist(&mut tx, credited, "mbid-ct")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(survivor, credited, "the older row survives");
    assert_eq!(
        library.list_track_artists(track).await.unwrap()[0].id,
        credited,
        "the track keeps crediting the surviving row"
    );
    assert!(
        artist_ids(&db, credited.uuid())
            .await
            .contains(&ExternalId {
                namespace: MB_ARTIST.to_owned(),
                value: "mbid-ct".to_owned(),
            }),
        "the id moved onto the surviving row"
    );
    assert!(library.get_artist(twin).await.unwrap().is_none());
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artists WHERE name = ?")
        .bind("Cocteau Twins")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(remaining, 1, "one act, one row");
}

#[tokio::test]
async fn identifying_an_album_merges_the_row_that_holds_the_release_into_the_older_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let credits = [ArtistCredit::new("Cocteau Twins", "main")];
    let mut first = AlbumInput::new("Heaven or Las Vegas");
    first.year = Some(1990);
    let album = library.upsert_album(&first, &credits).await.unwrap();
    let track = library
        .insert_track(
            &TrackInput {
                album_id: Some(album),
                ..track_input("Cherry-coloured Funk")
            },
            &credits,
            &[],
        )
        .await
        .unwrap();

    // A re-tagged file named the album differently and carried the release, so
    // it stored a second row for the release.
    let mut released = AlbumInput::new("Heaven or Las Vegas (Deluxe)");
    released.year = Some(1990);
    released.ids.push(release_id("rel-heaven"));
    let holder = library.upsert_album(&released, &credits).await.unwrap();
    assert_ne!(holder, album);

    let mut tx = db.writer().begin().await.unwrap();
    let survivor = hearthsound_library::albums::identify_album(&mut tx, album, "rel-heaven", None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(survivor, album, "the older row survives");
    assert_eq!(
        library.get_track(track).await.unwrap().unwrap().album_id,
        Some(album),
        "the track stays on the surviving album"
    );
    assert_eq!(
        album_ids(&db, album.uuid()).await,
        [release_id("rel-heaven")],
        "the release id moved onto the surviving row"
    );
    assert!(library.get_album(holder).await.unwrap().is_none());
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM albums")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(remaining, 1, "one release, one row");
}
