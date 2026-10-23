//! The library the browse endpoint tests read.
//!
//! One call to [`library`] inserts a small dataset that exercises every
//! behaviour the browse endpoints have:
//!
//! | Album | Artist | File | Scope |
//! |---|---|---|---|
//! | `Alpha` | `The Cardinals` | folder files in the account's own folder, with embedded art | the personal library and the server |
//! | `Bright` | `2 Mello` | a folder file in a generic folder, without art | the server only |
//! | `Cache` | `Charlotte` | a managed file in the evictable cache | the server view, for an administrator |
//! | `Delta` | `El Mar` | a folder file in a generic folder, without art | the server only |
//!
//! The artists are chosen so the index covers an article (`The Cardinals` files
//! under `C`), a name that starts with a digit (`2 Mello` files under `#`), and
//! a second article (`El Mar` files under `M`).
//!
//! The rows are written through the library crate's own write functions, so the
//! tests exercise the same rows a scan or an import would produce.

use std::path::Path;

use hearthsound_core::{Availability, EntityId, StorageClass};
use hearthsound_db::Database;
use hearthsound_library::{
    AlbumInput, ArtistCredit, LibraryFolder, LibraryRepository, ManagedMediaFile, Origin,
    ReplayGain, ScannedMediaFile, TrackInput,
};

/// The ids of the seeded entities.
pub struct Seeded {
    /// The artist of the owned folder's album.
    pub cardinals: EntityId,
    /// The album whose files are in the account's own folder.
    pub alpha: EntityId,
    /// The two tracks of [`Seeded::alpha`], in playing order.
    pub alpha_tracks: Vec<EntityId>,
    /// The artist of the generic folder's album.
    pub mello: EntityId,
    /// The album whose files are in a generic folder.
    pub bright: EntityId,
    /// The only track of [`Seeded::bright`].
    pub bright_track: EntityId,
    /// The artist whose only file is in the evictable cache.
    pub charlotte: EntityId,
    /// The album whose only file is in the evictable cache.
    pub cache_album: EntityId,
    /// The only track of [`Seeded::cache_album`].
    pub cache_track: EntityId,
    /// The artist whose name begins with an article.
    pub elmar: EntityId,
    /// The album with an article in its artist's name.
    pub delta: EntityId,
    /// The only track of [`Seeded::delta`].
    pub delta_track: EntityId,
}

/// Inserts the dataset and returns the ids a test refers to.
///
/// # Parameters
///
/// - `database`: the migrated database to write to.
/// - `owned`: the directory of the library folder owned by `owner`; it must
///   exist.
/// - `generic`: the directory of a generic library folder; it must exist.
/// - `owner`: the account the owned folder belongs to.
///
/// # Returns
///
/// The ids of the seeded entities.
pub async fn library(database: &Database, owned: &Path, generic: &Path, owner: EntityId) -> Seeded {
    let library = LibraryRepository::new(database);
    let owned = library
        .add_library_folder(owned, Some(owner.uuid()))
        .await
        .unwrap();
    let generic = library.add_library_folder(generic, None).await.unwrap();

    let alpha = album(&library, "Alpha", 1999, "The Cardinals", "album").await;
    let cardinals = artist_of(&library, alpha).await;
    let zero = track(
        &library,
        alpha,
        &TrackSpec::new("Zero", 1, 180_000)
            .genres(&["Rock"])
            .explicit(false)
            .isrc("US-AAA-01-00001"),
    )
    .await;
    let one = track(
        &library,
        alpha,
        &TrackSpec::new("One", 2, 200_000)
            .genres(&["Rock", "Pop"])
            .explicit(true),
    )
    .await;
    attach_folder(
        &library,
        &owned,
        zero,
        "the-cardinals/01 - Zero.mp3",
        &FileSpec::mp3(180_000, true),
    )
    .await;
    attach_folder(
        &library,
        &owned,
        one,
        "the-cardinals/02 - One.mp3",
        &FileSpec::mp3(200_000, true),
    )
    .await;

    let bright = album(&library, "Bright", 2001, "2 Mello", "album").await;
    let mello = artist_of(&library, bright).await;
    let signal = track(
        &library,
        bright,
        &TrackSpec::new("Signal", 1, 210_000).genres(&["Electronic"]),
    )
    .await;
    attach_folder(
        &library,
        &generic,
        signal,
        "2-mello/01 - Signal.flac",
        &FileSpec::flac(210_000),
    )
    .await;

    let cache_album = album(&library, "Cache", 2010, "Charlotte", "album").await;
    let charlotte = artist_of(&library, cache_album).await;
    let gone = track(
        &library,
        cache_album,
        &TrackSpec::new("Gone", 1, 240_000).cached(),
    )
    .await;
    attach_cache(
        &library,
        gone,
        "charlotte/01 - Gone.opus",
        &FileSpec::opus(240_000),
    )
    .await;

    let delta = album(&library, "Delta", 2005, "El Mar", "compilation").await;
    let elmar = artist_of(&library, delta).await;
    let ocean = track(
        &library,
        delta,
        &TrackSpec::new("Ocean", 1, 300_000).genres(&["Ambient"]),
    )
    .await;
    attach_folder(
        &library,
        &generic,
        ocean,
        "el-mar/01 - Ocean.flac",
        &FileSpec::flac(300_000),
    )
    .await;

    Seeded {
        cardinals,
        alpha,
        alpha_tracks: vec![zero, one],
        mello,
        bright,
        bright_track: signal,
        charlotte,
        cache_album,
        cache_track: gone,
        elmar,
        delta,
        delta_track: ocean,
    }
}

impl Seeded {
    /// Replaces every seeded client id in a response body with a stable marker.
    ///
    /// A seeded row's client id is derived from the UUIDv7 the insert
    /// generated, so it differs from run to run. The markers keep a snapshot
    /// readable and make a mix-up between two entities visible in the diff.
    ///
    /// # Parameters
    ///
    /// - `body`: the rendered response.
    ///
    /// # Returns
    ///
    /// The body with every seeded id replaced by the name of the entity it
    /// belongs to.
    pub fn stabilize(&self, body: &str) -> String {
        let mut out = body.to_owned();
        let markers = [
            (self.cardinals, "ar_cardinals"),
            (self.mello, "ar_mello"),
            (self.charlotte, "ar_charlotte"),
            (self.elmar, "ar_elmar"),
            (self.alpha, "al_alpha"),
            (self.bright, "al_bright"),
            (self.cache_album, "al_cache"),
            (self.delta, "al_delta"),
            (self.alpha_tracks[0], "tr_zero"),
            (self.alpha_tracks[1], "tr_one"),
            (self.bright_track, "tr_signal"),
            (self.cache_track, "tr_gone"),
            (self.delta_track, "tr_ocean"),
        ];
        for (id, marker) in markers {
            out = out.replace(&id.to_string(), marker);
        }
        out
    }
}

/// Inserts an album credited to one artist and returns its id.
///
/// # Parameters
///
/// - `library`: the repository to write through.
/// - `title`: the album title.
/// - `year`: the release year.
/// - `artist`: the album artist.
/// - `album_type`: the release type, for example `album` or `compilation`.
async fn album(
    library: &LibraryRepository,
    title: &str,
    year: i64,
    artist: &str,
    album_type: &str,
) -> EntityId {
    let mut input = AlbumInput::new(title, Origin::Local);
    input.year = Some(year);
    input.album_type = Some(album_type.to_owned());
    input.display_artist = Some(artist.to_owned());
    library
        .upsert_album(&input, &[ArtistCredit::new(artist, "main")])
        .await
        .unwrap()
}

/// Returns the id of an album's first credited artist.
async fn artist_of(library: &LibraryRepository, album: EntityId) -> EntityId {
    library.list_album_artists(album).await.unwrap()[0].id
}

/// Inserts a track and returns its id.
async fn track(library: &LibraryRepository, album: EntityId, spec: &TrackSpec) -> EntityId {
    let credit = ArtistCredit::new(
        library.list_album_artists(album).await.unwrap()[0]
            .name
            .clone(),
        "main",
    );
    let mut input = TrackInput::new(spec.title, Origin::Local, spec.availability);
    input.album_id = Some(album);
    input.display_artist = Some(credit.artist.name.clone());
    input.track_number = Some(spec.number);
    input.disc_number = Some(1);
    input.duration_ms = Some(spec.duration_ms);
    input.isrc = spec.isrc.map(str::to_owned);
    input.explicit = spec.explicit;
    let genres: Vec<String> = spec
        .genres
        .iter()
        .map(|genre| (*genre).to_owned())
        .collect();
    library
        .insert_track(&input, &[credit], &genres)
        .await
        .unwrap()
}

/// Attaches a scanned folder file to a track.
async fn attach_folder(
    library: &LibraryRepository,
    folder: &LibraryFolder,
    track: EntityId,
    rel_path: &str,
    spec: &FileSpec,
) {
    library
        .upsert_folder_media_file(
            track,
            &ScannedMediaFile {
                folder_id: folder.id,
                rel_path: rel_path.to_owned(),
                size: spec.size,
                mtime_ms: spec.mtime_ms,
                format: Some(spec.format.to_owned()),
                codec: Some(spec.codec.to_owned()),
                bitrate_kbps: spec.bitrate_kbps,
                sample_rate: spec.sample_rate,
                bit_depth: spec.bit_depth,
                channels: spec.channels,
                duration_ms: Some(spec.duration_ms),
                lossless: Some(spec.lossless),
                replaygain: spec.replaygain,
                has_embedded_art: Some(spec.has_embedded_art),
            },
        )
        .await
        .unwrap();
}

/// Attaches a managed file in the evictable cache to a track.
async fn attach_cache(
    library: &LibraryRepository,
    track: EntityId,
    rel_path: &str,
    spec: &FileSpec,
) {
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Cache,
            rel_path: rel_path.to_owned(),
            size: spec.size,
            mtime_ms: spec.mtime_ms,
            format: Some(spec.format.to_owned()),
            codec: Some(spec.codec.to_owned()),
            bitrate_kbps: spec.bitrate_kbps,
            sample_rate: spec.sample_rate,
            bit_depth: spec.bit_depth,
            channels: spec.channels,
            duration_ms: Some(spec.duration_ms),
            lossless: Some(spec.lossless),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: Some(spec.has_embedded_art),
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
}

/// One track to insert.
struct TrackSpec {
    /// The track title.
    title: &'static str,
    /// The track number within its disc.
    number: i64,
    /// The duration in milliseconds.
    duration_ms: i64,
    /// The explicit flag, when the tags carry one.
    explicit: Option<bool>,
    /// The genre names.
    genres: &'static [&'static str],
    /// The availability the track starts with.
    availability: Availability,
    /// The International Standard Recording Code, when the track has one.
    isrc: Option<&'static str>,
}

impl TrackSpec {
    /// Builds a local track with no genres and no explicit flag.
    fn new(title: &'static str, number: i64, duration_ms: i64) -> TrackSpec {
        TrackSpec {
            title,
            number,
            duration_ms,
            explicit: None,
            genres: &[],
            availability: Availability::Local,
            isrc: None,
        }
    }

    /// Sets the genre names.
    fn genres(mut self, genres: &'static [&'static str]) -> TrackSpec {
        self.genres = genres;
        self
    }

    /// Sets the explicit flag.
    fn explicit(mut self, explicit: bool) -> TrackSpec {
        self.explicit = Some(explicit);
        self
    }

    /// Sets the International Standard Recording Code.
    fn isrc(mut self, isrc: &'static str) -> TrackSpec {
        self.isrc = Some(isrc);
        self
    }

    /// Makes the track evictable, as an unpinned cache file is.
    fn cached(mut self) -> TrackSpec {
        self.availability = Availability::Cached;
        self
    }
}

/// The probe-derived fields of one seeded media file.
struct FileSpec {
    /// The file size in bytes.
    size: i64,
    /// The file modification time in unix milliseconds.
    mtime_ms: i64,
    /// The container literal.
    format: &'static str,
    /// The codec literal.
    codec: &'static str,
    /// The bitrate in kbit/s.
    bitrate_kbps: Option<i64>,
    /// The sample rate in hertz.
    sample_rate: Option<i64>,
    /// The sample bit depth.
    bit_depth: Option<i64>,
    /// The channel count.
    channels: Option<i64>,
    /// The duration in milliseconds.
    duration_ms: i64,
    /// Whether the codec stores samples without loss.
    lossless: bool,
    /// The ReplayGain values the file's tags carried.
    replaygain: Option<ReplayGain>,
    /// Whether the file carries an embedded picture.
    has_embedded_art: bool,
}

impl FileSpec {
    /// An mp3 file at 320 kbit/s, 44.1 kHz, stereo.
    fn mp3(duration_ms: i64, has_embedded_art: bool) -> FileSpec {
        FileSpec {
            size: 7_200_000,
            mtime_ms: 1_600_000_000_000,
            format: "mpeg",
            codec: "mp3",
            bitrate_kbps: Some(320),
            sample_rate: Some(44_100),
            bit_depth: None,
            channels: Some(2),
            duration_ms,
            lossless: false,
            replaygain: Some(ReplayGain {
                track_gain: Some(-6.54),
                track_peak: Some(0.988861),
                album_gain: Some(-5.2),
                album_peak: Some(0.999_969),
            }),
            has_embedded_art,
        }
    }

    /// A flac file at 24-bit 48 kHz stereo.
    fn flac(duration_ms: i64) -> FileSpec {
        FileSpec {
            size: 32_000_000,
            mtime_ms: 1_600_000_000_000,
            format: "flac",
            codec: "flac",
            bitrate_kbps: Some(1012),
            sample_rate: Some(48_000),
            bit_depth: Some(24),
            channels: Some(2),
            duration_ms,
            lossless: true,
            replaygain: None,
            has_embedded_art: false,
        }
    }

    /// An opus file at 96 kbit/s, 48 kHz, stereo.
    fn opus(duration_ms: i64) -> FileSpec {
        FileSpec {
            size: 2_880_000,
            mtime_ms: 1_600_000_000_000,
            format: "ogg",
            codec: "opus",
            bitrate_kbps: Some(96),
            sample_rate: Some(48_000),
            bit_depth: None,
            channels: Some(2),
            duration_ms,
            lossless: false,
            replaygain: None,
            has_embedded_art: false,
        }
    }
}
