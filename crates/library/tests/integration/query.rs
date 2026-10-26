//! Tests for the browse and search queries: personal library membership, the
//! server scope, album ordering, local full-text search, and the preferred file
//! resolution.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::Path;

use hearthsound_core::{Availability, EntityId, EntityKind, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::query::{
    self, AlbumSort, ArtistSort, Scope, ServerBrowse, TrackSort, content_type_for, sanitize_match,
};
use hearthsound_library::{
    Album, AlbumInput, ArtistCredit, LibraryRepository, ManagedMediaFile, Origin, ScannedMediaFile,
    StorageRoots, TrackInput,
};
use uuid::Uuid;

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

async fn insert_user(db: &Database, name: &str) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, 'x', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("{name}-{id}"))
    .bind(name)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Inserts an album by `artist` and returns both ids.
async fn make_album(
    library: &LibraryRepository,
    artist: &str,
    title: &str,
) -> (EntityId, EntityId) {
    let album = AlbumInput::new(title, Origin::Local);
    let credits = [ArtistCredit::new(artist, "main")];
    let album_id = library.upsert_album(&album, &credits).await.unwrap();
    let artist_id = library.list_album_artists(album_id).await.unwrap()[0].id;
    (album_id, artist_id)
}

/// Inserts a track in `album` and returns its id.
async fn make_track(
    library: &LibraryRepository,
    album: Option<EntityId>,
    title: &str,
    availability: Availability,
) -> EntityId {
    let mut input = TrackInput::new(title, Origin::Local, availability);
    input.album_id = album;
    library.insert_track(&input, &[], &[]).await.unwrap()
}

/// Attaches a managed file to a track.
async fn attach_managed(
    library: &LibraryRepository,
    track: EntityId,
    storage: StorageClass,
    rel_path: &str,
) {
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage,
            rel_path: rel_path.to_owned(),
            size: 1_000,
            mtime_ms: 1,
            format: Some("flac".to_owned()),
            codec: Some("flac".to_owned()),
            bitrate_kbps: Some(900),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            duration_ms: Some(123_000),
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: Some(false),
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
}

/// Attaches a scanned folder file to a track.
async fn attach_folder(
    library: &LibraryRepository,
    track: EntityId,
    folder_id: uuid::Uuid,
    name: &str,
) {
    library
        .upsert_folder_media_file(
            track,
            &ScannedMediaFile {
                folder_id,
                rel_path: name.to_owned(),
                size: 2_000,
                mtime_ms: 2,
                format: Some("mpeg".to_owned()),
                codec: Some("mp3".to_owned()),
                bitrate_kbps: Some(320),
                sample_rate: Some(44_100),
                bit_depth: None,
                channels: Some(2),
                duration_ms: Some(61_000),
                lossless: Some(false),
                replaygain: None,
                has_embedded_art: Some(false),
            },
        )
        .await
        .unwrap();
}

/// Returns the literal an `entity_kind` column stores.
fn kind_literal(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Track => "track",
        EntityKind::Album => "album",
        EntityKind::Artist => "artist",
        EntityKind::Playlist => "playlist",
    }
}

/// Records a like.
async fn like(db: &Database, user: uuid::Uuid, kind: EntityKind, id: EntityId) {
    like_at(db, user, kind, id, 0).await;
}

/// Records a like at one instant.
async fn like_at(db: &Database, user: uuid::Uuid, kind: EntityKind, id: EntityId, at: i64) {
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) VALUES (?, ?, ?, 'like', ?)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind_literal(kind))
    .bind(id.uuid().as_bytes().as_slice())
    .bind(at)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records an explicit save.
async fn save(db: &Database, user: uuid::Uuid, kind: EntityKind, id: EntityId) {
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, ?, ?, 'saved', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind_literal(kind))
    .bind(id.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Creates a playlist and returns its id.
async fn playlist(db: &Database, user: uuid::Uuid, temporary: bool) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO playlists (id, owner_user_id, name, is_temporary, created_at, updated_at) \
         VALUES (?, ?, 'mix', ?, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(user.as_bytes().as_slice())
    .bind(temporary)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Adds a track to a playlist at `position`.
async fn add_to_playlist(db: &Database, playlist: uuid::Uuid, track: EntityId, position: i64) {
    sqlx::query(
        "INSERT INTO playlist_tracks (playlist_id, position, track_id, added_at) VALUES (?, ?, ?, 0)",
    )
    .bind(playlist.as_bytes().as_slice())
    .bind(position)
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Everything a scope test needs to reason about.
struct Seeded {
    db: Database,
    library: LibraryRepository,
    user: uuid::Uuid,
    other: uuid::Uuid,
    /// A track whose file is in a folder owned by `user`.
    owned_track: EntityId,
    /// A track whose file is in a generic folder.
    generic_track: EntityId,
    /// A permanent managed track nobody adopted.
    library_track: EntityId,
    /// An evictable managed track nobody adopted.
    cache_track: EntityId,
    /// A managed track `user` liked.
    liked_track: EntityId,
    /// A managed track `user` saved.
    saved_track: EntityId,
    /// A managed track on `user`'s permanent playlist.
    playlist_track: EntityId,
    /// A managed track on `user`'s temporary playlist.
    temporary_track: EntityId,
    owned_artist: EntityId,
    generic_artist: EntityId,
    liked_artist: EntityId,
}

async fn seed(dir: &Path) -> Seeded {
    let db = migrated(dir).await;
    let library = LibraryRepository::new(&db);
    let user = insert_user(&db, "listener").await;
    let other = insert_user(&db, "other").await;

    let owned_dir = dir.join("owned");
    let generic_dir = dir.join("generic");
    std::fs::create_dir(&owned_dir).unwrap();
    std::fs::create_dir(&generic_dir).unwrap();
    let owned_folder = library
        .add_library_folder(&owned_dir, Some(user))
        .await
        .unwrap();
    let generic_folder = library
        .add_library_folder(&generic_dir, None)
        .await
        .unwrap();

    let (owned_album, owned_artist) = make_album(&library, "Owned Artist", "Owned Album").await;
    let owned_track = make_track(
        &library,
        Some(owned_album),
        "Owned Song",
        Availability::Local,
    )
    .await;
    attach_folder(&library, owned_track, owned_folder.id, "owned.mp3").await;

    let (generic_album, generic_artist) =
        make_album(&library, "Generic Artist", "Generic Album").await;
    let generic_track = make_track(
        &library,
        Some(generic_album),
        "Generic Song",
        Availability::Local,
    )
    .await;
    attach_folder(&library, generic_track, generic_folder.id, "generic.mp3").await;

    let (library_album, _) = make_album(&library, "Library Artist", "Library Album").await;
    let library_track = make_track(
        &library,
        Some(library_album),
        "Library Song",
        Availability::Local,
    )
    .await;
    attach_managed(
        &library,
        library_track,
        StorageClass::Library,
        "lib/song.flac",
    )
    .await;

    let (cache_album, _) = make_album(&library, "Cache Artist", "Cache Album").await;
    let cache_track = make_track(
        &library,
        Some(cache_album),
        "Cache Song",
        Availability::Cached,
    )
    .await;
    attach_managed(
        &library,
        cache_track,
        StorageClass::Cache,
        "cache/song.flac",
    )
    .await;

    let (liked_album, liked_artist) = make_album(&library, "Liked Artist", "Liked Album").await;
    let liked_track = make_track(
        &library,
        Some(liked_album),
        "Liked Song",
        Availability::Local,
    )
    .await;
    attach_managed(
        &library,
        liked_track,
        StorageClass::Library,
        "liked/song.flac",
    )
    .await;
    like(&db, user, EntityKind::Track, liked_track).await;

    let (saved_album, _) = make_album(&library, "Saved Artist", "Saved Album").await;
    let saved_track = make_track(
        &library,
        Some(saved_album),
        "Saved Song",
        Availability::Local,
    )
    .await;
    attach_managed(
        &library,
        saved_track,
        StorageClass::Library,
        "saved/song.flac",
    )
    .await;
    save(&db, user, EntityKind::Track, saved_track).await;

    let (playlist_album, _) = make_album(&library, "Playlist Artist", "Playlist Album").await;
    let playlist_track = make_track(
        &library,
        Some(playlist_album),
        "Playlist Song",
        Availability::Local,
    )
    .await;
    attach_managed(
        &library,
        playlist_track,
        StorageClass::Library,
        "pl/song.flac",
    )
    .await;
    let permanent = playlist(&db, user, false).await;
    add_to_playlist(&db, permanent, playlist_track, 0).await;

    let (temporary_album, _) = make_album(&library, "Temporary Artist", "Temporary Album").await;
    let temporary_track = make_track(
        &library,
        Some(temporary_album),
        "Temporary Song",
        Availability::Local,
    )
    .await;
    attach_managed(
        &library,
        temporary_track,
        StorageClass::Library,
        "tmp/song.flac",
    )
    .await;
    let temporary = playlist(&db, user, true).await;
    add_to_playlist(&db, temporary, temporary_track, 0).await;

    Seeded {
        db,
        library,
        user,
        other,
        owned_track,
        generic_track,
        library_track,
        cache_track,
        liked_track,
        saved_track,
        playlist_track,
        temporary_track,
        owned_artist,
        generic_artist,
        liked_artist,
    }
}

#[tokio::test]
async fn the_personal_library_is_the_union_of_its_sources() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();
    let scope = Scope::Personal {
        user_id: seeded.user,
    };

    for track in [
        seeded.owned_track,
        seeded.liked_track,
        seeded.saved_track,
        seeded.playlist_track,
    ] {
        assert!(
            query::track(&mut conn, scope, track)
                .await
                .unwrap()
                .is_some(),
            "{track} should be in the personal library"
        );
    }
    for track in [
        seeded.generic_track,
        seeded.library_track,
        seeded.cache_track,
        seeded.temporary_track,
    ] {
        assert!(
            query::track(&mut conn, scope, track)
                .await
                .unwrap()
                .is_none(),
            "{track} should not be in the personal library"
        );
    }

    // The generic track is in nobody's library, but the server still has it.
    let server = Scope::Server(ServerBrowse::All);
    assert!(
        query::track(&mut conn, server, seeded.generic_track)
            .await
            .unwrap()
            .is_some()
    );

    // A liked track surfaces its album and its artist.
    let liked_track = seeded
        .library
        .get_track(seeded.liked_track)
        .await
        .unwrap()
        .unwrap();
    let liked_album = liked_track.album_id.unwrap();
    assert!(
        query::album_detail(&mut conn, scope, liked_album)
            .await
            .unwrap()
            .is_some(),
        "the album of a liked track is in the library"
    );
    assert!(
        query::artist_detail(&mut conn, scope, seeded.liked_artist)
            .await
            .unwrap()
            .is_some(),
        "the artist of a liked track is in the library"
    );

    let artists = query::list_artists(&mut conn, scope, ArtistSort::Name, -1, 0)
        .await
        .unwrap();
    let names: Vec<&str> = artists.iter().map(|a| a.artist.name.as_str()).collect();
    assert!(names.contains(&"Owned Artist"));
    assert!(names.contains(&"Liked Artist"));
    assert!(names.contains(&"Saved Artist"));
    assert!(names.contains(&"Playlist Artist"));
    assert!(
        !names.contains(&"Generic Artist"),
        "a generic folder is nobody's"
    );
    assert!(!names.contains(&"Cache Artist"));
    assert!(
        !names.contains(&"Temporary Artist"),
        "temporary playlists do not pin"
    );

    // The album count follows the scope.
    let owned = artists
        .iter()
        .find(|summary| summary.artist.id == seeded.owned_artist)
        .unwrap();
    assert_eq!(owned.album_count, 1);

    // Another user sees nothing of this.
    let stranger = Scope::Personal {
        user_id: seeded.other,
    };
    assert!(
        query::list_artists(&mut conn, stranger, ArtistSort::Name, -1, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        query::track(&mut conn, stranger, seeded.owned_track)
            .await
            .unwrap()
            .is_none()
    );

    // The generic folder's artist is visible to nobody personally, but the
    // server scope still carries it.
    assert!(
        query::artist_detail(&mut conn, server, seeded.generic_artist)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn track_listings_follow_the_scope_and_page() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();
    let scope = Scope::Personal {
        user_id: seeded.user,
    };

    let listed = query::list_tracks(&mut conn, scope, TrackSort::Title, None, -1, 0)
        .await
        .unwrap();
    let titles: Vec<&str> = listed
        .iter()
        .map(|view| view.track.title.as_str())
        .collect();
    for title in ["Owned Song", "Liked Song", "Saved Song", "Playlist Song"] {
        assert!(titles.contains(&title), "{title} should be listed");
    }
    for title in ["Generic Song", "Cache Song", "Temporary Song"] {
        assert!(!titles.contains(&title), "{title} should not be listed");
    }

    // The listing is ordered by sort title, and paging walks it without repeats.
    let mut sorted = titles.clone();
    sorted.sort();
    assert_eq!(titles, sorted);
    let page_one = query::list_tracks(&mut conn, scope, TrackSort::Title, None, 2, 0)
        .await
        .unwrap();
    let page_two = query::list_tracks(&mut conn, scope, TrackSort::Title, None, 2, 2)
        .await
        .unwrap();
    assert_eq!(page_one.len(), 2);
    assert!(page_two.iter().all(|view| !page_one.contains(view)));

    // The server scope adds the permanent library tracks and the evictable
    // cache one, and the disabled view offers nothing.
    let everything = Scope::Server(ServerBrowse::All);
    let listed = query::list_tracks(&mut conn, everything, TrackSort::Title, None, -1, 0)
        .await
        .unwrap();
    let titles: Vec<&str> = listed
        .iter()
        .map(|view| view.track.title.as_str())
        .collect();
    assert!(titles.contains(&"Library Song"));
    assert!(titles.contains(&"Cache Song"));

    let disabled = Scope::Server(ServerBrowse::Disabled);
    assert!(
        query::list_tracks(&mut conn, disabled, TrackSort::Title, None, -1, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_server_scope_holds_every_stored_track() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();

    // The view holds the library tracks and the evictable cache ones.
    let everything = Scope::Server(ServerBrowse::All);
    assert!(
        query::track(&mut conn, everything, seeded.cache_track)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        query::track(&mut conn, everything, seeded.library_track)
            .await
            .unwrap()
            .is_some()
    );
    let albums = query::list_albums(&mut conn, everything, AlbumSort::Newest, -1, 0)
        .await
        .unwrap();
    assert!(albums.iter().any(|album| album.title == "Cache Album"));
    assert!(albums.iter().any(|album| album.title == "Library Album"));

    // The disabled view offers nothing at all.
    let disabled = Scope::Server(ServerBrowse::Disabled);
    assert!(
        query::track(&mut conn, disabled, seeded.library_track)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        query::list_albums(&mut conn, disabled, AlbumSort::Newest, -1, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        query::list_artists(&mut conn, disabled, ArtistSort::Name, -1, 0)
            .await
            .unwrap()
            .is_empty()
    );

    // A principal without the server view reads the disabled scope.
    assert_eq!(
        Scope::server_for_principal(true),
        Scope::Server(ServerBrowse::All)
    );
    assert_eq!(
        Scope::server_for_principal(false),
        Scope::Server(ServerBrowse::Disabled)
    );
}

/// The counts a client sizes its placeholders with match the lists they stand
/// for.
#[tokio::test]
async fn the_counts_match_the_listings_they_size() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();

    for scope in [
        Scope::Personal {
            user_id: seeded.user,
        },
        Scope::Server(ServerBrowse::All),
    ] {
        let artists = query::list_artists(&mut conn, scope, ArtistSort::Name, -1, 0)
            .await
            .unwrap()
            .len() as i64;
        let albums = query::list_albums(&mut conn, scope, AlbumSort::AlphabeticalByName, -1, 0)
            .await
            .unwrap()
            .len() as i64;
        let tracks = query::list_tracks(&mut conn, scope, TrackSort::Title, None, -1, 0)
            .await
            .unwrap()
            .len() as i64;

        assert_eq!(
            query::count_artists(&mut conn, scope).await.unwrap(),
            artists,
            "{scope:?} counts its artists as its listing lists them"
        );
        assert_eq!(
            query::count_albums(&mut conn, scope).await.unwrap(),
            albums,
            "{scope:?} counts its albums as its listing lists them"
        );
        assert_eq!(
            query::count_tracks(&mut conn, scope).await.unwrap(),
            tracks,
            "{scope:?} counts its tracks as its listing lists them"
        );
    }

    let starred = query::starred(&mut conn, seeded.user).await.unwrap();
    let counts = query::starred_counts(&mut conn, seeded.user).await.unwrap();
    assert_eq!(counts.artists, starred.artists.len() as i64);
    assert_eq!(counts.albums, starred.albums.len() as i64);
    assert_eq!(counts.tracks, starred.tracks.len() as i64);
    assert!(counts.tracks > 0, "the seeded account likes a track");
    assert_eq!(
        query::starred_counts(&mut conn, seeded.other)
            .await
            .unwrap(),
        query::StarredCounts::default(),
        "one account's likes are never counted for another"
    );

    let disabled = Scope::Server(ServerBrowse::Disabled);
    assert_eq!(query::count_artists(&mut conn, disabled).await.unwrap(), 0);
    assert_eq!(query::count_albums(&mut conn, disabled).await.unwrap(), 0);
    assert_eq!(query::count_tracks(&mut conn, disabled).await.unwrap(), 0);
}

#[tokio::test]
async fn album_lists_are_ordered_and_paged() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();
    let scope = Scope::Personal {
        user_id: seeded.user,
    };

    let newest = query::list_albums(&mut conn, scope, AlbumSort::Newest, -1, 0)
        .await
        .unwrap();
    let created: Vec<i64> = newest.iter().map(|album| album.created_at).collect();
    let mut sorted = created.clone();
    sorted.sort_by(|left, right| right.cmp(left));
    assert_eq!(created, sorted, "newest first");

    let alphabetical = query::list_albums(&mut conn, scope, AlbumSort::AlphabeticalByName, -1, 0)
        .await
        .unwrap();
    let titles: Vec<&str> = alphabetical
        .iter()
        .map(|album| album.sort_title.as_str())
        .collect();
    let mut sorted = titles.clone();
    sorted.sort();
    assert_eq!(titles, sorted);

    // Paging walks the alphabetical list without repeats.
    let page_one = query::list_albums(&mut conn, scope, AlbumSort::AlphabeticalByName, 2, 0)
        .await
        .unwrap();
    let page_two = query::list_albums(&mut conn, scope, AlbumSort::AlphabeticalByName, 2, 2)
        .await
        .unwrap();
    assert_eq!(page_one.len(), 2);
    assert!(page_two.iter().all(|album| !page_one.contains(album)));

    // A random list returns exactly the requested size from the same set.
    let random = query::list_albums(&mut conn, scope, AlbumSort::Random, 1, 0)
        .await
        .unwrap();
    assert_eq!(random.len(), 1);
    assert!(alphabetical.contains(&random[0]));

    // An artist's albums come back oldest first and scoped.
    let detail = query::artist_detail(&mut conn, scope, seeded.owned_artist)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.albums.len(), 1);
    assert_eq!(detail.albums[0].title, "Owned Album");
    assert_eq!(detail.artist.name, "Owned Artist");

    // An album detail carries its artists and tracks with the file projection.
    let detail = query::album_detail(&mut conn, scope, detail.albums[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.artists[0].name, "Owned Artist");
    assert_eq!(detail.tracks.len(), 1);
    let view = &detail.tracks[0];
    assert_eq!(view.track.title, "Owned Song");
    assert_eq!(view.album.as_ref().unwrap().title, "Owned Album");
    assert!(
        view.artists.is_empty(),
        "this track was inserted without credits of its own"
    );
    let file = view.file.as_ref().unwrap();
    assert_eq!(file.suffix, "mp3");
    assert_eq!(file.content_type, "audio/mpeg");
    assert_eq!(file.size, 2_000);
    assert_eq!(file.bitrate_kbps, Some(320));
    assert_eq!(file.duration_ms, Some(61_000));
    assert_eq!(file.path, "owned.mp3");
}

#[tokio::test]
async fn search_matches_the_local_index_inside_the_scope() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();
    let scope = Scope::Personal {
        user_id: seeded.user,
    };
    let kinds = [EntityKind::Artist, EntityKind::Album, EntityKind::Track];

    let results = query::search(&mut conn, scope, "owned", &kinds, 10, 0)
        .await
        .unwrap();
    assert_eq!(results.artists.len(), 1);
    assert_eq!(results.albums.len(), 1);
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].track.id, seeded.owned_track);

    // The match is a prefix query, and diacritics fold.
    let results = query::search(&mut conn, scope, "own", &[EntityKind::Track], 10, 0)
        .await
        .unwrap();
    assert_eq!(results.tracks.len(), 1);
    assert!(results.artists.is_empty(), "only tracks were asked for");

    // A generic folder's content is outside the personal scope but inside the
    // server scope.
    let results = query::search(&mut conn, scope, "generic", &kinds, 10, 0)
        .await
        .unwrap();
    assert!(results.tracks.is_empty());
    let server = Scope::Server(ServerBrowse::All);
    let results = query::search(&mut conn, server, "generic", &kinds, 10, 0)
        .await
        .unwrap();
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].track.id, seeded.generic_track);

    // A temporary playlist's track is in nobody's personal library.
    let results = query::search(&mut conn, scope, "temporary", &kinds, 10, 0)
        .await
        .unwrap();
    assert!(results.tracks.is_empty());

    // Nothing usable in the query means no results, never everything.
    let results = query::search(&mut conn, scope, "  ...  ", &kinds, 10, 0)
        .await
        .unwrap();
    assert_eq!(results, query::SearchResults::default());
    assert_eq!(
        sanitize_match("Cafe del Mar").unwrap(),
        "\"cafe\"* \"del\"* \"mar\"*"
    );
    assert_eq!(sanitize_match("***"), None);

    // The limit is honoured per kind.
    let results = query::search(&mut conn, scope, "song", &[EntityKind::Track], 1, 0)
        .await
        .unwrap();
    assert_eq!(results.tracks.len(), 1);
}

#[tokio::test]
async fn the_preferred_file_resolves_its_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();
    let roots = StorageRoots::new(dir.path().join("library"), dir.path().join("cache"));

    // A folder file resolves against its folder's path.
    let file = query::preferred_media_file(&mut conn, &roots, seeded.owned_track)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(file.media_file.storage, StorageClass::Folder);
    assert_eq!(file.absolute_path, dir.path().join("owned/owned.mp3"));

    // A managed file resolves against the root of its storage class.
    let file = query::preferred_media_file(&mut conn, &roots, seeded.cache_track)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        file.absolute_path,
        dir.path().join("cache/cache/song.flac"),
        "the cache root plus the relative path"
    );

    // A track with no file has no preferred file.
    let mut input = TrackInput::new("Nothing", Origin::Local, Availability::Remote);
    input.album_id = None;
    let empty = seeded.library.insert_track(&input, &[], &[]).await.unwrap();
    assert!(
        query::preferred_media_file(&mut conn, &roots, empty)
            .await
            .unwrap()
            .is_none()
    );

    // Touching the file records the access; it is a write, so it needs the
    // writer connection.
    let mut writer = seeded.db.writer().acquire().await.unwrap();
    assert!(
        query::touch_last_accessed(&mut writer, file.media_file.id)
            .await
            .unwrap()
    );
    let stored = seeded
        .library
        .get_media_file(file.media_file.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.last_accessed_at.is_some());
    assert!(
        !query::touch_last_accessed(&mut writer, uuid::Uuid::now_v7())
            .await
            .unwrap()
    );
}

#[test]
fn content_types_follow_the_suffix() {
    assert_eq!(content_type_for("flac"), "audio/flac");
    assert_eq!(content_type_for("mp3"), "audio/mpeg");
    assert_eq!(content_type_for("m4a"), "audio/mp4");
    assert_eq!(content_type_for("weird"), "application/octet-stream");
}

/// Inserts an album with a release year and returns both ids.
async fn make_dated_album(
    library: &LibraryRepository,
    artist: &str,
    title: &str,
    year: i64,
) -> (EntityId, EntityId) {
    let mut album = AlbumInput::new(title, Origin::Local);
    album.year = Some(year);
    let credits = [ArtistCredit::new(artist, "main")];
    let album_id = library.upsert_album(&album, &credits).await.unwrap();
    let artist_id = library.list_album_artists(album_id).await.unwrap()[0].id;
    (album_id, artist_id)
}

/// Inserts a track with one genre and returns its id.
async fn make_genre_track(
    library: &LibraryRepository,
    album: EntityId,
    title: &str,
    genre: &str,
) -> EntityId {
    let mut input = TrackInput::new(title, Origin::Local, Availability::Local);
    input.album_id = Some(album);
    library
        .insert_track(&input, &[], &[genre.to_owned()])
        .await
        .unwrap()
}

/// Records a track credit.
async fn credit(db: &Database, track: EntityId, artist: EntityId, position: i64, role: &str) {
    sqlx::query(
        "INSERT INTO track_artists (track_id, artist_id, position, role) VALUES (?, ?, ?, ?)",
    )
    .bind(track.uuid().as_bytes().as_slice())
    .bind(artist.uuid().as_bytes().as_slice())
    .bind(position)
    .bind(role)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Inserts a track with several genres and returns its id.
async fn make_dated_track(
    library: &LibraryRepository,
    album: EntityId,
    title: &str,
    genres: &[&str],
) -> EntityId {
    let mut input = TrackInput::new(title, Origin::Local, Availability::Local);
    input.album_id = Some(album);
    let genres: Vec<String> = genres.iter().map(|genre| (*genre).to_owned()).collect();
    library.insert_track(&input, &[], &genres).await.unwrap()
}

/// Records a rating.
async fn rate(db: &Database, user: uuid::Uuid, kind: EntityKind, id: EntityId, rating: i64) {
    sqlx::query(
        "INSERT INTO user_ratings (user_id, entity_kind, entity_id, rating, at) \
         VALUES (?, ?, ?, ?, 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind_literal(kind))
    .bind(id.uuid().as_bytes().as_slice())
    .bind(rating)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a play.
async fn play(db: &Database, user: uuid::Uuid, track: EntityId, started_at: i64) {
    sqlx::query(
        "INSERT INTO plays (id, user_id, track_id, started_at, listened_ms, audio_delivered) \
         VALUES (?, ?, ?, ?, 180000, 1)",
    )
    .bind(uuid::Uuid::now_v7().as_bytes().as_slice())
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .bind(started_at)
    .execute(db.writer())
    .await
    .unwrap();
}

/// The titles of a listing.
fn titles(albums: &[Album]) -> Vec<&str> {
    albums.iter().map(|album| album.title.as_str()).collect()
}

/// The personal scope of one user.
fn personal(user: uuid::Uuid) -> Scope {
    Scope::Personal { user_id: user }
}

/// Opens a database with one account and an empty folder it owns.
///
/// The album-list tests build their own library rather than the shared seed, so
/// the albums a listing returns are exactly the ones the test created.
async fn album_list_fixture(dir: &Path) -> (Database, LibraryRepository, uuid::Uuid) {
    let db = migrated(dir).await;
    let library = LibraryRepository::new(&db);
    let user = insert_user(&db, "listener").await;
    (db, library, user)
}

/// Every album-list order that reads only the albums themselves.
#[tokio::test]
async fn album_lists_follow_names_years_and_genres() {
    let dir = tempfile::tempdir().unwrap();
    let (db, library, user) = album_list_fixture(dir.path()).await;
    let mut conn = db.readers().acquire().await.unwrap();
    let owned_dir = dir.path().join("list");
    std::fs::create_dir(&owned_dir).unwrap();
    let folder = library
        .add_library_folder(&owned_dir, Some(user))
        .await
        .unwrap();

    // Three albums in the user's own folder, so the personal scope lists them:
    // a 1995 jazz album by Aardvark, a 1998 rock album by Mike, and a 2001 rock
    // album by Zulu.
    let (alpha, _) = make_dated_album(&library, "Aardvark", "Alpha Album", 1995).await;
    let alpha_track = make_genre_track(&library, alpha, "Alpha Song", "jazz").await;
    attach_folder(&library, alpha_track, folder.id, "alpha.mp3").await;
    let (middle, _) = make_dated_album(&library, "Mike", "Middle Album", 1998).await;
    let middle_track = make_genre_track(&library, middle, "Middle Song", "rock").await;
    attach_folder(&library, middle_track, folder.id, "middle.mp3").await;
    let (zed, _) = make_dated_album(&library, "Zulu", "Zed Album", 2001).await;
    let zed_track = make_genre_track(&library, zed, "Zed Song", "rock").await;
    attach_folder(&library, zed_track, folder.id, "zed.mp3").await;

    let scope = personal(user);
    let by_name = query::list_albums(&mut conn, scope, AlbumSort::AlphabeticalByName, -1, 0)
        .await
        .unwrap();
    assert_eq!(
        titles(&by_name),
        ["Alpha Album", "Middle Album", "Zed Album"]
    );
    assert_eq!(
        titles(
            &query::list_albums(&mut conn, scope, AlbumSort::AlphabeticalByArtist, -1, 0)
                .await
                .unwrap()
        ),
        ["Alpha Album", "Middle Album", "Zed Album"],
        "the albums are ordered by their first credited artist"
    );

    // ByYear counts up or down with the range it is given.
    assert_eq!(
        titles(
            &query::list_albums(
                &mut conn,
                scope,
                AlbumSort::ByYear {
                    from: 1990,
                    to: 1999
                },
                -1,
                0,
            )
            .await
            .unwrap()
        ),
        ["Alpha Album", "Middle Album"],
        "an album outside the range is left out"
    );
    assert_eq!(
        titles(
            &query::list_albums(
                &mut conn,
                scope,
                AlbumSort::ByYear {
                    from: 1999,
                    to: 1990
                },
                -1,
                0,
            )
            .await
            .unwrap()
        ),
        ["Middle Album", "Alpha Album"],
        "a range that counts down is ordered newest first"
    );

    assert_eq!(
        titles(
            &query::list_albums(
                &mut conn,
                scope,
                AlbumSort::ByGenre("rock".to_owned()),
                -1,
                0,
            )
            .await
            .unwrap()
        ),
        ["Middle Album", "Zed Album"]
    );
    assert_eq!(
        titles(
            &query::list_albums(
                &mut conn,
                scope,
                AlbumSort::ByGenre("jazz".to_owned()),
                -1,
                0,
            )
            .await
            .unwrap()
        ),
        ["Alpha Album"]
    );
}

/// Every album-list order that reads what the user did.
#[tokio::test]
async fn album_lists_follow_the_users_activity() {
    let dir = tempfile::tempdir().unwrap();
    let (db, library, user) = album_list_fixture(dir.path()).await;
    let other = insert_user(&db, "other").await;
    let mut conn = db.readers().acquire().await.unwrap();
    let owned_dir = dir.path().join("activity");
    std::fs::create_dir(&owned_dir).unwrap();
    let folder = library
        .add_library_folder(&owned_dir, Some(user))
        .await
        .unwrap();

    let (first, _) = make_dated_album(&library, "Artist A", "Album A", 2000).await;
    let first_track = make_track(&library, Some(first), "Song A", Availability::Local).await;
    attach_folder(&library, first_track, folder.id, "a.mp3").await;
    let (second, _) = make_dated_album(&library, "Artist B", "Album B", 2000).await;
    let second_track = make_track(&library, Some(second), "Song B", Availability::Local).await;
    attach_folder(&library, second_track, folder.id, "b.mp3").await;
    let (third, _) = make_dated_album(&library, "Artist C", "Album C", 2000).await;
    let third_track = make_track(&library, Some(third), "Song C", Availability::Local).await;
    attach_folder(&library, third_track, folder.id, "c.mp3").await;

    rate(&db, user, EntityKind::Album, first, 5).await;
    rate(&db, user, EntityKind::Album, second, 2).await;
    // Another user's rating must not move this user's list.
    rate(&db, other, EntityKind::Album, third, 5).await;
    play(&db, user, first_track, 1_000).await;
    play(&db, user, first_track, 2_000).await;
    play(&db, user, second_track, 3_000).await;
    like(&db, user, EntityKind::Album, second).await;

    let scope = personal(user);
    for (sort, expected, reason) in [
        (
            AlbumSort::Highest { user },
            &["Album A", "Album B"][..],
            "only rated albums are listed, the highest first",
        ),
        (
            AlbumSort::Frequent { user },
            &["Album A", "Album B"][..],
            "only played albums are listed, the most played first",
        ),
        (
            AlbumSort::Recent { user },
            &["Album B", "Album A"][..],
            "only played albums are listed, the most recently played first",
        ),
        (
            AlbumSort::Starred { user },
            &["Album B"][..],
            "only liked albums are listed",
        ),
    ] {
        let listed = query::list_albums(&mut conn, scope, sort, -1, 0)
            .await
            .unwrap();
        assert_eq!(titles(&listed), expected, "{reason}");
    }
}

/// The genres of the songs in scope, the songs of one genre, and a random
/// sample.
#[tokio::test]
async fn genres_are_counted_and_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let (db, library, user) = album_list_fixture(dir.path()).await;
    let mut conn = db.readers().acquire().await.unwrap();
    let owned_dir = dir.path().join("genres");
    std::fs::create_dir(&owned_dir).unwrap();
    let folder = library
        .add_library_folder(&owned_dir, Some(user))
        .await
        .unwrap();

    // Two rock songs on one album, one jazz song on another, and a song with
    // both genres.
    let (rock, _) = make_dated_album(&library, "Rock Artist", "Rock Album", 1994).await;
    let rock_one = make_genre_track(&library, rock, "Rock One", "Rock").await;
    attach_folder(&library, rock_one, folder.id, "rock1.mp3").await;
    let rock_two = make_genre_track(&library, rock, "Rock Two", "rock").await;
    attach_folder(&library, rock_two, folder.id, "rock2.mp3").await;
    let (jazz, _) = make_dated_album(&library, "Jazz Artist", "Jazz Album", 2004).await;
    let jazz_one = make_genre_track(&library, jazz, "Jazz One", "jazz").await;
    attach_folder(&library, jazz_one, folder.id, "jazz1.mp3").await;
    let (both, _) = make_dated_album(&library, "Both Artist", "Both Album", 1999).await;
    let both_one = make_dated_track(&library, both, "Both One", &["rock", "jazz"]).await;
    attach_folder(&library, both_one, folder.id, "both.mp3").await;

    let scope = personal(user);
    let genres = query::list_genres(&mut conn, scope).await.unwrap();
    let listed: Vec<(&str, i64, i64)> = genres
        .iter()
        .map(|genre| (genre.name.as_str(), genre.song_count, genre.album_count))
        .collect();
    assert_eq!(
        listed,
        [("jazz", 2, 2), ("rock", 3, 2)],
        "a genre's songs and albums are counted in scope, by folded name"
    );

    // The genre match is folded too.
    let by_genre = query::tracks_by_genre(&mut conn, scope, "ROCK", -1, 0)
        .await
        .unwrap();
    let titles: Vec<&str> = by_genre
        .iter()
        .map(|view| view.track.title.as_str())
        .collect();
    assert_eq!(titles, ["Both One", "Rock One", "Rock Two"]);
    assert_eq!(
        query::tracks_by_genre(&mut conn, scope, "rock", 2, 1)
            .await
            .unwrap()
            .len(),
        2,
        "the count and offset page the genre listing"
    );

    // The server scope reads the same way: every stored track the view allows,
    // in every folder, which the scope expression has to spell out itself.
    let server = Scope::Server(ServerBrowse::All);
    let counted = query::list_genres(&mut conn, server).await.unwrap();
    let listed: Vec<(&str, i64, i64)> = counted
        .iter()
        .map(|genre| (genre.name.as_str(), genre.song_count, genre.album_count))
        .collect();
    assert_eq!(
        listed,
        [("jazz", 2, 2), ("rock", 3, 2)],
        "the server view counts every stored track"
    );
    assert_eq!(
        query::tracks_by_genre(&mut conn, server, "rock", -1, 0)
            .await
            .unwrap()
            .len(),
        3,
        "the server view lists every stored track of a genre"
    );
    assert_eq!(
        query::random_tracks(&mut conn, server, 10, None, None, None)
            .await
            .unwrap()
            .len(),
        4,
        "the server view samples every stored track"
    );

    // A random sample honours the size, the genre, and the year range.
    let sample = query::random_tracks(&mut conn, scope, 10, None, None, None)
        .await
        .unwrap();
    assert_eq!(sample.len(), 4);
    let sampled = query::random_tracks(&mut conn, scope, 10, Some("jazz"), None, None)
        .await
        .unwrap();
    assert_eq!(sampled.len(), 2);
    let sampled = query::random_tracks(&mut conn, scope, 10, None, Some(1995), Some(2005))
        .await
        .unwrap();
    let mut years: Vec<i64> = sampled
        .iter()
        .map(|view| view.album.as_ref().and_then(|album| album.year).unwrap())
        .collect();
    years.sort_unstable();
    assert_eq!(years, [1999, 2004], "only the albums in the year range");
    assert!(
        query::random_tracks(&mut conn, scope, 0, None, None, None)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The top songs of an artist follow the server's play counts, and an artist
/// is found by name.
#[tokio::test]
async fn top_songs_follow_the_play_counts() {
    let dir = tempfile::tempdir().unwrap();
    let (db, library, user) = album_list_fixture(dir.path()).await;
    let other = insert_user(&db, "other").await;
    let mut conn = db.readers().acquire().await.unwrap();
    let owned_dir = dir.path().join("tops");
    std::fs::create_dir(&owned_dir).unwrap();
    let folder = library
        .add_library_folder(&owned_dir, Some(user))
        .await
        .unwrap();

    let (album, artist) = make_dated_album(&library, "Nova Reyes", "Nightfall", 2001).await;
    let first = make_track(&library, Some(album), "First", Availability::Local).await;
    attach_folder(&library, first, folder.id, "first.mp3").await;
    let second = make_track(&library, Some(album), "Second", Availability::Local).await;
    attach_folder(&library, second, folder.id, "second.mp3").await;
    let third = make_track(&library, Some(album), "Third", Availability::Local).await;
    attach_folder(&library, third, folder.id, "third.mp3").await;
    for (position, track) in [first, second, third].iter().enumerate() {
        credit(&db, *track, artist, position as i64, "main").await;
    }

    // Another artist's song credits the same artist as a featured credit.
    let (guest_album, _) = make_dated_album(&library, "Ida Frost", "Split", 2005).await;
    let guest = make_track(&library, Some(guest_album), "Guest", Availability::Local).await;
    attach_folder(&library, guest, folder.id, "guest.mp3").await;
    sqlx::query(
        "INSERT INTO track_artists (track_id, artist_id, position, role) VALUES (?, ?, 1, 'featured')",
    )
    .bind(guest.uuid().as_bytes().as_slice())
    .bind(artist.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();

    // Every user's plays count: Second twice, Guest once, First once, Third
    // never.
    play(&db, user, second, 1_000).await;
    play(&db, other, second, 2_000).await;
    play(&db, user, guest, 3_000).await;
    play(&db, user, first, 4_000).await;

    let top = query::top_tracks_for_artist(&mut conn, personal(user), artist, 10)
        .await
        .unwrap();
    let titles: Vec<&str> = top.iter().map(|view| view.track.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Second", "First", "Guest", "Third"],
        "the most played song comes first, then the oldest album and the track order"
    );
    assert_eq!(
        query::top_tracks_for_artist(&mut conn, personal(user), artist, 2)
            .await
            .unwrap()
            .len(),
        2,
        "the count bounds the listing"
    );
    let top = query::top_tracks_for_artist(&mut conn, Scope::Server(ServerBrowse::All), artist, 10)
        .await
        .unwrap();
    let titles: Vec<&str> = top.iter().map(|view| view.track.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Second", "First", "Guest", "Third"],
        "the server view ranks the same plays"
    );

    let found = query::find_artist_by_name(&mut conn, "  nova reyes ")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, artist);
    assert!(
        query::find_artist_by_name(&mut conn, "Nobody")
            .await
            .unwrap()
            .is_none()
    );
}

/// Starred entities come back newest first, per user, and the library's
/// last-modified instant follows its rows.
#[tokio::test]
async fn starred_entities_and_the_last_modified_instant() {
    let dir = tempfile::tempdir().unwrap();
    let (db, library, user) = album_list_fixture(dir.path()).await;
    let other = insert_user(&db, "other").await;
    let mut conn = db.readers().acquire().await.unwrap();

    let (first_album, first_artist) = make_album(&library, "Nova Reyes", "Nightfall").await;
    let first_track = make_track(&library, Some(first_album), "First", Availability::Remote).await;
    let (second_album, _) = make_album(&library, "Ida Frost", "Frostwork").await;
    let second_track =
        make_track(&library, Some(second_album), "Second", Availability::Remote).await;
    let before = query::library_last_modified(&mut conn).await.unwrap();
    assert!(before > 0, "the created rows carry an instant");

    // A liked entity is read however it is stored: the unscoped reads report a
    // track the server browse view leaves out, because it has no file yet.
    let server = Scope::Server(ServerBrowse::All);
    assert!(
        query::track(&mut conn, server, first_track)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        query::track_any(&mut conn, first_track)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        query::album_detail(&mut conn, server, first_album)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        query::album_detail_any(&mut conn, first_album)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        query::artist_detail_any(&mut conn, first_artist)
            .await
            .unwrap()
            .is_some()
    );

    // The likes are one instant apart, so the order is not a tie.
    like_at(&db, user, EntityKind::Track, first_track, 100).await;
    like_at(&db, user, EntityKind::Track, second_track, 300).await;
    like_at(&db, user, EntityKind::Album, first_album, 200).await;
    like_at(&db, user, EntityKind::Artist, first_artist, 150).await;
    // Another user's like never appears in this user's listing.
    like_at(&db, other, EntityKind::Album, second_album, 400).await;

    let starred = query::starred(&mut conn, user).await.unwrap();
    assert_eq!(
        starred
            .tracks
            .iter()
            .map(|view| view.track.title.as_str())
            .collect::<Vec<_>>(),
        ["Second", "First"],
        "the most recently liked track comes first"
    );
    assert_eq!(
        starred
            .albums
            .iter()
            .map(|album| album.title.as_str())
            .collect::<Vec<_>>(),
        ["Nightfall"]
    );
    assert_eq!(
        starred
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>(),
        ["Nova Reyes"]
    );
    assert!(
        starred
            .tracks
            .iter()
            .all(|view| view.track.availability == Availability::Remote),
        "a liked track is listed whatever its availability"
    );

    // A scan moves the instant forward.
    let scanned = query::library_last_modified(&mut conn).await.unwrap();
    assert!(scanned >= before);
    let folder_dir = dir.path().join("scan");
    std::fs::create_dir(&folder_dir).unwrap();
    let folder = library
        .add_library_folder(&folder_dir, Some(user))
        .await
        .unwrap();
    sqlx::query("UPDATE library_folders SET last_scan_at = ? WHERE id = ?")
        .bind(scanned + 1_000)
        .bind(folder.id.as_bytes().as_slice())
        .execute(db.writer())
        .await
        .unwrap();
    let mut conn = db.readers().acquire().await.unwrap();
    assert_eq!(
        query::library_last_modified(&mut conn).await.unwrap(),
        scanned + 1_000,
        "the folder's last scan is part of the instant"
    );
}

/// A library built for the order tests: three artists, three albums with
/// distinct years, four tracks with distinct titles, creation instants, and
/// plays, one of them carrying a genre.
struct Ordered {
    /// The migrated database.
    db: Database,
    /// The account whose library the tracks are in.
    user: Uuid,
    /// Another account, whose library is empty.
    other: Uuid,
}

/// Records a play of a track at one instant.
///
/// # Parameters
///
/// - `db`: the database to write to.
/// - `user`: the account that played the track.
/// - `track`: the track played.
/// - `at`: when the play started, in unix milliseconds.
async fn play_at(db: &Database, user: Uuid, track: EntityId, at: i64) {
    sqlx::query(
        "INSERT INTO plays (id, user_id, track_id, started_at, listened_ms, audio_delivered) \
         VALUES (?, ?, ?, ?, 1000, 1)",
    )
    .bind(Uuid::now_v7().as_bytes().as_slice())
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .bind(at)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Moves a track's creation instant.
///
/// # Parameters
///
/// - `db`: the database to write to.
/// - `track`: the track to stamp.
/// - `at`: the instant, in unix milliseconds.
async fn created_at_of_track(db: &Database, track: EntityId, at: i64) {
    sqlx::query("UPDATE tracks SET created_at = ? WHERE id = ?")
        .bind(at)
        .bind(track.uuid().as_bytes().as_slice())
        .execute(db.writer())
        .await
        .unwrap();
}

/// Moves an artist's creation instant.
///
/// # Parameters
///
/// - `db`: the database to write to.
/// - `artist`: the artist to stamp.
/// - `at`: the instant, in unix milliseconds.
async fn created_at_of_artist(db: &Database, artist: EntityId, at: i64) {
    sqlx::query("UPDATE artists SET created_at = ? WHERE id = ?")
        .bind(at)
        .bind(artist.uuid().as_bytes().as_slice())
        .execute(db.writer())
        .await
        .unwrap();
}

/// Inserts a released album with one credited artist.
///
/// # Parameters
///
/// - `library`: the repository to write with.
/// - `artist`: the credited artist's name.
/// - `title`: the album title.
/// - `year`: the release year.
///
/// # Returns
///
/// The album id and the artist id.
async fn released_album(
    library: &LibraryRepository,
    artist: &str,
    title: &str,
    year: i64,
) -> (EntityId, EntityId) {
    let mut album = AlbumInput::new(title, Origin::Local);
    album.year = Some(year);
    let credits = [ArtistCredit::new(artist, "main")];
    let album_id = library.upsert_album(&album, &credits).await.unwrap();
    let artist_id = library.list_album_artists(album_id).await.unwrap()[0].id;
    (album_id, artist_id)
}

/// Inserts a track with its own credit and genres.
///
/// # Parameters
///
/// - `library`: the repository to write with.
/// - `album`: the album the track belongs to.
/// - `artist`: the credited artist's name.
/// - `title`: the track title.
/// - `genres`: the genres the track carries.
///
/// # Returns
///
/// The track id.
async fn credited_track(
    library: &LibraryRepository,
    album: EntityId,
    artist: &str,
    title: &str,
    genres: &[&str],
) -> EntityId {
    let mut input = TrackInput::new(title, Origin::Local, Availability::Local);
    input.album_id = Some(album);
    input.display_artist = Some(artist.to_owned());
    let genres: Vec<String> = genres.iter().map(|genre| (*genre).to_owned()).collect();
    library
        .insert_track(&input, &[ArtistCredit::new(artist, "main")], &genres)
        .await
        .unwrap()
}

/// Builds the library the order tests read.
///
/// The rows are chosen so every order has a different first row:
///
/// | Track | Artist | Album | Year | Created | Played |
/// |---|---|---|---|---|---|
/// | `Aardvark` | Zulu | `Alpha Album` | 2005 | 50 | 100 |
/// | `Bravo` | Mike | `Middle Album` | 2010 | 200 | never |
/// | `Charlie` | Alpha | `Zeta Album` | 1990 | 100 | 900 |
/// | `Delta` | Zulu | `Alpha Album` | 2005 | 300 | 500 |
///
/// `Charlie` carries `rock` and `Bravo` carries `jazz`, so a genre filter keeps
/// one of them.
async fn ordered(dir: &Path) -> Ordered {
    let db = migrated(dir).await;
    let library = LibraryRepository::new(&db);
    let user = insert_user(&db, "listener").await;
    let other = insert_user(&db, "other").await;
    let owned = dir.join("ordered");
    std::fs::create_dir(&owned).unwrap();
    let folder = library
        .add_library_folder(&owned, Some(user))
        .await
        .unwrap();

    let (alpha_album, zulu) = released_album(&library, "Zulu", "Alpha Album", 2005).await;
    let (middle_album, mike) = released_album(&library, "Mike", "Middle Album", 2010).await;
    let (zeta_album, alpha) = released_album(&library, "Alpha", "Zeta Album", 1990).await;

    let aardvark = credited_track(&library, alpha_album, "Zulu", "Aardvark", &[]).await;
    let bravo = credited_track(&library, middle_album, "Mike", "Bravo", &["jazz"]).await;
    let charlie = credited_track(&library, zeta_album, "Alpha", "Charlie", &["rock"]).await;
    let delta = credited_track(&library, alpha_album, "Zulu", "Delta", &[]).await;
    for (track, name) in [
        (aardvark, "aardvark.mp3"),
        (bravo, "bravo.mp3"),
        (charlie, "charlie.mp3"),
        (delta, "delta.mp3"),
    ] {
        attach_folder(&library, track, folder.id, name).await;
    }

    created_at_of_track(&db, aardvark, 50).await;
    created_at_of_track(&db, bravo, 200).await;
    created_at_of_track(&db, charlie, 100).await;
    created_at_of_track(&db, delta, 300).await;

    created_at_of_artist(&db, alpha, 10).await;
    created_at_of_artist(&db, zulu, 20).await;
    created_at_of_artist(&db, mike, 30).await;

    play_at(&db, user, charlie, 900).await;
    play_at(&db, user, delta, 500).await;
    play_at(&db, user, aardvark, 100).await;

    Ordered { db, user, other }
}

/// Saving an album adds its songs to the account's Songs and its artist to its
/// Artists, and liking one does the same; another account's library is
/// untouched.
#[tokio::test]
async fn a_saved_or_liked_album_puts_its_songs_and_artist_in_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();
    let scope = Scope::Personal {
        user_id: seeded.user,
    };
    let stranger = Scope::Personal {
        user_id: seeded.other,
    };

    let generic_track = seeded
        .library
        .get_track(seeded.generic_track)
        .await
        .unwrap()
        .unwrap();
    let generic_album = generic_track.album_id.unwrap();
    let cache_track = seeded
        .library
        .get_track(seeded.cache_track)
        .await
        .unwrap()
        .unwrap();
    let cache_album = cache_track.album_id.unwrap();

    let before = query::count_tracks(&mut conn, scope).await.unwrap();
    let artists_before = query::count_artists(&mut conn, scope).await.unwrap();

    // The album of a generic folder is in nobody's library and its song is in
    // nobody's Songs, which is what the album's own listing already showed.
    assert!(
        query::album_in_scope(&mut conn, scope, generic_album)
            .await
            .unwrap()
            .eq(&false),
        "a generic album is in nobody's library before it is saved"
    );
    assert!(
        query::track(&mut conn, scope, seeded.generic_track)
            .await
            .unwrap()
            .is_none()
    );

    save(&seeded.db, seeded.user, EntityKind::Album, generic_album).await;
    like(&seeded.db, seeded.user, EntityKind::Album, cache_album).await;

    // Both albums now hold the account's songs, so each song is in its Songs.
    for track in [seeded.generic_track, seeded.cache_track] {
        assert!(
            query::track_in_scope(&mut conn, scope, track)
                .await
                .unwrap(),
            "the song of a saved or liked album is in the library"
        );
    }
    let listed: Vec<EntityId> = query::list_tracks(&mut conn, scope, TrackSort::Title, None, -1, 0)
        .await
        .unwrap()
        .iter()
        .map(|view| view.track.id)
        .collect();
    assert!(listed.contains(&seeded.generic_track));
    assert!(listed.contains(&seeded.cache_track));
    assert_eq!(
        query::count_tracks(&mut conn, scope).await.unwrap(),
        before + 2,
        "the count follows the listing"
    );
    assert_eq!(
        query::personal_track_ids(
            &mut conn,
            seeded.user,
            &[seeded.generic_track, seeded.cache_track]
        )
        .await
        .unwrap(),
        HashSet::from([seeded.generic_track, seeded.cache_track])
    );

    // The artist of each album is in the account's Artists, with its album.
    let names: Vec<String> = query::list_artists(&mut conn, scope, ArtistSort::Name, -1, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|summary| summary.artist.name)
        .collect();
    assert!(names.contains(&"Generic Artist".to_owned()));
    assert!(names.contains(&"Cache Artist".to_owned()));
    assert_eq!(
        query::count_artists(&mut conn, scope).await.unwrap(),
        artists_before + 2,
        "the artist of a saved album is an artist of the library"
    );
    assert!(
        query::artist_in_scope(&mut conn, scope, seeded.generic_artist)
            .await
            .unwrap(),
        "the artist of a saved album is in the library"
    );
    let generic = query::artist_detail(&mut conn, scope, seeded.generic_artist)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(generic.albums.len(), 1);
    assert_eq!(generic.albums[0].id, generic_album);

    // A search finds them too, because it searches the same scope.
    let found = query::search(&mut conn, scope, "generic", &[EntityKind::Track], 10, 0)
        .await
        .unwrap();
    assert_eq!(found.tracks.len(), 1);
    assert_eq!(found.tracks[0].track.id, seeded.generic_track);

    // Another account still holds nothing of it.
    for track in [seeded.generic_track, seeded.cache_track] {
        assert!(
            !query::track_in_scope(&mut conn, stranger, track)
                .await
                .unwrap(),
            "a save belongs to the account that made it"
        );
    }
    assert!(
        query::list_tracks(&mut conn, stranger, TrackSort::Title, None, -1, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !query::artist_in_scope(&mut conn, stranger, seeded.generic_artist)
            .await
            .unwrap()
    );
    assert_eq!(query::count_artists(&mut conn, stranger).await.unwrap(), 0);
}

/// Every song order lists the whole scope in its own first row, and the count
/// of the scope does not depend on the order.
#[tokio::test]
async fn song_orders_start_with_the_row_their_order_names() {
    let dir = tempfile::tempdir().unwrap();
    let ordered = ordered(dir.path()).await;
    let mut conn = ordered.db.readers().acquire().await.unwrap();
    let scope = Scope::Personal {
        user_id: ordered.user,
    };

    let mut firsts = Vec::new();
    for (sort, expected) in [
        (TrackSort::Title, "Aardvark"),
        (TrackSort::Added, "Delta"),
        (TrackSort::Played { user: ordered.user }, "Charlie"),
        (TrackSort::Artist, "Charlie"),
        (TrackSort::Album, "Aardvark"),
        (TrackSort::Year, "Bravo"),
    ] {
        let listed = query::list_tracks(&mut conn, scope, sort, None, -1, 0)
            .await
            .unwrap();
        assert_eq!(listed.len(), 4, "{sort:?} lists the whole scope");
        assert_eq!(
            listed[0].track.title, expected,
            "{sort:?} starts at {expected}"
        );
        firsts.push(listed[0].track.title.clone());
        assert_eq!(
            query::count_tracks(&mut conn, scope).await.unwrap(),
            4,
            "{sort:?} does not change the count"
        );
    }
    assert_eq!(
        firsts,
        [
            "Aardvark", "Delta", "Charlie", "Charlie", "Aardvark", "Bravo"
        ],
        "the orders are not the same order"
    );

    // The album order keeps the tracks with an album before the album-less
    // ones, and the year order lists the newest release first.
    let by_album: Vec<String> = query::list_tracks(&mut conn, scope, TrackSort::Album, None, -1, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|view| view.track.title)
        .collect();
    assert_eq!(by_album, ["Aardvark", "Delta", "Bravo", "Charlie"]);
    let by_year: Vec<String> = query::list_tracks(&mut conn, scope, TrackSort::Year, None, -1, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|view| view.track.title)
        .collect();
    assert_eq!(by_year, ["Bravo", "Aardvark", "Delta", "Charlie"]);

    // The genre filter narrows the listing to the songs that carry it, and the
    // order still applies.
    let rock: Vec<String> =
        query::list_tracks(&mut conn, scope, TrackSort::Title, Some("Rock"), -1, 0)
            .await
            .unwrap()
            .into_iter()
            .map(|view| view.track.title)
            .collect();
    assert_eq!(rock, ["Charlie"], "the genre name folds to the stored form");
    let jazz: Vec<String> =
        query::list_tracks(&mut conn, scope, TrackSort::Title, Some("jazz"), -1, 0)
            .await
            .unwrap()
            .into_iter()
            .map(|view| view.track.title)
            .collect();
    assert_eq!(jazz, ["Bravo"]);
    assert!(
        query::list_tracks(&mut conn, scope, TrackSort::Title, Some("polka"), -1, 0)
            .await
            .unwrap()
            .is_empty()
    );

    // Paging walks an order without repeats.
    let page_one = query::list_tracks(&mut conn, scope, TrackSort::Year, None, 2, 0)
        .await
        .unwrap();
    let page_two = query::list_tracks(&mut conn, scope, TrackSort::Year, None, 2, 2)
        .await
        .unwrap();
    assert!(page_two.iter().all(|view| !page_one.contains(view)));

    // Another account's library holds none of it, whatever the order.
    let stranger = Scope::Personal {
        user_id: ordered.other,
    };
    assert!(
        query::list_tracks(&mut conn, stranger, TrackSort::Year, None, -1, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Every artist order lists the whole scope in its own first row.
#[tokio::test]
async fn artist_orders_start_with_the_artist_their_order_names() {
    let dir = tempfile::tempdir().unwrap();
    let ordered = ordered(dir.path()).await;
    let mut conn = ordered.db.readers().acquire().await.unwrap();
    let scope = Scope::Personal {
        user_id: ordered.user,
    };

    let mut firsts = Vec::new();
    for (sort, expected) in [
        (ArtistSort::Name, "Alpha"),
        (ArtistSort::Added, "Mike"),
        (ArtistSort::Played { user: ordered.user }, "Zulu"),
    ] {
        let listed = query::list_artists(&mut conn, scope, sort, -1, 0)
            .await
            .unwrap();
        assert_eq!(listed.len(), 3, "{sort:?} lists the whole scope");
        assert_eq!(
            listed[0].artist.name, expected,
            "{sort:?} starts at {expected}"
        );
        firsts.push(listed[0].artist.name.clone());
        assert_eq!(
            query::count_artists(&mut conn, scope).await.unwrap(),
            3,
            "{sort:?} does not change the count"
        );
    }
    assert_eq!(firsts, ["Alpha", "Mike", "Zulu"]);

    let by_name: Vec<String> = query::list_artists(&mut conn, scope, ArtistSort::Name, -1, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|summary| summary.artist.name)
        .collect();
    assert_eq!(by_name, ["Alpha", "Mike", "Zulu"]);
    let by_added: Vec<String> = query::list_artists(&mut conn, scope, ArtistSort::Added, -1, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|summary| summary.artist.name)
        .collect();
    assert_eq!(by_added, ["Mike", "Zulu", "Alpha"]);
    let by_played: Vec<String> = query::list_artists(
        &mut conn,
        scope,
        ArtistSort::Played { user: ordered.user },
        -1,
        0,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|summary| summary.artist.name)
    .collect();
    assert_eq!(by_played, ["Zulu", "Alpha", "Mike"]);
}
