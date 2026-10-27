//! The client id codec.
//!
//! Clients never see the raw UUIDv7 primary keys the database stores. Every
//! record a client can name has a deterministic, URL-safe string id derived
//! from its key: `<prefix>_<payload>`, where the prefix names the record kind
//! and the payload is the lowercase Crockford base32 encoding of the record's
//! UUIDv7, left zero padded to exactly 26 characters (130 bits, of which the
//! leading two are always zero).
//!
//! Crockford base32 excludes `i`, `l`, `o` and `u`, which removes the common
//! visual ambiguities; only the lowercase form is accepted and produced.
//!
//! There is one id form: an entity either has a row or it does not, and every
//! provider's own identifier for it is a namespace and a value on that row, not
//! part of the id.
//!
//! # Examples
//!
//! ```
//! use hearthsound_core::{EntityId, IdPrefix};
//!
//! let track = EntityId::new(IdPrefix::Track);
//! let text = track.to_string();
//! assert!(text.starts_with("tr_"));
//! assert_eq!(EntityId::parse(&text).unwrap(), track);
//! ```

use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

use crate::EntityKind;

/// The lowercase Crockford base32 alphabet.
///
/// The alphabet is `0-9` followed by the letters of the Latin alphabet except
/// `i`, `l`, `o` and `u`. Index equals digit value.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The number of characters in an id's payload.
///
/// 26 characters carry 130 bits, which covers the 128 bits of a UUIDv7 with
/// two leading zero bits.
const PAYLOAD_LEN: usize = 26;

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

/// The prefix of an entity's client id.
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
    /// A plugin repository (`pr`).
    PluginRepository,
}

impl IdPrefix {
    /// Every prefix, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [IdPrefix; 11] = [
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
        IdPrefix::PluginRepository,
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
            IdPrefix::PluginRepository => "pr",
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
            | IdPrefix::Upload
            | IdPrefix::PluginRepository => None,
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

/// An entity's client id.
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

impl EntityId {
    /// Parses `<prefix>_<26 character Crockford payload>` into an id.
    ///
    /// # Parameters
    ///
    /// - `value`: the id as a client sends it, for example `tr_01j9z3k4m5n6p7q8r9s0t1v2w3`.
    ///
    /// # Returns
    ///
    /// The id when `value` names a known record kind.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::UnknownPrefix`] for a prefix that names no record
    /// kind, and the payload errors of [`EntityId::parse`]'s decoder:
    /// [`IdError::InvalidPayloadLength`], [`IdError::InvalidCharacter`] and
    /// [`IdError::PayloadOverflow`].
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::{EntityId, IdPrefix};
    ///
    /// let id = EntityId::new(IdPrefix::Track);
    /// assert_eq!(EntityId::parse(&id.to_string()).unwrap(), id);
    /// assert!(EntityId::parse("zz_00000000000000000000000000").is_err());
    /// ```
    pub fn parse(value: &str) -> Result<EntityId, IdError> {
        parse_entity(value)
    }
}

impl FromStr for EntityId {
    type Err = IdError;

    /// Parses an entity id; see [`EntityId::parse`].
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        EntityId::parse(value)
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

/// The error returned when a string is not a well-formed client id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The characters before the first underscore are not a known prefix.
    #[error("unknown client id prefix `{0}`")]
    UnknownPrefix(String),
    /// An id's payload is not exactly 26 characters long.
    #[error("invalid client id payload length {0}: expected 26 characters")]
    InvalidPayloadLength(usize),
    /// A payload contains a character that is not a lowercase Crockford base32
    /// digit.
    #[error("invalid client id character `{0}`: expected a lowercase Crockford base32 digit")]
    InvalidCharacter(char),
    /// An id's first payload character encodes bits the 128-bit
    /// value does not have.
    #[error("client id payload does not fit in a 128-bit identifier")]
    PayloadOverflow,
}

/// Parses an id of the form `<prefix>_<payload>`.
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

/// Decodes the 26-character payload of an id.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload of the all-zero UUIDv7.
    const ZERO_PAYLOAD: &str = "00000000000000000000000000";

    #[test]
    fn ids_round_trip() {
        let uuid = Uuid::from_u128(0x0194_2a00_0000_7000_8000_0000_0000_0001);
        for kind in IdPrefix::ALL {
            let id = EntityId::from_uuid(kind, uuid);
            let text = id.to_string();
            assert_eq!(text.len(), kind.as_str().len() + 1 + PAYLOAD_LEN);
            assert!(text.starts_with(kind.as_str()));
            assert_eq!(EntityId::parse(&text).unwrap(), id);
            assert_eq!(text.parse::<EntityId>().unwrap(), id);
        }
    }

    #[test]
    fn plugin_repository_ids_carry_the_pr_prefix() {
        let id = EntityId::new(IdPrefix::PluginRepository);
        let text = id.to_string();
        assert_eq!(IdPrefix::PluginRepository.as_str(), "pr");
        assert!(text.starts_with("pr_"));
        assert_eq!(text.len(), 3 + PAYLOAD_LEN);
        assert_eq!(EntityId::parse(&text).unwrap(), id);
        assert_eq!(IdPrefix::PluginRepository.entity_kind(), None);
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
        assert_eq!(EntityId::parse(&expected).unwrap(), max);
    }

    #[test]
    fn rejects_ids_with_bad_prefixes() {
        assert_eq!(
            EntityId::parse("zz_00000000000000000000000000"),
            Err(IdError::UnknownPrefix("zz".to_owned()))
        );
        assert_eq!(
            EntityId::parse("TR_00000000000000000000000000"),
            Err(IdError::UnknownPrefix("TR".to_owned()))
        );
        assert_eq!(
            EntityId::parse("tr"),
            Err(IdError::UnknownPrefix("tr".to_owned()))
        );
        assert_eq!(
            EntityId::parse("_00000000000000000000000000"),
            Err(IdError::UnknownPrefix(String::new()))
        );
    }

    #[test]
    fn rejects_ids_with_bad_lengths() {
        let short = format!("tr_{}", "0".repeat(PAYLOAD_LEN - 1));
        assert_eq!(
            EntityId::parse(&short),
            Err(IdError::InvalidPayloadLength(PAYLOAD_LEN - 1))
        );
        let long = format!("tr_{}{}", "0".repeat(PAYLOAD_LEN), "0");
        assert_eq!(
            EntityId::parse(&long),
            Err(IdError::InvalidPayloadLength(PAYLOAD_LEN + 1))
        );
        assert_eq!(
            EntityId::parse("tr_"),
            Err(IdError::InvalidPayloadLength(0))
        );
    }

    #[test]
    fn rejects_characters_outside_the_alphabet() {
        for bad in ['i', 'l', 'o', 'u', 'I', 'L', 'O', 'U', 'Z', '-', '_'] {
            let payload = format!("00{bad}{}", "0".repeat(PAYLOAD_LEN - 3));
            assert_eq!(
                EntityId::parse(&format!("tr_{payload}")),
                Err(IdError::InvalidCharacter(bad)),
                "payload {payload} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_payloads_above_128_bits() {
        assert_eq!(
            EntityId::parse("tr_80000000000000000000000000"),
            Err(IdError::PayloadOverflow)
        );
        assert_eq!(
            EntityId::parse("tr_z0000000000000000000000000"),
            Err(IdError::PayloadOverflow)
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
