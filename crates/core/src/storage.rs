//! The retention and storage literals: [`PinReason`], [`ImportMode`] and
//! [`StorageClass`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A reason a track's managed file is permanent.
///
/// `track_retention.pin_reasons` holds the active reasons as a JSON array, and
/// `track_retention.pinned` is set when at least one is active. A track's
/// managed file is permanent if and only if the track is pinned; an unpinned
/// managed file is a cache file and is evictable. The literals are fixed by the
/// data model, so they must not be renamed.
///
/// Retention is maintained in the same transaction as every pin-changing write,
/// so the retention view never disagrees with the rows that justify it.
///
/// # Examples
///
/// ```
/// use hearthsound_core::PinReason;
///
/// assert_eq!(PinReason::UserLibrary.as_str(), "user-library");
/// assert_eq!("policy".parse::<PinReason>().unwrap(), PinReason::Policy);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PinReason {
    /// The track is saved in a user's personal library.
    UserLibrary,
    /// The track is on a non-temporary playlist.
    Playlist,
    /// The track was uploaded into or associated with a library folder.
    Folder,
    /// An administrator pinned the track explicitly.
    Admin,
    /// A storage policy pinned the track.
    Policy,
}

impl PinReason {
    /// Every pin reason, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [PinReason; 5] = [
        PinReason::UserLibrary,
        PinReason::Playlist,
        PinReason::Folder,
        PinReason::Admin,
        PinReason::Policy,
    ];

    /// Returns the literal stored in `track_retention.pin_reasons`.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::PinReason;
    ///
    /// assert_eq!(PinReason::Admin.as_str(), "admin");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            PinReason::UserLibrary => "user-library",
            PinReason::Playlist => "playlist",
            PinReason::Folder => "folder",
            PinReason::Admin => "admin",
            PinReason::Policy => "policy",
        }
    }
}

impl fmt::Display for PinReason {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`PinReason`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown pin reason `{0}`")]
pub struct UnknownPinReason(pub String);

impl FromStr for PinReason {
    type Err = UnknownPinReason;

    /// Parses a pin reason from its literal.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownPinReason`] when `s` is not one of the literals
    /// produced by [`PinReason::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|reason| reason.as_str() == s)
            .ok_or_else(|| UnknownPinReason(s.to_owned()))
    }
}

/// The post-import action of an import root.
///
/// An import root is a configured backend directory such as slskd's downloads
/// directory. Its mode decides what happens to a file after it has been
/// imported into managed storage. The literals are fixed by the acquisition
/// design, so they must not be renamed.
///
/// # Examples
///
/// ```
/// use hearthsound_core::ImportMode;
///
/// assert_eq!(ImportMode::Link.as_str(), "link");
/// assert_eq!("copy".parse::<ImportMode>().unwrap(), ImportMode::Copy);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportMode {
    /// Remove the imported file from the import root, and remove the emptied
    /// parent directories up to the root.
    Move,
    /// Leave the file in the import root untouched.
    Copy,
    /// Hardlink the file into managed storage when the filesystem supports it,
    /// falling back to a copy, and leave the original untouched.
    Link,
}

impl ImportMode {
    /// Every import mode, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [ImportMode; 3] = [ImportMode::Move, ImportMode::Copy, ImportMode::Link];

    /// Returns the literal of this mode.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::ImportMode;
    ///
    /// assert_eq!(ImportMode::Move.as_str(), "move");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            ImportMode::Move => "move",
            ImportMode::Copy => "copy",
            ImportMode::Link => "link",
        }
    }
}

impl fmt::Display for ImportMode {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`ImportMode`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown import mode `{0}`")]
pub struct UnknownImportMode(pub String);

impl FromStr for ImportMode {
    type Err = UnknownImportMode;

    /// Parses an import mode from its literal.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownImportMode`] when `s` is not one of the literals
    /// produced by [`ImportMode::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|mode| mode.as_str() == s)
            .ok_or_else(|| UnknownImportMode(s.to_owned()))
    }
}

/// Where a media file lives.
///
/// A track may have several media files across storage classes; exactly one is
/// marked preferred. The literals are the values stored in
/// `media_files.storage`, so they must not be renamed.
///
/// # Examples
///
/// ```
/// use hearthsound_core::StorageClass;
///
/// assert_eq!(StorageClass::Cache.as_str(), "cache");
/// assert_eq!("folder".parse::<StorageClass>().unwrap(), StorageClass::Folder);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageClass {
    /// A file inside a scanned folder. Only a save into an owned folder writes
    /// one, and nothing else mutates it.
    Folder,
    /// A permanent managed file under `<storage>/library`.
    Library,
    /// An evictable managed file under `<storage>/cache`.
    Cache,
}

impl StorageClass {
    /// Every storage class, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [StorageClass; 3] = [
        StorageClass::Folder,
        StorageClass::Library,
        StorageClass::Cache,
    ];

    /// Returns the literal stored in `media_files.storage`.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::StorageClass;
    ///
    /// assert_eq!(StorageClass::Library.as_str(), "library");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            StorageClass::Folder => "folder",
            StorageClass::Library => "library",
            StorageClass::Cache => "cache",
        }
    }
}

impl fmt::Display for StorageClass {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`StorageClass`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown storage class `{0}`")]
pub struct UnknownStorageClass(pub String);

impl FromStr for StorageClass {
    type Err = UnknownStorageClass;

    /// Parses a storage class from its literal.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownStorageClass`] when `s` is not one of the literals
    /// produced by [`StorageClass::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|class| class.as_str() == s)
            .ok_or_else(|| UnknownStorageClass(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_match_the_specification() {
        assert_eq!(
            PinReason::ALL.map(PinReason::as_str),
            ["user-library", "playlist", "folder", "admin", "policy"]
        );
        assert_eq!(
            ImportMode::ALL.map(ImportMode::as_str),
            ["move", "copy", "link"]
        );
        assert_eq!(
            StorageClass::ALL.map(StorageClass::as_str),
            ["folder", "library", "cache"]
        );
    }

    #[test]
    fn literals_round_trip() {
        fn check<T>(value: T, literal: &str)
        where
            T: Copy + std::fmt::Debug + PartialEq + Serialize + serde::de::DeserializeOwned,
        {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(json, format!("\"{literal}\""));
            assert_eq!(serde_json::from_str::<T>(&json).unwrap(), value);
        }

        for reason in PinReason::ALL {
            check(reason, reason.as_str());
            assert_eq!(reason.to_string(), reason.as_str());
            assert_eq!(reason.as_str().parse::<PinReason>().unwrap(), reason);
        }
        for mode in ImportMode::ALL {
            check(mode, mode.as_str());
            assert_eq!(mode.to_string(), mode.as_str());
            assert_eq!(mode.as_str().parse::<ImportMode>().unwrap(), mode);
        }
        for class in StorageClass::ALL {
            check(class, class.as_str());
            assert_eq!(class.to_string(), class.as_str());
            assert_eq!(class.as_str().parse::<StorageClass>().unwrap(), class);
        }
    }

    #[test]
    fn rejects_unknown_literals() {
        assert_eq!(
            "UserLibrary".parse::<PinReason>(),
            Err(UnknownPinReason("UserLibrary".to_owned()))
        );
        assert!(matches!(
            "readOnly".parse::<ImportMode>(),
            Err(UnknownImportMode(_))
        ));
        assert!(matches!(
            "staging".parse::<StorageClass>(),
            Err(UnknownStorageClass(_))
        ));
        assert!(serde_json::from_str::<PinReason>("\"user_library\"").is_err());
    }
}
