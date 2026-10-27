//! The stored form of a provider response.
//!
//! every stored source payload is JSON, and the core provider records are
//! deliberately plain data with no serialization of their own. This module is
//! the translation layer between the two: one mirror per record the cache
//! stores, with a conversion in each direction.
//!
//! The mirror is a storage contract, not an API: field names are the Rust names
//! written in snake case, only the fields a record cannot do without are
//! required, and every other field is defaulted when it is absent. A row written
//! by one version of the server therefore stays readable by the next one even
//! when a record grows a field. A row that no longer deserializes at all -
//! because a field changed type, or the payload is not the shape it claims - is
//! treated as a cache miss by [`crate::cache::Cache`], so an upgrade costs a
//! provider call, never a failure.

use hearthsound_core::providers::{
    ArtistCredit, ArtistInfo, ArtistLink, CatalogAlbum, CatalogAlbumRef, CatalogArtist,
    CatalogResults, CatalogTrack, ExternalId, Image, RecordingMatch, ReleaseSummary,
};
use serde::{Deserialize, Serialize};

/// The stored form of an [`ExternalId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireExternalId {
    /// The identifier's namespace.
    pub namespace: String,
    /// The identifier's value within its namespace.
    pub value: String,
}

/// The stored form of an [`ArtistCredit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireArtistCredit {
    /// The credited artist's name.
    pub name: String,
    /// The artist's external identifiers.
    #[serde(default)]
    pub ids: Vec<WireExternalId>,
}

/// The stored form of an [`Image`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireImage {
    /// The image URL.
    pub url: String,
    /// The image width in pixels, when known.
    #[serde(default)]
    pub width: Option<u32>,
    /// The image height in pixels, when known.
    #[serde(default)]
    pub height: Option<u32>,
}

/// The stored form of a [`CatalogAlbumRef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireAlbumRef {
    /// The provider-local album identifier.
    pub id: String,
    /// The album title.
    pub title: String,
    /// The album's artists.
    #[serde(default)]
    pub artists: Vec<WireArtistCredit>,
    /// The release year, when known.
    #[serde(default)]
    pub year: Option<u16>,
    /// The cover images.
    #[serde(default)]
    pub images: Vec<WireImage>,
    /// The album type, when known.
    #[serde(default)]
    pub album_type: Option<String>,
    /// The provider's name for this pressing, when known.
    #[serde(default)]
    pub edition: Option<String>,
    /// The provider's track count for the release, when known.
    #[serde(default)]
    pub track_count: Option<u32>,
    /// The canonical identifiers of the release.
    #[serde(default)]
    pub ids: Vec<WireExternalId>,
}

/// The stored form of a [`CatalogTrack`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireTrack {
    /// The provider-local track identifier.
    pub id: String,
    /// The track title.
    pub title: String,
    /// The track's artists.
    #[serde(default)]
    pub artists: Vec<WireArtistCredit>,
    /// The album the track belongs to, when known.
    #[serde(default)]
    pub album: Option<WireAlbumRef>,
    /// The duration in milliseconds, when known.
    #[serde(default)]
    pub duration_ms: Option<u32>,
    /// The disc number, when known.
    #[serde(default)]
    pub disc_number: Option<u16>,
    /// The track number, when known.
    #[serde(default)]
    pub track_number: Option<u16>,
    /// The International Standard Recording Code, when known.
    #[serde(default)]
    pub isrc: Option<String>,
    /// Whether the track is explicit, when the provider says so.
    #[serde(default)]
    pub explicit: Option<bool>,
    /// The track's external identifiers.
    #[serde(default)]
    pub ids: Vec<WireExternalId>,
}

/// The stored form of a [`CatalogAlbum`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireAlbum {
    /// The provider-local album identifier.
    pub id: String,
    /// The album title.
    pub title: String,
    /// The album's artists.
    #[serde(default)]
    pub artists: Vec<WireArtistCredit>,
    /// The release date, when known.
    #[serde(default)]
    pub release_date: Option<String>,
    /// The release type, when known.
    #[serde(default)]
    pub album_type: Option<String>,
    /// The release UPC, when known.
    #[serde(default)]
    pub upc: Option<String>,
    /// The album's genres.
    #[serde(default)]
    pub genres: Vec<String>,
    /// The cover images.
    #[serde(default)]
    pub images: Vec<WireImage>,
    /// The album's tracks, in playing order.
    #[serde(default)]
    pub tracks: Vec<WireTrack>,
    /// The album's external identifiers.
    #[serde(default)]
    pub ids: Vec<WireExternalId>,
    /// The provider's name for this pressing, when known.
    #[serde(default)]
    pub edition: Option<String>,
}

/// The stored form of a [`CatalogArtist`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireArtist {
    /// The provider-local artist identifier.
    pub id: String,
    /// The artist's display name.
    pub name: String,
    /// Images of the artist.
    #[serde(default)]
    pub images: Vec<WireImage>,
    /// The artist's external identifiers.
    #[serde(default)]
    pub ids: Vec<WireExternalId>,
    /// The number of albums the provider reports, when it reports one.
    #[serde(default)]
    pub album_count: Option<u32>,
}

/// The stored form of a [`CatalogResults`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireResults {
    /// The matching tracks.
    #[serde(default)]
    pub tracks: Vec<WireTrack>,
    /// The matching albums.
    #[serde(default)]
    pub albums: Vec<WireAlbumRef>,
    /// The matching artists.
    #[serde(default)]
    pub artists: Vec<WireArtist>,
}

/// The stored form of a list of images.
///
/// A newtype for the same reason as [`WireAlbumRefs`]; a stored answer is a JSON
/// array of [`WireImage`] objects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireImages(pub(crate) Vec<WireImage>);

impl WireImages {
    /// Builds the stored form of a list of images.
    pub(crate) fn new(images: Vec<WireImage>) -> WireImages {
        WireImages(images)
    }
}

/// The stored form of a list of album references.
///
/// A newtype because the cache needs a `From` conversion in both directions for
/// the whole list, and a bare `Vec` is a foreign type. Serde writes a newtype
/// as the value it wraps, so a stored listing is still a JSON array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireAlbumRefs(Vec<WireAlbumRef>);

/// The stored form of a list of tracks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireTracks(pub(crate) Vec<WireTrack>);

impl WireTracks {
    /// Returns whether the list is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates the stored tracks.
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, WireTrack> {
        self.0.iter()
    }
}

/// The stored form of a [`ReleaseSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireReleaseSummary {
    /// The release's canonical identifiers.
    #[serde(default)]
    pub ids: Vec<WireExternalId>,
    /// The release title.
    pub title: String,
    /// The release date, when known.
    #[serde(default)]
    pub date: Option<String>,
    /// The number of tracks on the release, when known.
    #[serde(default)]
    pub track_count: Option<u32>,
}

impl WireAlbumRefs {
    /// Builds the stored form of a list of album references.
    pub(crate) fn new(refs: Vec<WireAlbumRef>) -> WireAlbumRefs {
        WireAlbumRefs(refs)
    }

    /// Returns the stored list.
    pub(crate) fn into_inner(self) -> Vec<WireAlbumRef> {
        self.0
    }
}

/// The stored form of a [`RecordingMatch`].
///
/// The match carries a score, so this mirror is the one stored shape that is
/// compared with `PartialEq` rather than `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireRecordingMatch {
    /// The recording's canonical identifiers.
    #[serde(default)]
    pub ids: Vec<WireExternalId>,
    /// The recording title.
    pub title: String,
    /// The recording's artists.
    #[serde(default)]
    pub artists: Vec<WireArtistCredit>,
    /// The recording duration in milliseconds, when known.
    #[serde(default)]
    pub duration_ms: Option<u32>,
    /// The releases the recording appears on.
    #[serde(default)]
    pub releases: Vec<WireReleaseSummary>,
    /// The provider's confidence in the match.
    pub score: f32,
}

/// The stored form of the recordings one resolution answered with.
///
/// A newtype for the same reason as [`WireAlbumRefs`]; a stored answer is a
/// JSON array of [`WireRecordingMatch`] objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireRecordingMatches(Vec<WireRecordingMatch>);

/// The stored form of what a metadata instance knows about an artist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireArtistInfo {
    /// The identifiers the instance matched the artist by.
    #[serde(default)]
    pub ids: Vec<WireExternalId>,
    /// The artist's biography, when the instance has one.
    #[serde(default)]
    pub biography: Option<String>,
    /// The pages about the artist.
    #[serde(default)]
    pub links: Vec<WireArtistLink>,
}

/// The stored form of one page about an artist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireArtistLink {
    /// The kind of page.
    pub kind: String,
    /// The page's URL.
    pub url: String,
}

impl From<&ArtistInfo> for WireArtistInfo {
    /// Copies an artist's information into its stored form.
    fn from(value: &ArtistInfo) -> WireArtistInfo {
        WireArtistInfo {
            ids: value.ids.iter().map(WireExternalId::from).collect(),
            biography: value.biography.clone(),
            links: value
                .links
                .iter()
                .map(|link| WireArtistLink {
                    kind: link.kind.clone(),
                    url: link.url.clone(),
                })
                .collect(),
        }
    }
}

impl From<WireArtistInfo> for ArtistInfo {
    /// Rebuilds an artist's information from its stored form.
    fn from(value: WireArtistInfo) -> ArtistInfo {
        ArtistInfo {
            ids: value.ids.into_iter().map(ExternalId::from).collect(),
            biography: value.biography,
            links: value
                .links
                .into_iter()
                .map(|link| ArtistLink {
                    kind: link.kind,
                    url: link.url,
                })
                .collect(),
        }
    }
}

impl From<&Vec<CatalogAlbumRef>> for WireAlbumRefs {
    /// Copies an album listing into its stored form.
    fn from(value: &Vec<CatalogAlbumRef>) -> WireAlbumRefs {
        WireAlbumRefs(value.iter().map(WireAlbumRef::from).collect())
    }
}

impl From<WireAlbumRefs> for Vec<CatalogAlbumRef> {
    /// Rebuilds an album listing from its stored form.
    fn from(value: WireAlbumRefs) -> Vec<CatalogAlbumRef> {
        value.0.into_iter().map(CatalogAlbumRef::from).collect()
    }
}

impl From<&Vec<CatalogTrack>> for WireTracks {
    /// Copies a track listing into its stored form.
    fn from(value: &Vec<CatalogTrack>) -> WireTracks {
        WireTracks(value.iter().map(WireTrack::from).collect())
    }
}

impl From<WireTracks> for Vec<CatalogTrack> {
    /// Rebuilds a track listing from its stored form.
    fn from(value: WireTracks) -> Vec<CatalogTrack> {
        value.0.into_iter().map(CatalogTrack::from).collect()
    }
}

impl From<&ExternalId> for WireExternalId {
    /// Copies an external identifier into its stored form.
    fn from(value: &ExternalId) -> WireExternalId {
        WireExternalId {
            namespace: value.namespace.clone(),
            value: value.value.clone(),
        }
    }
}

impl From<WireExternalId> for ExternalId {
    /// Rebuilds an external identifier from its stored form.
    fn from(value: WireExternalId) -> ExternalId {
        ExternalId {
            namespace: value.namespace,
            value: value.value,
        }
    }
}

impl From<&ArtistCredit> for WireArtistCredit {
    /// Copies an artist credit into its stored form.
    fn from(value: &ArtistCredit) -> WireArtistCredit {
        WireArtistCredit {
            name: value.name.clone(),
            ids: value.ids.iter().map(WireExternalId::from).collect(),
        }
    }
}

impl From<WireArtistCredit> for ArtistCredit {
    /// Rebuilds an artist credit from its stored form.
    fn from(value: WireArtistCredit) -> ArtistCredit {
        ArtistCredit {
            name: value.name,
            ids: value.ids.into_iter().map(ExternalId::from).collect(),
        }
    }
}

impl From<&Image> for WireImage {
    /// Copies an image into its stored form.
    fn from(value: &Image) -> WireImage {
        WireImage {
            url: value.url.clone(),
            width: value.width,
            height: value.height,
        }
    }
}

impl From<WireImage> for Image {
    /// Rebuilds an image from its stored form.
    fn from(value: WireImage) -> Image {
        Image {
            url: value.url,
            width: value.width,
            height: value.height,
        }
    }
}

impl From<&CatalogAlbumRef> for WireAlbumRef {
    /// Copies an album reference into its stored form.
    fn from(value: &CatalogAlbumRef) -> WireAlbumRef {
        WireAlbumRef {
            id: value.id.clone(),
            title: value.title.clone(),
            artists: value.artists.iter().map(WireArtistCredit::from).collect(),
            year: value.year,
            images: value.images.iter().map(WireImage::from).collect(),
            album_type: value.album_type.clone(),
            edition: value.edition.clone(),
            track_count: value.track_count,
            ids: value.ids.iter().map(WireExternalId::from).collect(),
        }
    }
}

impl From<WireAlbumRef> for CatalogAlbumRef {
    /// Rebuilds an album reference from its stored form.
    fn from(value: WireAlbumRef) -> CatalogAlbumRef {
        CatalogAlbumRef {
            id: value.id,
            title: value.title,
            artists: value.artists.into_iter().map(ArtistCredit::from).collect(),
            year: value.year,
            images: value.images.into_iter().map(Image::from).collect(),
            album_type: value.album_type,
            edition: value.edition,
            track_count: value.track_count,
            ids: value.ids.into_iter().map(ExternalId::from).collect(),
        }
    }
}

impl From<&CatalogTrack> for WireTrack {
    /// Copies a track into its stored form.
    fn from(value: &CatalogTrack) -> WireTrack {
        WireTrack {
            id: value.id.clone(),
            title: value.title.clone(),
            artists: value.artists.iter().map(WireArtistCredit::from).collect(),
            album: value.album.as_ref().map(WireAlbumRef::from),
            duration_ms: value.duration_ms,
            disc_number: value.disc_number,
            track_number: value.track_number,
            isrc: value.isrc.clone(),
            explicit: value.explicit,
            ids: value.ids.iter().map(WireExternalId::from).collect(),
        }
    }
}

impl From<WireTrack> for CatalogTrack {
    /// Rebuilds a track from its stored form.
    fn from(value: WireTrack) -> CatalogTrack {
        CatalogTrack {
            id: value.id,
            title: value.title,
            artists: value.artists.into_iter().map(ArtistCredit::from).collect(),
            album: value.album.map(CatalogAlbumRef::from),
            duration_ms: value.duration_ms,
            disc_number: value.disc_number,
            track_number: value.track_number,
            isrc: value.isrc,
            explicit: value.explicit,
            ids: value.ids.into_iter().map(ExternalId::from).collect(),
        }
    }
}

impl From<&CatalogAlbum> for WireAlbum {
    /// Copies an album and its tracklist into their stored form.
    fn from(value: &CatalogAlbum) -> WireAlbum {
        WireAlbum {
            id: value.id.clone(),
            title: value.title.clone(),
            artists: value.artists.iter().map(WireArtistCredit::from).collect(),
            release_date: value.release_date.clone(),
            album_type: value.album_type.clone(),
            upc: value.upc.clone(),
            genres: value.genres.clone(),
            images: value.images.iter().map(WireImage::from).collect(),
            tracks: value.tracks.iter().map(WireTrack::from).collect(),
            ids: value.ids.iter().map(WireExternalId::from).collect(),
            edition: value.edition.clone(),
        }
    }
}

impl From<WireAlbum> for CatalogAlbum {
    /// Rebuilds an album and its tracklist from their stored form.
    fn from(value: WireAlbum) -> CatalogAlbum {
        CatalogAlbum {
            id: value.id,
            title: value.title,
            artists: value.artists.into_iter().map(ArtistCredit::from).collect(),
            release_date: value.release_date,
            album_type: value.album_type,
            upc: value.upc,
            genres: value.genres,
            images: value.images.into_iter().map(Image::from).collect(),
            tracks: value.tracks.into_iter().map(CatalogTrack::from).collect(),
            ids: value.ids.into_iter().map(ExternalId::from).collect(),
            edition: value.edition,
        }
    }
}

impl From<&CatalogArtist> for WireArtist {
    /// Copies an artist into its stored form.
    fn from(value: &CatalogArtist) -> WireArtist {
        WireArtist {
            id: value.id.clone(),
            name: value.name.clone(),
            images: value.images.iter().map(WireImage::from).collect(),
            ids: value.ids.iter().map(WireExternalId::from).collect(),
            album_count: value.album_count,
        }
    }
}

impl From<WireArtist> for CatalogArtist {
    /// Rebuilds an artist from its stored form.
    fn from(value: WireArtist) -> CatalogArtist {
        CatalogArtist {
            id: value.id,
            name: value.name,
            images: value.images.into_iter().map(Image::from).collect(),
            ids: value.ids.into_iter().map(ExternalId::from).collect(),
            album_count: value.album_count,
        }
    }
}

impl From<&CatalogResults> for WireResults {
    /// Copies a search result into its stored form.
    fn from(value: &CatalogResults) -> WireResults {
        WireResults {
            tracks: value.tracks.iter().map(WireTrack::from).collect(),
            albums: value.albums.iter().map(WireAlbumRef::from).collect(),
            artists: value.artists.iter().map(WireArtist::from).collect(),
        }
    }
}

impl From<WireResults> for CatalogResults {
    /// Rebuilds a search result from its stored form.
    fn from(value: WireResults) -> CatalogResults {
        CatalogResults {
            tracks: value.tracks.into_iter().map(CatalogTrack::from).collect(),
            albums: value
                .albums
                .into_iter()
                .map(CatalogAlbumRef::from)
                .collect(),
            artists: value.artists.into_iter().map(CatalogArtist::from).collect(),
        }
    }
}

impl From<&ReleaseSummary> for WireReleaseSummary {
    /// Copies a release summary into its stored form.
    fn from(value: &ReleaseSummary) -> WireReleaseSummary {
        WireReleaseSummary {
            ids: value.ids.iter().map(WireExternalId::from).collect(),
            title: value.title.clone(),
            date: value.date.clone(),
            track_count: value.track_count,
        }
    }
}

impl From<WireReleaseSummary> for ReleaseSummary {
    /// Rebuilds a release summary from its stored form.
    fn from(value: WireReleaseSummary) -> ReleaseSummary {
        ReleaseSummary {
            ids: value.ids.into_iter().map(ExternalId::from).collect(),
            title: value.title,
            date: value.date,
            track_count: value.track_count,
        }
    }
}

impl From<&RecordingMatch> for WireRecordingMatch {
    /// Copies a recording match into its stored form.
    fn from(value: &RecordingMatch) -> WireRecordingMatch {
        WireRecordingMatch {
            ids: value.ids.iter().map(WireExternalId::from).collect(),
            title: value.title.clone(),
            artists: value.artists.iter().map(WireArtistCredit::from).collect(),
            duration_ms: value.duration_ms,
            releases: value
                .releases
                .iter()
                .map(WireReleaseSummary::from)
                .collect(),
            score: value.score,
        }
    }
}

impl From<WireRecordingMatch> for RecordingMatch {
    /// Rebuilds a recording match from its stored form.
    fn from(value: WireRecordingMatch) -> RecordingMatch {
        RecordingMatch {
            ids: value.ids.into_iter().map(ExternalId::from).collect(),
            title: value.title,
            artists: value.artists.into_iter().map(ArtistCredit::from).collect(),
            duration_ms: value.duration_ms,
            releases: value
                .releases
                .into_iter()
                .map(ReleaseSummary::from)
                .collect(),
            score: value.score,
        }
    }
}

impl From<&Vec<RecordingMatch>> for WireRecordingMatches {
    /// Copies a resolution answer into its stored form.
    fn from(value: &Vec<RecordingMatch>) -> WireRecordingMatches {
        WireRecordingMatches(value.iter().map(WireRecordingMatch::from).collect())
    }
}

impl From<WireRecordingMatches> for Vec<RecordingMatch> {
    /// Rebuilds a resolution answer from its stored form.
    fn from(value: WireRecordingMatches) -> Vec<RecordingMatch> {
        value.0.into_iter().map(RecordingMatch::from).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored search payload as the table holds it, with the fields a track
    /// cannot do without.
    const STORED: &str = r#"{"tracks":[{"id":"remote-1","title":"Remote Song",
        "artists":[{"name":"Remote Artist"}],"duration_ms":1000,
        "isrc":"USRC17607839"}],"albums":[],"artists":[]}"#;

    #[test]
    fn a_stored_payload_round_trips() {
        let results = CatalogResults {
            tracks: vec![CatalogTrack {
                id: "t1".to_owned(),
                title: "Title".to_owned(),
                artists: vec![ArtistCredit {
                    name: "Artist".to_owned(),
                    ids: vec![ExternalId {
                        namespace: "deezer".to_owned(),
                        value: "1".to_owned(),
                    }],
                }],
                album: Some(CatalogAlbumRef {
                    id: "a1".to_owned(),
                    title: "Album".to_owned(),
                    artists: Vec::new(),
                    year: Some(2026),
                    images: vec![Image {
                        url: "https://example.invalid/cover.jpg".to_owned(),
                        width: Some(500),
                        height: Some(500),
                    }],
                    album_type: Some("album".to_owned()),
                    edition: Some("Deluxe".to_owned()),
                    track_count: Some(1),
                    ids: vec![ExternalId {
                        namespace: "mb-release".to_owned(),
                        value: "rel".to_owned(),
                    }],
                }),
                duration_ms: Some(1_000),
                disc_number: Some(1),
                track_number: Some(2),
                isrc: Some("USRC17607839".to_owned()),
                explicit: Some(false),
                ids: Vec::new(),
            }],
            albums: vec![CatalogAlbumRef {
                id: "a1".to_owned(),
                title: "Album".to_owned(),
                artists: Vec::new(),
                year: None,
                images: Vec::new(),
                album_type: None,
                edition: None,
                track_count: None,
                ids: Vec::new(),
            }],
            artists: vec![CatalogArtist {
                id: "ar1".to_owned(),
                name: "Artist".to_owned(),
                images: Vec::new(),
                ids: Vec::new(),
                album_count: Some(38),
            }],
        };

        let json = serde_json::to_string(&WireResults::from(&results)).unwrap();
        let decoded = WireResults::deserialize(&mut serde_json::Deserializer::from_str(&json))
            .unwrap()
            .into();

        assert_eq!(results, decoded);
    }

    #[test]
    fn round_trips_an_artists_album_count() {
        let artist = CatalogArtist {
            id: "ar1".to_owned(),
            name: "Artist".to_owned(),
            images: Vec::new(),
            ids: Vec::new(),
            album_count: Some(12),
        };
        let json = serde_json::to_string(&WireArtist::from(&artist)).unwrap();
        let decoded: CatalogArtist = serde_json::from_str::<WireArtist>(&json).unwrap().into();
        assert_eq!(decoded, artist);

        // A provider that reports no count keeps `None`, and so does a payload
        // stored before the field existed.
        let none = CatalogArtist {
            id: "ar2".to_owned(),
            name: "Other".to_owned(),
            images: Vec::new(),
            ids: Vec::new(),
            album_count: None,
        };
        let json = serde_json::to_string(&WireArtist::from(&none)).unwrap();
        let decoded: CatalogArtist = serde_json::from_str::<WireArtist>(&json).unwrap().into();
        assert_eq!(decoded, none);

        let stored: WireArtist = serde_json::from_str(r#"{"id":"ar3","name":"Legacy"}"#).unwrap();
        assert_eq!(CatalogArtist::from(stored).album_count, None);
    }

    #[test]
    fn a_stored_payload_that_omits_optional_fields_still_decodes() {
        let decoded = serde_json::from_str::<WireResults>(STORED).unwrap();
        let results: CatalogResults = decoded.into();

        assert_eq!(results.tracks.len(), 1);
        assert_eq!(results.tracks[0].title, "Remote Song");
        assert_eq!(results.tracks[0].artists[0].name, "Remote Artist");
        assert_eq!(results.tracks[0].duration_ms, Some(1_000));
        assert_eq!(results.tracks[0].album, None);
        assert!(results.tracks[0].ids.is_empty());
    }
}

/// The stored form of a list of provider ids.
///
/// Discovery lists similar artists by the provider's own ids, and the stored
/// answer keeps them that way: the rows are resolved when the fields are
/// recomputed, so an answer survives a restart that renamed nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireProviderIds(Vec<String>);

impl WireProviderIds {
    /// Builds the stored form of a list of provider ids.
    pub(crate) fn new(ids: Vec<String>) -> WireProviderIds {
        WireProviderIds(ids)
    }

    /// Returns whether the list is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates the provider ids.
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, String> {
        self.0.iter()
    }
}

/// Returns the stored row a payload's raw uuid names, when it names one.
///
/// The indexer resolves a provider answer into rows before it stores it, so the
/// payload of a list relation holds the rows themselves rather than the ids the
/// provider used: a row matched by ISRC or a MusicBrainz id carries no id of
/// the answering instance at all.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `value`: the row's uuid, as the payload wrote it.
///
/// # Returns
///
/// The row, or `None` when the value is not a uuid or no row of that kind has
/// it.
///
/// # Errors
///
/// Returns [`crate::error::CatalogError::Database`] when the read fails.
pub(crate) async fn row_by_uuid(
    conn: &mut sqlx::SqliteConnection,
    kind: hearthsound_core::EntityKind,
    value: &str,
) -> Result<Option<uuid::Uuid>, crate::error::CatalogError> {
    let Ok(row) = uuid::Uuid::parse_str(value) else {
        return Ok(None);
    };
    let bound = row.as_bytes().as_slice();
    let found: Option<Vec<u8>> = match kind {
        hearthsound_core::EntityKind::Track => {
            sqlx::query_scalar("SELECT id FROM tracks WHERE id = ?")
                .bind(bound)
                .fetch_optional(&mut *conn)
                .await?
        }
        hearthsound_core::EntityKind::Album => {
            sqlx::query_scalar("SELECT id FROM albums WHERE id = ?")
                .bind(bound)
                .fetch_optional(&mut *conn)
                .await?
        }
        hearthsound_core::EntityKind::Artist => {
            sqlx::query_scalar("SELECT id FROM artists WHERE id = ?")
                .bind(bound)
                .fetch_optional(&mut *conn)
                .await?
        }
        hearthsound_core::EntityKind::Playlist => None,
    };
    Ok(found
        .filter(|bytes| bytes.len() == 16)
        .and_then(|bytes| uuid::Uuid::from_slice(&bytes).ok()))
}
