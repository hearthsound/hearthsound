//! The client id codec.
//!
//! Clients never see the raw UUIDv7 primary keys the database stores. Every
//! record a client can name has a deterministic, URL-safe string id derived
//! from its key, and every catalog entity that has no row yet has an id that
//! names the provider instance holding it. This module owns both forms:
//!
//! - a materialized entity id is `<prefix>_<payload>`, where the prefix names
//!   the record kind and the payload is the lowercase Crockford base32
//!   encoding of the record's UUIDv7, left zero padded to exactly 26
//!   characters (130 bits, of which the leading two are always zero);
//! - an unmaterialized catalog id is `x_<slug>_<kind>_<payload>`, where `slug`
//!   is the provider instance that issued the provider-local id, `kind` is the
//!   single letter `t`, `a`, `r` or `p`, and the payload is the lowercase
//!   Crockford base32 encoding of the provider id's UTF-8 bytes with no
//!   padding to a fixed width.
//!
//! Crockford base32 excludes `i`, `l`, `o` and `u`, which removes the common
//! visual ambiguities; only the lowercase form is accepted and produced.
//!
//! [`ClientId::parse`] decodes either form, so a router can dispatch on the
//! variant instead of guessing which endpoint an id came from.
//!
//! # Examples
//!
//! ```
//! use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix, Slug};
//!
//! let track = EntityId::new(IdPrefix::Track);
//! let text = track.to_string();
//! assert!(text.starts_with("tr_"));
//! assert_eq!(ClientId::parse(&text).unwrap(), ClientId::Entity(track));
//!
//! let slug = Slug::new("deezer").unwrap();
//! let catalog = CatalogId::new(slug, EntityKind::Track, "3135556").unwrap();
//! assert_eq!(catalog.to_string(), "x_deezer_t_6crk6d9n6mv0");
//! assert_eq!(ClientId::parse("x_deezer_t_6crk6d9n6mv0").unwrap(), ClientId::Catalog(catalog));
//! ```

use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

use crate::EntityKind;
use crate::slug::{InvalidSlug, Slug};

/// The lowercase Crockford base32 alphabet.
///
/// The alphabet is `0-9` followed by the letters of the Latin alphabet except
/// `i`, `l`, `o` and `u`. Index equals digit value.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The number of characters in a materialized id's payload.
///
/// 26 characters carry 130 bits, which covers the 128 bits of a UUIDv7 with
/// two leading zero bits.
const PAYLOAD_LEN: usize = 26;

/// The marker that starts an unmaterialized catalog id.
const CATALOG_MARKER: &str = "x_";

/// Generates a new time-ordered UUIDv7.
///
/// Every entity primary key the server creates is generated here. UUIDv7 sorts
/// by creation time, so inserts append to the end of the SQLite B-tree instead
/// of splitting pages somewhere in the middle.
///
/// # Returns
///
/// A fresh UUIDv7. Two ids generated in the same millisecond are still ordered
/// by their creation, because the generator keeps a sub-millisecond counter.
///
/// # Examples
///
/// ```
/// use hearthsound_core::new_uuid_v7;
///
/// let first = new_uuid_v7();
/// let second = new_uuid_v7();
/// assert_ne!(first, second);
/// assert_eq!(first.get_version_num(), 7);
/// ```
pub fn new_uuid_v7() -> Uuid {
    Uuid::now_v7()
}

/// The prefix of a materialized client id.
///
/// The prefix names what kind of record the id points at, which is what lets a
/// router reject an id used on the wrong endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IdPrefix {
    /// A track (`tr`).
    Track,
    /// An album (`al`).
    Album,
    /// An artist (`ar`).
    Artist,
    /// A playlist (`pl`).
    Playlist,
    /// A user (`us`).
    User,
    /// An acquisition (`ac`).
    Acquisition,
    /// An API key (`ak`).
    ApiKey,
    /// An app password (`ap`).
    AppPassword,
    /// A scanned library folder (`lf`).
    LibraryFolder,
    /// An upload (`up`).
    Upload,
}

impl IdPrefix {
    /// Every prefix, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [IdPrefix; 10] = [
        IdPrefix::Track,
        IdPrefix::Album,
        IdPrefix::Artist,
        IdPrefix::Playlist,
        IdPrefix::User,
        IdPrefix::Acquisition,
        IdPrefix::ApiKey,
        IdPrefix::AppPassword,
        IdPrefix::LibraryFolder,
        IdPrefix::Upload,
    ];

    /// Returns the two-letter prefix written into a client id.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::IdPrefix;
    ///
    /// assert_eq!(IdPrefix::Acquisition.as_str(), "ac");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            IdPrefix::Track => "tr",
            IdPrefix::Album => "al",
            IdPrefix::Artist => "ar",
            IdPrefix::Playlist => "pl",
            IdPrefix::User => "us",
            IdPrefix::Acquisition => "ac",
            IdPrefix::ApiKey => "ak",
            IdPrefix::AppPassword => "ap",
            IdPrefix::LibraryFolder => "lf",
            IdPrefix::Upload => "up",
        }
    }

    /// Returns the catalog entity kind this prefix names, when there is one.
    ///
    /// `Track`, `Album`, `Artist` and `Playlist` map to the matching
    /// [`EntityKind`]; every other prefix names a record that is not a catalog
    /// entity and maps to [`None`].
    ///
    /// # Returns
    ///
    /// The matching entity kind, or [`None`] for a user or acquisition id.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::{EntityKind, IdPrefix};
    ///
    /// assert_eq!(IdPrefix::Album.entity_kind(), Some(EntityKind::Album));
    /// assert_eq!(IdPrefix::User.entity_kind(), None);
    /// ```
    pub const fn entity_kind(self) -> Option<EntityKind> {
        match self {
            IdPrefix::Track => Some(EntityKind::Track),
            IdPrefix::Album => Some(EntityKind::Album),
            IdPrefix::Artist => Some(EntityKind::Artist),
            IdPrefix::Playlist => Some(EntityKind::Playlist),
            IdPrefix::User
            | IdPrefix::Acquisition
            | IdPrefix::ApiKey
            | IdPrefix::AppPassword
            | IdPrefix::LibraryFolder
            | IdPrefix::Upload => None,
        }
    }

    /// Returns the prefix of a catalog entity kind.
    ///
    /// This is the inverse of [`IdPrefix::entity_kind`] on the four catalog
    /// kinds.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::{EntityKind, IdPrefix};
    ///
    /// assert_eq!(IdPrefix::from_entity_kind(EntityKind::Track), IdPrefix::Track);
    /// ```
    pub const fn from_entity_kind(kind: EntityKind) -> IdPrefix {
        match kind {
            EntityKind::Track => IdPrefix::Track,
            EntityKind::Album => IdPrefix::Album,
            EntityKind::Artist => IdPrefix::Artist,
            EntityKind::Playlist => IdPrefix::Playlist,
        }
    }
}

impl fmt::Display for IdPrefix {
    /// Writes the two-letter prefix.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A materialized record's client id.
///
/// The id is a pure function of the record's UUIDv7 primary key, so a client
/// id can be turned back into the primary key by decoding it; no lookup is
/// required to route it to a row.
///
/// # Examples
///
/// ```
/// use hearthsound_core::{EntityId, IdPrefix, new_uuid_v7};
///
/// let uuid = new_uuid_v7();
/// let id = EntityId::from_uuid(IdPrefix::Album, uuid);
/// assert_eq!(id.uuid(), uuid);
/// assert!(id.to_string().starts_with("al_"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntityId {
    kind: IdPrefix,
    uuid: Uuid,
}

impl EntityId {
    /// Generates a new id for `kind` around a fresh UUIDv7.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::{EntityId, IdPrefix};
    ///
    /// let id = EntityId::new(IdPrefix::Track);
    /// assert_eq!(id.kind(), IdPrefix::Track);
    /// assert_eq!(id.uuid().get_version_num(), 7);
    /// ```
    pub fn new(kind: IdPrefix) -> EntityId {
        EntityId::from_uuid(kind, new_uuid_v7())
    }

    /// Wraps an existing UUIDv7 primary key.
    ///
    /// # Parameters
    ///
    /// - `kind`: the record kind the id names.
    /// - `uuid`: the record's primary key. Any 128-bit value round trips; the
    ///   server only ever generates UUIDv7 values.
    pub fn from_uuid(kind: IdPrefix, uuid: Uuid) -> EntityId {
        EntityId { kind, uuid }
    }

    /// Returns the record kind this id names.
    pub fn kind(&self) -> IdPrefix {
        self.kind
    }

    /// Returns the record's UUIDv7 primary key.
    ///
    /// The returned value is the 16 raw bytes the database stores in the row's
    /// `BLOB` primary key.
    pub fn uuid(&self) -> Uuid {
        self.uuid
    }
}

impl fmt::Display for EntityId {
    /// Writes `<prefix>_<26 character Crockford payload>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.uuid.as_u128();
        let mut payload = [0u8; PAYLOAD_LEN];
        for (index, slot) in payload.iter_mut().enumerate() {
            let shift = 5 * (PAYLOAD_LEN - 1 - index);
            *slot = ALPHABET[((value >> shift) & 0x1f) as usize];
        }
        // The payload only ever holds characters of the ASCII alphabet.
        let payload = std::str::from_utf8(&payload).map_err(|_| fmt::Error)?;
        write!(f, "{}_{payload}", self.kind.as_str())
    }
}

/// An unmaterialized catalog entity's client id.
///
/// The id names a provider instance's own identifier for an entity that has no
/// row in the database yet. The payload is opaque to the core: it means
/// nothing until it is resolved through the issuing instance, which is why the
/// provider id is kept verbatim rather than parsed.
///
/// # Examples
///
/// ```
/// use hearthsound_core::{CatalogId, EntityKind, Slug};
///
/// let slug = Slug::new("deezer").unwrap();
/// let id = CatalogId::new(slug, EntityKind::Album, "12345678").unwrap();
/// assert_eq!(id.slug().as_str(), "deezer");
/// assert_eq!(id.kind(), EntityKind::Album);
/// assert_eq!(id.provider_id(), "12345678");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CatalogId {
    slug: Slug,
    kind: EntityKind,
    provider_id: String,
}

impl CatalogId {
    /// Creates a catalog id for `provider_id` as issued by `slug`.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::EmptyProviderId`] when `provider_id` is empty: an id
    /// that names no provider id cannot be resolved.
    pub fn new(
        slug: Slug,
        kind: EntityKind,
        provider_id: impl Into<String>,
    ) -> Result<CatalogId, IdError> {
        let provider_id = provider_id.into();
        if provider_id.is_empty() {
            return Err(IdError::EmptyProviderId);
        }
        Ok(CatalogId {
            slug,
            kind,
            provider_id,
        })
    }

    /// Returns the provider instance slug that issued the id.
    pub fn slug(&self) -> &Slug {
        &self.slug
    }

    /// Returns the entity kind the id names.
    pub fn kind(&self) -> EntityKind {
        self.kind
    }

    /// Returns the provider's own identifier for the entity.
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
}

impl fmt::Display for CatalogId {
    /// Writes `x_<slug>_<kind letter>_<Crockford payload>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}_{}_{}",
            CATALOG_MARKER,
            self.slug,
            kind_letter(self.kind),
            encode_bytes(self.provider_id.as_bytes())
        )
    }
}

/// Either form of client id, as parsed from a request.
///
/// A router receives ids from clients without knowing which form they are;
/// [`ClientId::parse`] answers that, and the router dispatches on the variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClientId {
    /// A materialized record, addressed by its UUIDv7 primary key.
    Entity(EntityId),
    /// An unmaterialized catalog entity, addressed through its issuing
    /// provider instance.
    Catalog(CatalogId),
}

impl ClientId {
    /// Parses either id form.
    ///
    /// A value starting with `x_` is parsed as a catalog id, everything else
    /// as a materialized entity id. The comparison is case-sensitive: `X_` is
    /// rejected, not treated as a catalog id.
    ///
    /// # Errors
    ///
    /// Returns [`IdError`] when `value` is not a well-formed client id: an
    /// unknown or missing prefix, a payload of the wrong length, a character
    /// outside the lowercase Crockford alphabet, a materialized payload that
    /// does not fit 128 bits, a catalog id that is missing a field or carries
    /// an empty provider id, or an invalid provider instance slug.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::{ClientId, EntityId, IdPrefix};
    ///
    /// let id = EntityId::new(IdPrefix::User);
    /// assert_eq!(ClientId::parse(&id.to_string()).unwrap(), ClientId::Entity(id));
    /// assert!(ClientId::parse("nope").is_err());
    /// ```
    pub fn parse(value: &str) -> Result<ClientId, IdError> {
        match value.strip_prefix(CATALOG_MARKER) {
            Some(rest) => parse_catalog(value, rest).map(ClientId::Catalog),
            None => parse_entity(value).map(ClientId::Entity),
        }
    }
}

impl FromStr for ClientId {
    type Err = IdError;

    /// Parses either id form; see [`ClientId::parse`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ClientId::parse(s)
    }
}

impl From<EntityId> for ClientId {
    fn from(id: EntityId) -> ClientId {
        ClientId::Entity(id)
    }
}

impl From<CatalogId> for ClientId {
    fn from(id: CatalogId) -> ClientId {
        ClientId::Catalog(id)
    }
}

impl fmt::Display for ClientId {
    /// Writes whichever id form this is.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientId::Entity(id) => id.fmt(f),
            ClientId::Catalog(id) => id.fmt(f),
        }
    }
}

/// The error returned when a string is not a well-formed client id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The characters before the first underscore are not a known prefix.
    #[error("unknown client id prefix `{0}`")]
    UnknownPrefix(String),
    /// A materialized id's payload is not exactly 26 characters long.
    #[error("invalid client id payload length {0}: expected 26 characters")]
    InvalidPayloadLength(usize),
    /// A payload contains a character that is not a lowercase Crockford base32
    /// digit.
    #[error("invalid client id character `{0}`: expected a lowercase Crockford base32 digit")]
    InvalidCharacter(char),
    /// A materialized id's first payload character encodes bits the 128-bit
    /// value does not have.
    #[error("client id payload does not fit in a 128-bit identifier")]
    PayloadOverflow,
    /// A catalog id is missing one of its four underscore separated fields.
    #[error("malformed catalog id `{0}`: expected `x_<slug>_<kind>_<payload>`")]
    MalformedCatalogId(String),
    /// A catalog id's payload is empty, so it names no provider id.
    #[error("catalog id names no provider id")]
    EmptyProviderId,
    /// A catalog id's payload is not the canonical encoding of its bytes.
    #[error("catalog id payload has non-zero padding bits")]
    NonCanonicalPayload,
    /// A catalog id's slug is not a valid provider instance slug.
    #[error("invalid catalog id slug: {0}")]
    InvalidSlug(InvalidSlug),
}

/// Parses a materialized id of the form `<prefix>_<payload>`.
///
/// # Errors
///
/// Returns [`IdError::UnknownPrefix`] for an unrecognized prefix and the errors
/// of [`decode_payload`] for a malformed payload.
fn parse_entity(value: &str) -> Result<EntityId, IdError> {
    let (prefix, payload) = value
        .split_once('_')
        .ok_or_else(|| IdError::UnknownPrefix(value.to_owned()))?;
    let kind = IdPrefix::ALL
        .iter()
        .copied()
        .find(|candidate| candidate.as_str() == prefix)
        .ok_or_else(|| IdError::UnknownPrefix(prefix.to_owned()))?;
    Ok(EntityId::from_uuid(kind, decode_payload(payload)?))
}

/// Parses a catalog id whose `x_` marker has already been stripped.
///
/// # Parameters
///
/// - `value`: the whole id, used for error messages.
/// - `rest`: the id after the `x_` marker: `<slug>_<kind>_<payload>`.
///
/// # Errors
///
/// Returns [`IdError::MalformedCatalogId`] when a field is missing or the kind
/// letter is unknown, [`IdError::InvalidSlug`] for a bad slug, and
/// [`IdError::EmptyProviderId`] or [`IdError::NonCanonicalPayload`] for a
/// malformed payload.
fn parse_catalog(value: &str, rest: &str) -> Result<CatalogId, IdError> {
    let fields: Vec<&str> = rest.splitn(3, '_').collect();
    let [slug, kind, payload] = fields.as_slice() else {
        return Err(IdError::MalformedCatalogId(value.to_owned()));
    };
    let slug = Slug::new(slug).map_err(IdError::InvalidSlug)?;
    let kind = match *kind {
        "t" => EntityKind::Track,
        "a" => EntityKind::Album,
        "r" => EntityKind::Artist,
        "p" => EntityKind::Playlist,
        _ => return Err(IdError::MalformedCatalogId(value.to_owned())),
    };
    let provider_id = String::from_utf8(decode_bytes(payload)?)
        .map_err(|_| IdError::MalformedCatalogId(value.to_owned()))?;
    CatalogId::new(slug, kind, provider_id)
}

/// Returns the single letter a catalog id uses for `kind`.
///
/// # Examples
///
/// This is the inverse of the mapping applied by [`parse_catalog`].
fn kind_letter(kind: EntityKind) -> char {
    match kind {
        EntityKind::Track => 't',
        EntityKind::Album => 'a',
        EntityKind::Artist => 'r',
        EntityKind::Playlist => 'p',
    }
}

/// Returns the digit value of a lowercase Crockford base32 character.
///
/// The letters `i`, `l`, `o` and `u` and every uppercase letter map to
/// [`None`], so they are rejected rather than folded to their lookalike digits.
fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'c' => Some(byte - b'a' + 10),
        b'd'..=b'h' => Some(byte - b'd' + 13),
        b'j'..=b'k' => Some(byte - b'j' + 18),
        b'm'..=b'n' => Some(byte - b'm' + 20),
        b'p'..=b't' => Some(byte - b'p' + 22),
        b'v'..=b'z' => Some(byte - b'v' + 27),
        _ => None,
    }
}

/// Decodes the 26-character payload of a materialized id.
///
/// The payload is the UUIDv7 left zero padded to 130 bits, so the two leading
/// bits of the first character must be zero and the remaining 128 bits are the
/// primary key.
///
/// # Errors
///
/// Returns [`IdError::InvalidPayloadLength`] when the payload is not 26 bytes,
/// [`IdError::InvalidCharacter`] for a character outside the alphabet, and
/// [`IdError::PayloadOverflow`] when the first character encodes bits above
/// the 128-bit value.
fn decode_payload(payload: &str) -> Result<Uuid, IdError> {
    let bytes = payload.as_bytes();
    if bytes.len() != PAYLOAD_LEN {
        return Err(IdError::InvalidPayloadLength(bytes.len()));
    }
    let mut value: u128 = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        let digit = digit(byte).ok_or(IdError::InvalidCharacter(char::from(byte)))?;
        if index == 0 && digit > 7 {
            return Err(IdError::PayloadOverflow);
        }
        value = (value << 5) | u128::from(digit);
    }
    Ok(Uuid::from_u128(value))
}

/// Encodes bytes as lowercase Crockford base32 with no padding to a fixed
/// width.
///
/// Digits are emitted most significant bit first; the final character carries
/// the remaining bits left aligned with zero padding bits, exactly as RFC 4648
/// base32 without the `=` characters. Because the character count is derived
/// from the byte count, the encoding is reversible including leading zero
/// bytes.
fn encode_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 0x1f) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(
            ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize],
        ));
    }
    out
}

/// Decodes the Crockford base32 payload of a catalog id back to bytes.
///
/// # Errors
///
/// Returns [`IdError::InvalidCharacter`] for a character outside the alphabet,
/// [`IdError::EmptyProviderId`] for an empty payload, and
/// [`IdError::NonCanonicalPayload`] when the padding bits of the final
/// character are not zero (which would let two ids name the same bytes).
fn decode_bytes(payload: &str) -> Result<Vec<u8>, IdError> {
    if payload.is_empty() {
        return Err(IdError::EmptyProviderId);
    }
    let mut out = Vec::with_capacity(payload.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in payload.as_bytes() {
        let digit = digit(byte).ok_or(IdError::InvalidCharacter(char::from(byte)))?;
        buffer = (buffer << 5) | u32::from(digit);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    if bits > 0 && (buffer & ((1 << bits) - 1)) != 0 {
        return Err(IdError::NonCanonicalPayload);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload of the all-zero UUIDv7.
    const ZERO_PAYLOAD: &str = "00000000000000000000000000";

    #[test]
    fn materialized_ids_round_trip() {
        let uuid = Uuid::from_u128(0x0194_2a00_0000_7000_8000_0000_0000_0001);
        for kind in IdPrefix::ALL {
            let id = EntityId::from_uuid(kind, uuid);
            let text = id.to_string();
            assert_eq!(text.len(), kind.as_str().len() + 1 + PAYLOAD_LEN);
            assert!(text.starts_with(kind.as_str()));
            assert_eq!(ClientId::parse(&text).unwrap(), ClientId::Entity(id));
            assert_eq!(text.parse::<ClientId>().unwrap(), ClientId::Entity(id));
        }
    }

    #[test]
    fn generated_ids_are_version_7() {
        let first = new_uuid_v7();
        let second = new_uuid_v7();
        assert_eq!(first.get_version_num(), 7);
        assert_eq!(second.get_version_num(), 7);
        assert_ne!(first, second);
    }

    #[test]
    fn encodes_boundary_uuids() {
        let zero = EntityId::from_uuid(IdPrefix::Track, Uuid::nil());
        assert_eq!(zero.to_string(), format!("tr_{ZERO_PAYLOAD}"));

        let max = EntityId::from_uuid(IdPrefix::Track, Uuid::from_u128(u128::MAX));
        let expected = format!("tr_7{}", "z".repeat(PAYLOAD_LEN - 1));
        assert_eq!(max.to_string(), expected);
        assert_eq!(ClientId::parse(&expected).unwrap(), ClientId::Entity(max));
    }

    #[test]
    fn rejects_materialized_ids_with_bad_prefixes() {
        assert_eq!(
            ClientId::parse("zz_00000000000000000000000000"),
            Err(IdError::UnknownPrefix("zz".to_owned()))
        );
        assert_eq!(
            ClientId::parse("TR_00000000000000000000000000"),
            Err(IdError::UnknownPrefix("TR".to_owned()))
        );
        assert_eq!(
            ClientId::parse("tr"),
            Err(IdError::UnknownPrefix("tr".to_owned()))
        );
        assert_eq!(
            ClientId::parse("_00000000000000000000000000"),
            Err(IdError::UnknownPrefix(String::new()))
        );
    }

    #[test]
    fn rejects_materialized_ids_with_bad_lengths() {
        let short = format!("tr_{}", "0".repeat(PAYLOAD_LEN - 1));
        assert_eq!(
            ClientId::parse(&short),
            Err(IdError::InvalidPayloadLength(PAYLOAD_LEN - 1))
        );
        let long = format!("tr_{}{}", "0".repeat(PAYLOAD_LEN), "0");
        assert_eq!(
            ClientId::parse(&long),
            Err(IdError::InvalidPayloadLength(PAYLOAD_LEN + 1))
        );
        assert_eq!(
            ClientId::parse("tr_"),
            Err(IdError::InvalidPayloadLength(0))
        );
    }

    #[test]
    fn rejects_characters_outside_the_alphabet() {
        for bad in ['i', 'l', 'o', 'u', 'I', 'L', 'O', 'U', 'Z', '-', '_'] {
            let payload = format!("00{bad}{}", "0".repeat(PAYLOAD_LEN - 3));
            assert_eq!(
                ClientId::parse(&format!("tr_{payload}")),
                Err(IdError::InvalidCharacter(bad)),
                "payload {payload} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_payloads_above_128_bits() {
        assert_eq!(
            ClientId::parse("tr_80000000000000000000000000"),
            Err(IdError::PayloadOverflow)
        );
        assert_eq!(
            ClientId::parse("tr_z0000000000000000000000000"),
            Err(IdError::PayloadOverflow)
        );
    }

    #[test]
    fn catalog_ids_round_trip() {
        let slug = Slug::new("deezer").unwrap();
        let provider_ids = [
            "3135556",
            "0",
            "4a2b7c-9f",
            "a_b.c",
            "Bj\u{f6}rk - J\u{f3}ga (Live) [Remastered]",
            "\u{7}bell \u{1f600}",
        ];
        for kind in [
            EntityKind::Track,
            EntityKind::Album,
            EntityKind::Artist,
            EntityKind::Playlist,
        ] {
            for provider_id in provider_ids {
                let id = CatalogId::new(slug.clone(), kind, provider_id).unwrap();
                let text = id.to_string();
                assert!(text.starts_with(&format!("x_deezer_{}_", kind_letter(kind))));
                assert_eq!(
                    ClientId::parse(&text).unwrap(),
                    ClientId::Catalog(id.clone())
                );
                assert_eq!(text.parse::<ClientId>().unwrap(), ClientId::Catalog(id));
            }
        }
    }

    #[test]
    fn encodes_catalog_payloads_exactly() {
        let slug = Slug::new("deezer").unwrap();
        let id = CatalogId::new(slug.clone(), EntityKind::Track, "3135556").unwrap();
        assert_eq!(id.to_string(), "x_deezer_t_6crk6d9n6mv0");
        let id = CatalogId::new(slug.clone(), EntityKind::Playlist, "a_b.c").unwrap();
        assert_eq!(id.to_string(), "x_deezer_p_c5fp4bk3");
        let id = CatalogId::new(slug, EntityKind::Album, "0").unwrap();
        assert_eq!(id.to_string(), "x_deezer_a_60");
    }

    #[test]
    fn rejects_malformed_catalog_ids() {
        assert_eq!(
            ClientId::parse("x_deezer_t"),
            Err(IdError::MalformedCatalogId("x_deezer_t".to_owned()))
        );
        assert_eq!(
            ClientId::parse("x_deezer_x_60"),
            Err(IdError::MalformedCatalogId("x_deezer_x_60".to_owned()))
        );
        assert_eq!(
            ClientId::parse("x_deezer_tt_60"),
            Err(IdError::MalformedCatalogId("x_deezer_tt_60".to_owned()))
        );
        assert!(matches!(
            ClientId::parse("x_Deezer_t_60"),
            Err(IdError::InvalidSlug(_))
        ));
        assert!(matches!(
            ClientId::parse("x__t_60"),
            Err(IdError::InvalidSlug(_))
        ));
        assert_eq!(
            ClientId::parse("x_deezer_t_"),
            Err(IdError::EmptyProviderId)
        );
        assert_eq!(
            CatalogId::new(Slug::new("deezer").unwrap(), EntityKind::Track, ""),
            Err(IdError::EmptyProviderId)
        );
    }

    #[test]
    fn rejects_payloads_that_are_not_canonical() {
        // A single character carries five bits, so both padding bits are
        // non-zero: it cannot be the encoding of any byte string.
        assert_eq!(
            ClientId::parse("x_deezer_t_1"),
            Err(IdError::NonCanonicalPayload)
        );
        assert_eq!(
            ClientId::parse("x_deezer_t_63"),
            Err(IdError::NonCanonicalPayload)
        );
        // `60` is the canonical one-byte encoding of `0x30`.
        assert_eq!(
            ClientId::parse("x_deezer_t_60").unwrap(),
            ClientId::Catalog(
                CatalogId::new(Slug::new("deezer").unwrap(), EntityKind::Track, "0").unwrap()
            )
        );
    }

    #[test]
    fn rejects_uppercase_catalog_payloads() {
        assert_eq!(
            ClientId::parse("x_deezer_t_6CRK6D9N6MV0"),
            Err(IdError::InvalidCharacter('C'))
        );
        assert_eq!(
            ClientId::parse("X_deezer_t_6crk6d9n6mv0"),
            Err(IdError::UnknownPrefix("X".to_owned()))
        );
    }

    #[test]
    fn dispatches_on_the_id_form() {
        let entity = ClientId::parse(&format!("us_{ZERO_PAYLOAD}")).unwrap();
        assert!(matches!(entity, ClientId::Entity(_)));
        assert_eq!(entity.to_string(), format!("us_{ZERO_PAYLOAD}"));

        let catalog = ClientId::parse("x_slskd_p_c5fp4bk3").unwrap();
        assert!(matches!(catalog, ClientId::Catalog(_)));

        assert_eq!(
            ClientId::from(EntityId::from_uuid(IdPrefix::User, Uuid::nil())).to_string(),
            format!("us_{ZERO_PAYLOAD}")
        );
    }

    #[test]
    fn maps_prefixes_to_entity_kinds() {
        for kind in [
            EntityKind::Track,
            EntityKind::Album,
            EntityKind::Artist,
            EntityKind::Playlist,
        ] {
            let prefix = IdPrefix::from_entity_kind(kind);
            assert_eq!(prefix.entity_kind(), Some(kind));
        }
        assert_eq!(IdPrefix::User.entity_kind(), None);
        assert_eq!(IdPrefix::Acquisition.entity_kind(), None);
    }
}
