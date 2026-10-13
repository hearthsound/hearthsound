//! The per-track [`Availability`] state.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The availability of a track to this server.
///
/// Every track a user can see is in exactly one of these five states. The
/// literals are fixed by the product specification and are the values stored in
/// `tracks.availability`, so they must not be renamed.
///
/// The state machine behind them is `remote -> acquiring -> local | cached |
/// unavailable`, with `local <-> cached` on pin changes and `cached -> remote`
/// on cache eviction.
///
/// # Examples
///
/// ```
/// use hearthsound_core::Availability;
///
/// assert_eq!(Availability::Remote.as_str(), "remote");
/// assert_eq!("cached".parse::<Availability>().unwrap(), Availability::Cached);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Availability {
    /// A permanent managed file exists on disk.
    Local,
    /// An evictable managed file exists on disk.
    Cached,
    /// An acquisition is in flight for the track.
    Acquiring,
    /// The track exists in a provider catalog only; no file on this server.
    Remote,
    /// The last acquisition failed; a new search waits for the cooldown.
    Unavailable,
}

impl Availability {
    /// Every availability, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [Availability; 5] = [
        Availability::Local,
        Availability::Cached,
        Availability::Acquiring,
        Availability::Remote,
        Availability::Unavailable,
    ];

    /// Returns the literal written to the database and to event payloads.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::Availability;
    ///
    /// assert_eq!(Availability::Unavailable.as_str(), "unavailable");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            Availability::Local => "local",
            Availability::Cached => "cached",
            Availability::Acquiring => "acquiring",
            Availability::Remote => "remote",
            Availability::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for Availability {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`Availability`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown availability `{0}`")]
pub struct UnknownAvailability(pub String);

impl FromStr for Availability {
    type Err = UnknownAvailability;

    /// Parses an availability from its literal.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownAvailability`] when `s` is not one of the literals
    /// produced by [`Availability::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|availability| availability.as_str() == s)
            .ok_or_else(|| UnknownAvailability(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_match_the_specification() {
        assert_eq!(
            Availability::ALL.map(Availability::as_str),
            ["local", "cached", "acquiring", "remote", "unavailable"]
        );
    }

    #[test]
    fn literals_round_trip() {
        for availability in Availability::ALL {
            assert_eq!(
                availability.as_str().parse::<Availability>().unwrap(),
                availability
            );
            assert_eq!(availability.to_string(), availability.as_str());
            let json = serde_json::to_string(&availability).unwrap();
            assert_eq!(json, format!("\"{availability}\""));
            assert_eq!(
                serde_json::from_str::<Availability>(&json).unwrap(),
                availability
            );
        }
    }

    #[test]
    fn rejects_unknown_literals() {
        assert_eq!(
            "Local".parse::<Availability>(),
            Err(UnknownAvailability("Local".to_owned()))
        );
        assert!("streaming".parse::<Availability>().is_err());
        assert!(serde_json::from_str::<Availability>("\"Local\"").is_err());
    }
}
