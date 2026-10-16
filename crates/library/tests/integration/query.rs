//! Tests for the browse and search queries: personal library membership, the
//! server scope and its privacy setting, album ordering, local full-text
//! search, and the preferred file resolution.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_config::{SettingValue, SettingsRegistry, SettingsStore};
use hearthsound_core::{Availability, EntityId, EntityKind, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::query::{
    self, AlbumSort, Scope, ServerBrowse, content_type_for, sanitize_match,
};
use hearthsound_library::{
    Album, AlbumInput, ArtistCredit, LibraryRepository, ManagedMediaFile, Origin, ScannedMediaFile,
    StorageRoots, TrackInput,
};

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
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) VALUES (?, ?, ?, 'like', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind_literal(kind))
    .bind(id.uuid().as_bytes().as_slice())
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

    let artists = query::list_artists(&mut conn, scope, -1, 0).await.unwrap();
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
        query::list_artists(&mut conn, stranger, -1, 0)
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

    let listed = query::list_tracks(&mut conn, scope, -1, 0).await.unwrap();
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
    let page_one = query::list_tracks(&mut conn, scope, 2, 0).await.unwrap();
    let page_two = query::list_tracks(&mut conn, scope, 2, 2).await.unwrap();
    assert_eq!(page_one.len(), 2);
    assert!(page_two.iter().all(|view| !page_one.contains(view)));

    // The permanent server scope adds the permanent library track, not the
    // evictable cache one, and the disabled view offers nothing.
    let permanent = Scope::Server(ServerBrowse::PermanentOnly);
    let listed = query::list_tracks(&mut conn, permanent, -1, 0)
        .await
        .unwrap();
    let titles: Vec<&str> = listed
        .iter()
        .map(|view| view.track.title.as_str())
        .collect();
    assert!(titles.contains(&"Library Song"));
    assert!(!titles.contains(&"Cache Song"));

    let disabled = Scope::Server(ServerBrowse::Disabled);
    assert!(
        query::list_tracks(&mut conn, disabled, -1, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_server_scope_follows_the_privacy_setting() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();

    // permanent_only hides the evictable cache track and its album and artist.
    let permanent = Scope::Server(ServerBrowse::PermanentOnly);
    assert!(
        query::track(&mut conn, permanent, seeded.cache_track)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        query::track(&mut conn, permanent, seeded.library_track)
            .await
            .unwrap()
            .is_some()
    );
    let albums = query::list_albums(&mut conn, permanent, AlbumSort::Newest, -1, 0)
        .await
        .unwrap();
    assert!(albums.iter().all(|album| album.title != "Cache Album"));
    assert!(albums.iter().any(|album| album.title == "Library Album"));

    // all includes it.
    let everything = Scope::Server(ServerBrowse::All);
    assert!(
        query::track(&mut conn, everything, seeded.cache_track)
            .await
            .unwrap()
            .is_some()
    );
    let albums = query::list_albums(&mut conn, everything, AlbumSort::Newest, -1, 0)
        .await
        .unwrap();
    assert!(albums.iter().any(|album| album.title == "Cache Album"));

    // disabled offers nothing at all.
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
        query::list_artists(&mut conn, disabled, -1, 0)
            .await
            .unwrap()
            .is_empty()
    );

    // The setting maps onto the view.
    let settings = SettingsStore::defaults(&SettingsRegistry::builtin());
    assert_eq!(
        Scope::server_from_settings(&settings).unwrap(),
        Scope::Server(ServerBrowse::PermanentOnly)
    );
    let mut sources = hearthsound_config::SettingSources::default();
    sources.database.insert(
        "privacy.server_browse".to_owned(),
        SettingValue::String("all".to_owned()),
    );
    let settings = SettingsStore::resolve(&SettingsRegistry::builtin(), &sources).unwrap();
    assert_eq!(
        Scope::server_from_settings(&settings).unwrap(),
        Scope::Server(ServerBrowse::All)
    );
    assert_eq!(ServerBrowse::Disabled.as_str(), "disabled");
    assert!(matches!(
        settings.value("privacy.server_browse"),
        Some(SettingValue::String(value)) if value == "all"
    ));
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
            ["Album A", "Album B", "Album C"],
            "the highest rating comes first and an unrated album last",
        ),
        (
            AlbumSort::Frequent { user },
            ["Album A", "Album B", "Album C"],
            "the most played album comes first",
        ),
        (
            AlbumSort::Recent { user },
            ["Album B", "Album A", "Album C"],
            "the most recently played album comes first",
        ),
        (
            AlbumSort::Starred { user },
            ["Album B", "Album A", "Album C"],
            "the liked album comes first",
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
