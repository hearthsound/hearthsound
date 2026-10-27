//! Response shapes several resources share.

use std::collections::HashMap;

use hearthsound_auth::{User, UserPermissions};
use hearthsound_catalog::CatalogService;
use hearthsound_catalog::views::{AlbumRef, ArtistRef, TrackRef};
use hearthsound_config::{SettingDef, SettingKind, SettingValue};
use hearthsound_core::{Availability, EntityId, EntityKind, IdPrefix};
use hearthsound_library::query::EntityFacts;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::problem::{self, Problem};

/// What an account may do besides streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsView {
    /// May trigger acquisitions (`can_acquire`).
    pub acquire: bool,
    /// May upload files (`can_upload`).
    pub upload: bool,
    /// May download files (`can_download`).
    pub download: bool,
}

impl From<UserPermissions> for PermissionsView {
    fn from(permissions: UserPermissions) -> PermissionsView {
        PermissionsView {
            acquire: permissions.acquire,
            upload: permissions.upload,
            download: permissions.download,
        }
    }
}

impl From<PermissionsView> for UserPermissions {
    fn from(view: PermissionsView) -> UserPermissions {
        UserPermissions {
            acquire: view.acquire,
            upload: view.upload,
            download: view.download,
        }
    }
}

/// An account, without its secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserView {
    /// The account's client id (`us_...`).
    pub id: String,
    /// The sign-in name.
    pub username: String,
    /// The name clients show.
    pub display_name: String,
    /// Whether the account administers the server.
    pub is_admin: bool,
    /// What the account may do besides streaming.
    pub permissions: PermissionsView,
    /// When the account was created, in unix milliseconds.
    pub created_at: i64,
    /// When the account last changed, in unix milliseconds.
    pub updated_at: i64,
    /// When the account was disabled, in unix milliseconds; absent while it is
    /// active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<i64>,
}

impl From<&User> for UserView {
    fn from(user: &User) -> UserView {
        UserView {
            id: EntityId::from_uuid(IdPrefix::User, user.id).to_string(),
            username: user.username.clone(),
            display_name: user.display_name.clone(),
            is_admin: user.is_admin,
            permissions: user.permissions().into(),
            created_at: user.created_at,
            updated_at: user.updated_at,
            disabled_at: user.disabled_at,
        }
    }
}

/// The current time in unix milliseconds.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Reads a client id from a path and checks its kind.
///
/// # Parameters
///
/// - `value`: the path segment.
/// - `prefix`: the kind the resource names, for example [`IdPrefix::User`].
///
/// # Returns
///
/// The row id.
///
/// # Errors
///
/// Answers `not-found` when the segment is not a client id of that kind:
/// nothing can live at such a path.
pub fn parse_id(value: &str, prefix: IdPrefix) -> Result<Uuid, Problem> {
    match EntityId::parse(value) {
        Ok(id) if id.kind() == prefix => Ok(id.uuid()),
        _ => Err(Problem::not_found(format!(
            "`{value}` is not a `{prefix}_` id."
        ))),
    }
}

/// Reads an entity id from a path and resolves a merged-away id to its survivor.
///
/// A merge keeps the loser's id resolving, so a client that held an id from an
/// earlier answer is answered the row that stands for it now, exactly as the
/// page's own body names it.
///
/// # Parameters
///
/// - `catalog`: the catalog layer, which owns the redirects.
/// - `value`: the path segment.
/// - `prefix`: the kind the resource names: [`IdPrefix::Track`],
///   [`IdPrefix::Album`] or [`IdPrefix::Artist`].
///
/// # Returns
///
/// The surviving row's id.
///
/// # Errors
///
/// Answers `not-found` when the segment is not an id of that kind or when
/// neither the row nor a redirect exists, and `internal` when the redirect
/// cannot be read.
pub async fn entity_id(
    catalog: &CatalogService,
    value: &str,
    prefix: IdPrefix,
) -> Result<EntityId, Problem> {
    let id = EntityId::from_uuid(prefix, parse_id(value, prefix)?);
    let Some(kind) = prefix.entity_kind() else {
        return Err(Problem::not_found(format!("`{value}` names no entity.")));
    };
    match catalog.canonical(kind, id).await {
        Ok(Some(canonical)) => Ok(canonical),
        Ok(None) => Err(Problem::not_found(format!(
            "No entity with the id `{value}` is stored."
        ))),
        Err(error) => Err(problem::internal("resolving an entity id", &error)),
    }
}

/// The caller's own state for one entity.
///
/// The values come from [`hearthsound_library::query::entity_facts`], which the
/// Subsonic `hearthsound` member reads too, so both surfaces agree about an
/// entity. A row the facts map does not name is reported as an unreferenced
/// remote entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// Whether the caller's personal library holds the entity.
    pub in_library: bool,
    /// Whether the caller likes the entity.
    pub liked: bool,
    /// Whether the caller dislikes the entity.
    pub disliked: bool,
    /// The rating the caller gave the entity, from 1 to 5.
    pub rating: Option<u8>,
    /// How available the entity is to this server.
    pub availability: Availability,
}

impl Facts {
    /// Reads one entity's facts out of a map [`hearthsound_library::query::entity_facts`]
    /// returned.
    ///
    /// # Parameters
    ///
    /// - `facts`: the facts of every entity the page names.
    /// - `id`: the entity whose facts are wanted.
    ///
    /// # Returns
    ///
    /// The entity's facts, or the default of one no row backs.
    pub fn of(facts: &HashMap<Uuid, EntityFacts>, id: EntityId) -> Facts {
        match facts.get(&id.uuid()) {
            Some(found) => Facts {
                in_library: found.in_library,
                liked: found.liked,
                disliked: found.disliked,
                rating: found.rating,
                availability: found.availability,
            },
            None => Facts::default(),
        }
    }
}

impl Default for Facts {
    /// Reports an entity no facts row backs as remote and untouched.
    fn default() -> Facts {
        Facts {
            in_library: false,
            liked: false,
            disliked: false,
            rating: None,
            availability: Availability::Remote,
        }
    }
}

/// One external page an artist is linked from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkView {
    /// The kind of page, for example `wikipedia` or `homepage`.
    pub kind: String,
    /// The page's URL.
    pub url: String,
}

impl From<&hearthsound_catalog::views::Link> for LinkView {
    fn from(link: &hearthsound_catalog::views::Link) -> LinkView {
        LinkView {
            kind: link.kind.clone(),
            url: link.url.clone(),
        }
    }
}

/// One artist as a list names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtistRefView {
    /// The artist's client id (`ar_...`).
    pub id: String,
    /// The artist's display name.
    pub name: String,
}

impl From<&ArtistRef> for ArtistRefView {
    fn from(reference: &ArtistRef) -> ArtistRefView {
        ArtistRefView {
            id: reference.artist.id.to_string(),
            name: reference.artist.name.clone(),
        }
    }
}

/// One artist in a shelf or a result list.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtistSummary {
    /// The artist's client id (`ar_...`).
    pub id: String,
    /// The artist's display name.
    pub name: String,
    /// Whether the server holds a portrait.
    pub has_image: bool,
    /// Whether the caller's personal library holds the artist.
    pub in_library: bool,
    /// How strongly the artist matched, from 1.0 (the best) down towards zero.
    ///
    /// The catalog layer does not expose a relevance score yet, so a search
    /// ranks each kind's hits by their position in the answer: the first hit is
    /// `1.0` and each later one is `1 / (offset + position + 1)`, which keeps a
    /// later page's hits below an earlier page's. An entity page carries `1.0`
    /// for its own entity and for every member it names.
    pub score: f32,
}

impl ArtistSummary {
    /// Builds the summary of one artist.
    ///
    /// # Parameters
    ///
    /// - `reference`: the artist and whether a portrait is stored.
    /// - `facts`: the caller's state for the artist.
    /// - `score`: how strongly the artist matched, 1.0 on an entity page.
    pub fn of(reference: &ArtistRef, facts: Facts, score: f32) -> ArtistSummary {
        ArtistSummary {
            id: reference.artist.id.to_string(),
            name: reference.artist.name.clone(),
            has_image: reference.has_image,
            in_library: facts.in_library,
            score,
        }
    }
}

/// One album in a shelf or a result list.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AlbumSummary {
    /// The album's client id (`al_...`).
    pub id: String,
    /// The album's title.
    pub title: String,
    /// The edition label, when the release states one.
    pub edition: Option<String>,
    /// The release year, when known.
    pub year: Option<i64>,
    /// The release type: `album`, `ep`, `single` or a secondary type.
    pub album_type: Option<String>,
    /// The album artist as it is displayed.
    pub display_artist: Option<String>,
    /// The primary credited artist's client id (`ar_...`).
    pub artist_id: Option<String>,
    /// Whether the server holds a cover.
    pub has_image: bool,
    /// Whether the caller's personal library holds the album.
    pub in_library: bool,
    /// How strongly the album matched, from 1.0 (the best) down towards zero.
    ///
    /// The catalog layer does not expose a relevance score yet, so a search
    /// ranks each kind's hits by their position in the answer: the first hit is
    /// `1.0` and each later one is `1 / (offset + position + 1)`, which keeps a
    /// later page's hits below an earlier page's. An entity page carries `1.0`
    /// for its own entity and for every member it names.
    pub score: f32,
}

impl AlbumSummary {
    /// Builds the summary of one album.
    ///
    /// # Parameters
    ///
    /// - `reference`: the album, its primary artist and whether a cover is
    ///   stored.
    /// - `facts`: the caller's state for the album.
    /// - `score`: how strongly the album matched, 1.0 on an entity page.
    pub fn of(reference: &AlbumRef, facts: Facts, score: f32) -> AlbumSummary {
        AlbumSummary {
            id: reference.album.id.to_string(),
            title: reference.album.title.clone(),
            edition: reference.album.edition.clone(),
            year: reference.album.year,
            album_type: reference.album.album_type.clone(),
            display_artist: reference.album.display_artist.clone().or_else(|| {
                reference
                    .artist
                    .as_ref()
                    .map(|artist| artist.artist.name.clone())
            }),
            artist_id: reference
                .artist
                .as_ref()
                .map(|artist| artist.artist.id.to_string()),
            has_image: reference.has_image,
            in_library: facts.in_library,
            score,
        }
    }
}

/// One track as a page, a shelf or a result list names it.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrackSummary {
    /// The track's client id (`tr_...`), which `/rest/stream` accepts.
    pub id: String,
    /// The track's title.
    pub title: String,
    /// The credited artists, in order.
    pub artists: Vec<ArtistRefView>,
    /// The album's client id (`al_...`); `null` when the track has no album.
    pub album_id: Option<String>,
    /// The album's title; `null` when the track has no album.
    pub album_title: Option<String>,
    /// The disc number within the release.
    pub disc_number: Option<i64>,
    /// The track number within its disc.
    pub track_number: Option<i64>,
    /// The track's length in milliseconds.
    pub duration_ms: Option<i64>,
    /// Whether the track is explicit, when the tags say.
    pub explicit: Option<bool>,
    /// The track's availability: `local`, `cached`, `acquiring`, `remote` or
    /// `unavailable`.
    pub availability: String,
    /// Whether the caller's personal library holds the track.
    pub in_library: bool,
    /// Whether the caller liked the track.
    pub liked: bool,
    /// Whether the caller disliked the track.
    pub disliked: bool,
    /// The caller's rating, from 1 to 5.
    pub rating: Option<u8>,
    /// Whether the server holds a cover for the track or its album.
    pub has_image: bool,
    /// How strongly the track matched, from 1.0 (the best) down towards zero.
    ///
    /// The catalog layer does not expose a relevance score yet, so a search
    /// ranks each kind's hits by their position in the answer: the first hit is
    /// `1.0` and each later one is `1 / (offset + position + 1)`, which keeps a
    /// later page's hits below an earlier page's. An entity page carries `1.0`
    /// for its own entity and for every member it names.
    pub score: f32,
}

impl TrackSummary {
    /// Builds the summary of one track.
    ///
    /// # Parameters
    ///
    /// - `reference`: the track, its artists and whether a cover is stored.
    /// - `facts`: the caller's state for the track.
    /// - `score`: how strongly the track matched, 1.0 on an entity page.
    pub fn of(reference: &TrackRef, facts: Facts, score: f32) -> TrackSummary {
        TrackSummary {
            id: reference.track.id.to_string(),
            title: reference.track.title.clone(),
            artists: reference.artists.iter().map(ArtistRefView::from).collect(),
            album_id: reference.track.album_id.map(|id| id.to_string()),
            album_title: reference.album_title.clone(),
            disc_number: reference.track.disc_number,
            track_number: reference.track.track_number,
            duration_ms: reference.track.duration_ms,
            explicit: reference.track.explicit,
            availability: facts.availability.as_str().to_owned(),
            in_library: facts.in_library,
            liked: facts.liked,
            disliked: facts.disliked,
            rating: facts.rating,
            has_image: reference.has_image,
            score,
        }
    }
}

/// Reads the caller's facts for a list of entities of one kind.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `user`: the caller whose library, feedback and ratings are read.
/// - `kind`: the kind of every id in the list.
/// - `ids`: the entities the answer names.
///
/// # Returns
///
/// The facts of every stored entity, keyed by its raw uuid.
///
/// # Errors
///
/// Answers `internal` when the read fails.
pub async fn entity_facts(
    state: &crate::state::NativeState,
    user: Uuid,
    kind: EntityKind,
    ids: &[EntityId],
) -> Result<HashMap<Uuid, EntityFacts>, Problem> {
    let rows: Vec<Uuid> = ids.iter().map(EntityId::uuid).collect();
    hearthsound_library::query::entity_facts(state.database(), user, kind, &rows)
        .await
        .map_err(|error| problem::internal("reading an entity's facts", &error))
}

/// Formats a row id as the client id of its kind.
pub fn client_id(prefix: IdPrefix, id: Uuid) -> String {
    EntityId::from_uuid(prefix, id).to_string()
}

/// Reads a JSON value as the value of one setting.
///
/// The JSON type has to match the setting's kind (a boolean, an integer, a
/// number, a string, or a list of strings), and the value has to pass the
/// setting's own validation (its range or its allowed literals).
///
/// # Parameters
///
/// - `def`: the setting.
/// - `json`: the value a request sent.
///
/// # Returns
///
/// The setting value.
///
/// # Errors
///
/// Answers `invalid-request` naming the key when the value does not fit.
pub fn setting_value(def: &SettingDef, json: &serde_json::Value) -> Result<SettingValue, Problem> {
    let invalid = |reason: &str| Problem::invalid_request(format!("`{}`: {reason}", def.key));
    let value = match def.kind {
        SettingKind::Bool => json
            .as_bool()
            .map(SettingValue::Bool)
            .ok_or_else(|| invalid("expected a boolean"))?,
        SettingKind::Integer { .. } => json
            .as_i64()
            .map(SettingValue::Integer)
            .ok_or_else(|| invalid("expected an integer"))?,
        SettingKind::Float { .. } => json
            .as_f64()
            .map(SettingValue::Float)
            .ok_or_else(|| invalid("expected a number"))?,
        SettingKind::String { .. } => json
            .as_str()
            .map(|text| SettingValue::String(text.to_owned()))
            .ok_or_else(|| invalid("expected a string"))?,
        SettingKind::StringList { .. } => {
            let items = json
                .as_array()
                .ok_or_else(|| invalid("expected a list of strings"))?;
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(
                    item.as_str()
                        .ok_or_else(|| invalid("expected a list of strings"))?
                        .to_owned(),
                );
            }
            SettingValue::StringList(values)
        }
    };
    def.validate(&value)
        .map_err(|error| Problem::invalid_request(error.to_string()))?;
    Ok(value)
}

/// Writes a setting value as JSON.
pub fn setting_json(value: &SettingValue) -> serde_json::Value {
    serde_json::from_str(&value.to_json()).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_of_the_wrong_kind_lives_nowhere() {
        let album = EntityId::from_uuid(IdPrefix::Album, Uuid::nil());
        assert_eq!(
            parse_id(&album.to_string(), IdPrefix::Album),
            Ok(Uuid::nil())
        );
        assert_eq!(
            parse_id(&album.to_string(), IdPrefix::Artist)
                .unwrap_err()
                .status,
            404
        );
        assert_eq!(
            parse_id("not-an-id", IdPrefix::Album).unwrap_err().status,
            404
        );
    }

    #[test]
    fn facts_of_an_absent_row_describe_a_remote_entity() {
        let facts = Facts::of(
            &HashMap::new(),
            EntityId::from_uuid(IdPrefix::Track, Uuid::nil()),
        );
        assert_eq!(facts, Facts::default());
        assert!(!facts.in_library);
        assert_eq!(facts.availability, Availability::Remote);
    }

    #[test]
    fn facts_of_a_stored_row_carry_the_callers_state() {
        let id = EntityId::from_uuid(IdPrefix::Album, Uuid::nil());
        let mut map = HashMap::new();
        map.insert(
            id.uuid(),
            EntityFacts {
                in_library: true,
                liked: true,
                disliked: false,
                rating: Some(4),
                availability: Availability::Local,
            },
        );
        let facts = Facts::of(&map, id);
        assert!(facts.in_library);
        assert!(facts.liked);
        assert!(!facts.disliked);
        assert_eq!(facts.rating, Some(4));
        assert_eq!(facts.availability, Availability::Local);
    }
}
