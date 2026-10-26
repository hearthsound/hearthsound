//! The test harness the catalog tests share.
//!
//! Every test builds a real plugin host, provider registry, instance service,
//! settings service and database in a temporary directory, installs fixture
//! plugins, and drives the service under test the way the API layer will.
//! Nothing here reaches the network: `hello-catalog` and `spin-catalog` answer
//! in process, and `http-catalog` talks to a local wiremock server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hearthsound_catalog::CatalogService;
use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry};
use hearthsound_core::providers::CatalogQuery;
use hearthsound_core::{CatalogId, EntityId, EntityKind, Layer, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{
    InstallError, InstanceService, InstanceSpec, PluginHost, PluginRepository, PluginSource,
    ProviderRegistry,
};
use hearthsound_test_support::fixture_plugin;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The plugin id of the hello catalog fixture.
pub const HELLO_CATALOG_ID: &str = "com.example.hello-catalog";

/// The plugin id of the HTTP catalog fixture.
pub const HTTP_CATALOG_ID: &str = "com.example.http-catalog";

/// The plugin id of the spin catalog fixture.
pub const SPIN_CATALOG_ID: &str = "com.example.spin-catalog";

/// The plugin id of the metadata fixture.
pub const METADATA_ID: &str = "com.example.metadata-fixture";

/// Opens a migrated temporary database for a test.
pub async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

/// One test's host, database, settings and provider registry.
pub struct Env {
    /// Kept alive so the plugin directory outlives the host.
    _dir: tempfile::TempDir,
    /// The artwork cache directory the service under test writes to.
    artwork: std::path::PathBuf,
    /// The plugin host the fixtures are installed into.
    pub host: Arc<PluginHost>,
    /// The plugin tables.
    pub repository: PluginRepository,
    /// The secret box instance configurations are sealed with.
    pub secrets: SecretBox,
    /// The settings service the service under test reads.
    pub settings: Settings,
    /// The migrated database.
    pub database: Arc<Database>,
    /// The provider registry the service resolves instances through.
    pub registry: Arc<ProviderRegistry>,
}

impl Env {
    /// Builds an environment over a fresh temporary directory.
    pub async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let database = database(dir.path()).await;
        let settings = Settings::load(
            database.writer().clone(),
            SettingsRegistry::builtin(),
            SettingSources::default(),
        )
        .await
        .unwrap();
        let host = Arc::new(
            PluginHost::new(
                dir.path().join("plugins"),
                settings.current(),
                Arc::clone(&database),
            )
            .unwrap(),
        );
        let secrets = SecretKey::generate().secret_box();
        let repository = PluginRepository::new(&database);
        let registry = Arc::new(ProviderRegistry::new(
            Arc::clone(&host),
            repository.clone(),
            secrets.clone(),
        ));
        let artwork = dir.path().join("artwork");
        Env {
            _dir: dir,
            artwork,
            host,
            repository,
            secrets,
            settings,
            database,
            registry,
        }
    }

    /// Builds the service under test.
    pub fn service(&self) -> CatalogService {
        CatalogService::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.database),
            self.settings.clone(),
            self.artwork.clone(),
        )
    }

    /// Returns the directory cached cover images are written to.
    pub fn artwork_dir(&self) -> &std::path::Path {
        &self.artwork
    }

    /// Lists the file names of the artwork cache directory.
    pub fn artwork_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.artwork)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Builds the instance service over this environment.
    pub fn instances(&self) -> InstanceService {
        InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        )
    }

    /// Installs a fixture plugin, returning its plugin id.
    ///
    /// Installing a plugin that is already registered is a no-op, so an
    /// environment can hold two instances of one fixture.
    pub async fn install(&self, archive: &str) -> String {
        match self.host.install(&fixture_plugin(archive)).await {
            Ok(installed) => {
                self.repository
                    .register(&installed, PluginSource::Sideload)
                    .await
                    .unwrap()
                    .id
            }
            Err(InstallError::AlreadyInstalled { id, .. }) => id,
            Err(error) => panic!("installing {archive} failed: {error}"),
        }
    }

    /// Installs a fixture plugin and configures one instance of it.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    /// - `archive`: the fixture archive file name.
    /// - `plugin_id`: the fixture's plugin id.
    /// - `config`: the instance's server-scoped configuration.
    pub async fn instance(&self, slug: &str, archive: &str, plugin_id: &str, config: Value) {
        self.install(archive).await;
        self.instances()
            .create(&InstanceSpec {
                slug: slug.to_owned(),
                plugin_id: plugin_id.to_owned(),
                display_name: format!("{slug} display"),
                server_config: config,
                host_config: Value::Null,
            })
            .await
            .unwrap();
    }

    /// Assigns the given instances to a layer, in order, and rebuilds.
    pub async fn assign(&self, layer: Layer, slugs: &[&str]) {
        let slugs: Vec<String> = slugs.iter().map(|slug| (*slug).to_owned()).collect();
        self.instances().set_layer(layer, &slugs).await.unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Writes a setting and republishes the snapshot.
    pub async fn set(&self, key: &str, value: SettingValue) {
        self.settings.set(key, value, None).await.unwrap();
    }

    /// Stores an album with its artist and returns the album's id.
    ///
    /// The row is what a scan or a materialization would write: the album
    /// carries the identifiers an artwork provider matches on, and the artist is
    /// credited first.
    ///
    /// # Parameters
    ///
    /// - `title`: the album title.
    /// - `artist`: the artist name.
    ///
    /// # Returns
    ///
    /// The stored album's client id.
    pub async fn stored_album(&self, title: &str, artist: &str) -> EntityId {
        use hearthsound_library::{
            AlbumInput, ArtistCredit, ArtistInput, LibraryRepository, Origin,
        };
        let library = LibraryRepository::new(&self.database);
        library
            .upsert_album(
                &AlbumInput {
                    title: title.to_owned(),
                    sort_title: None,
                    display_artist: None,
                    year: Some(2026),
                    release_date: None,
                    album_type: None,
                    mb_release_id: Some("22222222-2222-4222-8222-222222222222".to_owned()),
                    mb_release_group_id: None,
                    upc: None,
                    origin: Origin::Catalog,
                },
                &[ArtistCredit {
                    artist: ArtistInput {
                        name: artist.to_owned(),
                        sort_name: None,
                        mb_artist_id: Some("44444444-4444-4444-8444-444444444444".to_owned()),
                        origin: Origin::Catalog,
                    },
                    role: "main".to_owned(),
                }],
            )
            .await
            .unwrap()
    }

    /// Stores an artist and returns its id.
    ///
    /// # Parameters
    ///
    /// - `name`: the artist name.
    ///
    /// # Returns
    ///
    /// The stored artist's client id.
    pub async fn stored_artist(&self, name: &str) -> EntityId {
        use hearthsound_library::{ArtistInput, LibraryRepository, Origin};
        LibraryRepository::new(&self.database)
            .upsert_artist(&ArtistInput {
                name: name.to_owned(),
                sort_name: None,
                mb_artist_id: Some("44444444-4444-4444-8444-444444444444".to_owned()),
                origin: Origin::Catalog,
            })
            .await
            .unwrap()
    }

    /// Stores an album with one available track and returns the album, the
    /// track and the credited artist.
    ///
    /// The track carries a managed file, so the artist, the album and the track
    /// are in every server scope: a page built over these rows answers from the
    /// library instead of degrading to nothing.
    ///
    /// # Parameters
    ///
    /// - `title`: the album title.
    /// - `artist`: the artist name.
    /// - `year`: the release year.
    ///
    /// # Returns
    ///
    /// The album, the track and the artist ids, in that order.
    pub async fn stored_album_with_track(
        &self,
        title: &str,
        artist: &str,
        year: i64,
    ) -> (EntityId, EntityId, EntityId) {
        use hearthsound_core::{Availability, StorageClass};
        use hearthsound_library::{
            AlbumInput, ArtistCredit, LibraryRepository, ManagedMediaFile, Origin, TrackInput,
        };
        let library = LibraryRepository::new(&self.database);
        let mut input = AlbumInput::new(title, Origin::Local);
        input.year = Some(year);
        let album = library
            .upsert_album(&input, &[ArtistCredit::new(artist, "main")])
            .await
            .unwrap();
        let credited = library.list_album_artists(album).await.unwrap();
        let artist_id = credited.first().unwrap().id;
        let mut track =
            TrackInput::new(format!("{title} Track"), Origin::Local, Availability::Local);
        track.album_id = Some(album);
        track.duration_ms = Some(180_000);
        let track = library.insert_track(&track, &[], &[]).await.unwrap();
        library
            .insert_managed_media_file(&ManagedMediaFile {
                track_id: track,
                storage: StorageClass::Library,
                rel_path: format!("{artist}/{title}/01.flac"),
                size: 1_000,
                mtime_ms: 1,
                format: Some("flac".to_owned()),
                codec: Some("flac".to_owned()),
                bitrate_kbps: Some(900),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                channels: Some(2),
                duration_ms: Some(180_000),
                lossless: Some(true),
                quality_tier: None,
                replaygain: None,
                has_embedded_art: Some(false),
                uploaded_by: None,
                acquisition_id: None,
            })
            .await
            .unwrap();
        (album, track, artist_id)
    }

    /// Returns the id of a configured instance.
    pub fn instance_id(&self, slug: &str) -> uuid::Uuid {
        self.registry.instance(slug).unwrap().id()
    }
}

/// Builds a catalog search query for one text.
pub fn query(text: &str) -> CatalogQuery {
    CatalogQuery {
        text: text.to_owned(),
        kinds: vec![EntityKind::Track, EntityKind::Album, EntityKind::Artist],
        limit: 10,
        offset: 0,
    }
}

/// Builds a catalog id for a slug and provider id.
pub fn catalog_id(slug: &str, kind: EntityKind, provider_id: &str) -> CatalogId {
    CatalogId::new(Slug::new(slug).unwrap(), kind, provider_id).unwrap()
}

/// Serves one JSON search response from a wiremock endpoint.
///
/// # Parameters
///
/// - `status`: the HTTP status to answer with.
/// - `body`: the body to answer with, for a success status.
/// - `delay`: how long the endpoint waits before answering.
///
/// # Returns
///
/// The server, kept alive by the caller, and the URL to configure an instance
/// with.
pub async fn catalog_endpoint(
    status: u16,
    body: &str,
    delay: Option<Duration>,
) -> (MockServer, String) {
    let server = MockServer::start().await;
    let mut template = ResponseTemplate::new(status);
    if status < 300 {
        template = template.set_body_string(body);
    }
    if let Some(delay) = delay {
        template = template.set_delay(delay);
    }
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(template)
        .mount(&server)
        .await;
    let url = format!("{}/search", server.uri());
    (server, url)
}

/// Builds the server configuration of an `http-catalog` instance.
///
/// # Parameters
///
/// - `url`: the endpoint the fixture searches.
pub fn http_config(url: &str) -> Value {
    json!({ "url": url })
}

/// Builds the server configuration of a metadata fixture instance.
///
/// # Parameters
///
/// - `recordings`: the recordings the fixture knows, as JSON objects with
///   `title`, `mbid`, an optional `isrc`, an optional `score`, and optional
///   `releases`.
pub fn metadata_config(recordings: Value) -> Value {
    json!({ "recordings": recordings })
}

/// Writes one row of the catalog cache directly.
///
/// # Parameters
///
/// - `database`: the database to write.
/// - `instance`: the instance id part of the key.
/// - `request_key`: the canonical request key.
/// - `response`: the stored response JSON.
/// - `expires_at`: the instant the row stops being fresh, in unix milliseconds.
pub async fn write_cache_row(
    database: &Database,
    instance: uuid::Uuid,
    request_key: &str,
    response: &str,
    expires_at: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO catalog_cache (instance_id, request_key, response, fetched_at, expires_at) \
         VALUES (?, ?, ?, 0, ?)",
    )
    .bind(instance.as_bytes().as_slice())
    .bind(request_key)
    .bind(response)
    .bind(expires_at)
    .execute(database.writer())
    .await
    .unwrap();
}

/// Reads the lifetime a stored cache row was given.
///
/// # Returns
///
/// The row's `expires_at - fetched_at` in milliseconds, or `None` when the row
/// does not exist or has no expiry.
pub async fn cache_row_lifetime(
    database: &Database,
    instance: uuid::Uuid,
    request_key: &str,
) -> Option<i64> {
    let row: (i64, Option<i64>) = sqlx::query_as(
        "SELECT fetched_at, expires_at FROM catalog_cache \
         WHERE instance_id = ? AND request_key = ?",
    )
    .bind(instance.as_bytes().as_slice())
    .bind(request_key)
    .fetch_optional(database.readers())
    .await
    .unwrap()?;
    Some(row.1? - row.0)
}

/// Counts the rows of the catalog cache.
pub async fn count_cache_rows(database: &Database) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM catalog_cache")
        .fetch_one(database.readers())
        .await
        .unwrap()
}

/// Returns the current time in unix milliseconds.
pub fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}

/// Inserts a stored track row directly, for a test that needs a local hit.
///
/// # Parameters
///
/// - `database`: the database to write.
/// - `title`: the track title.
/// - `isrc`: the track's ISRC, when it has one.
/// - `duration_ms`: the track duration in milliseconds, when known.
///
/// # Returns
///
/// The client id of the inserted row.
pub async fn insert_local_track(
    database: &Database,
    title: &str,
    isrc: Option<&str>,
    duration_ms: Option<i64>,
) -> hearthsound_core::EntityId {
    let id = uuid::Uuid::now_v7();
    let now = now_millis();
    sqlx::query(
        "INSERT INTO tracks (id, album_id, title, sort_title, display_artist, disc_number, \
                             track_number, duration_ms, isrc, mb_recording_id, explicit, origin, \
                             availability, unavailable_until, created_at, updated_at) \
         VALUES (?, NULL, ?, ?, NULL, NULL, NULL, ?, ?, NULL, NULL, 'local', 'local', NULL, ?, ?)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(title)
    .bind(title.to_lowercase())
    .bind(duration_ms)
    .bind(isrc)
    .bind(now)
    .bind(now)
    .execute(database.writer())
    .await
    .unwrap();
    hearthsound_core::EntityId::from_uuid(hearthsound_core::IdPrefix::Track, id)
}

/// Inserts a stored album row directly, for a test that needs a stored entity.
///
/// # Parameters
///
/// - `database`: the database to write.
/// - `title`: the album title.
///
/// # Returns
///
/// The client id of the inserted row.
pub async fn insert_local_album(database: &Database, title: &str) -> hearthsound_core::EntityId {
    let id = uuid::Uuid::now_v7();
    let now = now_millis();
    sqlx::query(
        "INSERT INTO albums (id, title, sort_title, display_artist, year, release_date, \
                             album_type, mb_release_id, mb_release_group_id, upc, origin, \
                             created_at, updated_at) \
         VALUES (?, ?, ?, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'local', ?, ?)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(title)
    .bind(title.to_lowercase())
    .bind(now)
    .bind(now)
    .execute(database.writer())
    .await
    .unwrap();
    hearthsound_core::EntityId::from_uuid(hearthsound_core::IdPrefix::Album, id)
}

/// Writes one row of the alias table directly.
///
/// # Parameters
///
/// - `database`: the database to write.
/// - `namespace`: the alias namespace, for example `p:hello`.
/// - `kind`: the entity kind the alias names.
/// - `value`: the alias value.
/// - `entity`: the row the alias points at.
pub async fn write_alias(
    database: &Database,
    namespace: &str,
    kind: hearthsound_core::EntityKind,
    value: &str,
    entity: hearthsound_core::EntityId,
) {
    let kind = entity_kind_literal(kind);
    sqlx::query(
        "INSERT INTO external_ids (namespace, value, entity_kind, entity_id) VALUES (?, ?, ?, ?)",
    )
    .bind(namespace)
    .bind(value)
    .bind(kind)
    .bind(entity.uuid().as_bytes().as_slice())
    .execute(database.writer())
    .await
    .unwrap();
}

/// Returns the literal an entity kind has in the `entity_kind` column.
///
/// # Parameters
///
/// - `kind`: the entity kind.
pub fn entity_kind_literal(kind: hearthsound_core::EntityKind) -> &'static str {
    match kind {
        hearthsound_core::EntityKind::Track => "track",
        hearthsound_core::EntityKind::Album => "album",
        hearthsound_core::EntityKind::Artist => "artist",
        hearthsound_core::EntityKind::Playlist => "playlist",
    }
}

/// Returns the stored row an alias names.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `namespace`: the alias namespace.
/// - `kind`: the entity kind the alias names.
/// - `value`: the alias value.
pub async fn alias_target(
    database: &Database,
    namespace: &str,
    kind: hearthsound_core::EntityKind,
    value: &str,
) -> Option<Vec<u8>> {
    sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT entity_id FROM external_ids \
         WHERE namespace = ? AND entity_kind = ? AND value = ?",
    )
    .bind(namespace)
    .bind(entity_kind_literal(kind))
    .bind(value)
    .fetch_optional(database.readers())
    .await
    .unwrap()
}

/// Counts the rows of one table.
///
/// The statement per table is a literal, because a query string sqlx accepts
/// has to be one.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `table`: the table name, one of the names the harness knows.
///
/// # Panics
///
/// Panics when the table is not one the harness knows, which is a test bug.
pub async fn count_rows(database: &Database, table: &str) -> i64 {
    let sql = match table {
        "tracks" => "SELECT COUNT(*) FROM tracks",
        "albums" => "SELECT COUNT(*) FROM albums",
        "artists" => "SELECT COUNT(*) FROM artists",
        "external_ids" => "SELECT COUNT(*) FROM external_ids",
        "catalog_cache" => "SELECT COUNT(*) FROM catalog_cache",
        "artwork_cache" => "SELECT COUNT(*) FROM artwork_cache",
        other => panic!("the harness does not know the table `{other}`"),
    };
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(database.readers())
        .await
        .unwrap()
}

/// One stored track row, as the materialization tests read it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTrackRow {
    /// The album the track belongs to, as raw bytes.
    pub album_id: Option<Vec<u8>>,
    /// The track title.
    pub title: String,
    /// The track number within its disc.
    pub track_number: Option<i64>,
    /// Where the row comes from.
    pub origin: String,
    /// The track's availability.
    pub availability: String,
    /// The International Standard Recording Code.
    pub isrc: Option<String>,
    /// The MusicBrainz recording id.
    pub mb_recording_id: Option<String>,
}

/// Reads one stored track row.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `id`: the track's raw primary key.
pub async fn stored_track(database: &Database, id: uuid::Uuid) -> Option<StoredTrackRow> {
    sqlx::query_as::<
        _,
        (
            Option<Vec<u8>>,
            String,
            Option<i64>,
            String,
            String,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT album_id, title, track_number, origin, availability, isrc, mb_recording_id \
         FROM tracks WHERE id = ?",
    )
    .bind(id.as_bytes().as_slice())
    .fetch_optional(database.readers())
    .await
    .unwrap()
    .map(
        |(album_id, title, track_number, origin, availability, isrc, mb_recording_id)| {
            StoredTrackRow {
                album_id,
                title,
                track_number,
                origin,
                availability,
                isrc,
                mb_recording_id,
            }
        },
    )
}

/// Reads the album artist, the title and the year of one stored album.
///
/// These are the three album values the import naming template renders from.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `id`: the album's raw primary key.
///
/// # Returns
///
/// The album's title, display artist and year, or `None` when no such row
/// exists.
pub async fn stored_album_fields(
    database: &Database,
    id: uuid::Uuid,
) -> Option<(String, Option<String>, Option<i64>)> {
    sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
        "SELECT title, display_artist, year FROM albums WHERE id = ?",
    )
    .bind(id.as_bytes().as_slice())
    .fetch_optional(database.readers())
    .await
    .unwrap()
}

/// Reads the artist ids one stored album credits, in credit order.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `album_id`: the album's raw primary key.
pub async fn album_artist_ids(database: &Database, album_id: uuid::Uuid) -> Vec<Vec<u8>> {
    sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT artist_id FROM album_artists WHERE album_id = ? ORDER BY position",
    )
    .bind(album_id.as_bytes().as_slice())
    .fetch_all(database.readers())
    .await
    .unwrap()
}

/// Reads the origin and the MusicBrainz columns of one stored album.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `id`: the album's raw primary key.
pub async fn stored_album(
    database: &Database,
    id: uuid::Uuid,
) -> Option<(String, Option<String>, Option<String>, Option<String>)> {
    sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
        "SELECT origin, mb_release_id, mb_release_group_id, upc FROM albums WHERE id = ?",
    )
    .bind(id.as_bytes().as_slice())
    .fetch_optional(database.readers())
    .await
    .unwrap()
}

/// Reads the origin and the MusicBrainz id of one stored artist.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `id`: the artist's raw primary key.
pub async fn stored_artist(
    database: &Database,
    id: uuid::Uuid,
) -> Option<(String, Option<String>)> {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT origin, mb_artist_id FROM artists WHERE id = ?",
    )
    .bind(id.as_bytes().as_slice())
    .fetch_optional(database.readers())
    .await
    .unwrap()
}

/// Reads the artist ids one stored track credits, in credit order.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `track_id`: the track's raw primary key.
pub async fn track_artist_ids(database: &Database, track_id: uuid::Uuid) -> Vec<Vec<u8>> {
    sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT artist_id FROM track_artists WHERE track_id = ? ORDER BY position",
    )
    .bind(track_id.as_bytes().as_slice())
    .fetch_all(database.readers())
    .await
    .unwrap()
}

/// Reads one stored artwork cache row.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `key`: the cache key.
pub async fn stored_artwork(
    database: &Database,
    key: &str,
) -> Option<(String, String, String, Option<i64>, Option<i64>)> {
    sqlx::query_as::<_, (String, String, String, Option<i64>, Option<i64>)>(
        "SELECT source, path, content_type, width, height FROM artwork_cache WHERE key = ?",
    )
    .bind(key)
    .fetch_optional(database.readers())
    .await
    .unwrap()
}

/// Inserts a stored recording with an album and a display artist.
///
/// A scanned file carries its album and artist in the rows a materialization
/// compares against, so a test that exercises the title-and-artist rule needs
/// them.
///
/// # Parameters
///
/// - `database`: the database to write.
/// - `title`: the track title.
/// - `artist`: the display artist.
/// - `album`: the album title.
/// - `duration_ms`: the track duration.
/// - `mb_recording_id`: the MusicBrainz recording id, when known.
/// - `isrc`: the ISRC, when known.
///
/// # Returns
///
/// The client id of the inserted track.
pub async fn insert_stored_recording(
    database: &Database,
    title: &str,
    artist: &str,
    album: &str,
    duration_ms: i64,
    mb_recording_id: Option<&str>,
    isrc: Option<&str>,
) -> hearthsound_core::EntityId {
    let album_id = insert_local_album(database, album).await;
    let id = uuid::Uuid::now_v7();
    let now = now_millis();
    sqlx::query(
        "INSERT INTO tracks (id, album_id, title, sort_title, display_artist, disc_number, \
                             track_number, duration_ms, isrc, mb_recording_id, explicit, origin, \
                             availability, unavailable_until, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, 1, 1, ?, ?, ?, 0, 'local', 'local', NULL, ?, ?)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(album_id.uuid().as_bytes().as_slice())
    .bind(title)
    .bind(title.to_lowercase())
    .bind(artist)
    .bind(duration_ms)
    .bind(isrc)
    .bind(mb_recording_id)
    .bind(now)
    .bind(now)
    .execute(database.writer())
    .await
    .unwrap();
    hearthsound_core::EntityId::from_uuid(hearthsound_core::IdPrefix::Track, id)
}
