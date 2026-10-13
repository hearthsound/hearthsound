//! The acquisition [`AcquisitionState`], [`Priority`], [`FailureReason`] and
//! [`Granularity`] types.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The state of an acquisition in the acquisition state machine.
///
/// An acquisition moves from [`AcquisitionState::Queued`] through resolution,
/// search, download, verification and import; most states may end in
/// [`AcquisitionState::Failed`], and every state that holds backend work may be
/// cancelled. A failed candidate does not fail the acquisition: the job returns
/// to [`AcquisitionState::Searching`] for the next candidate.
///
/// The literals are fixed by the architecture and are the values stored in
/// `acquisitions.state`, so they must not be renamed.
///
/// # Examples
///
/// ```
/// use hearthsound_core::AcquisitionState;
///
/// assert!(AcquisitionState::Queued.can_transition_to(AcquisitionState::Resolving));
/// assert!(!AcquisitionState::Completed.can_transition_to(AcquisitionState::Failed));
/// assert!(AcquisitionState::Completed.is_terminal());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcquisitionState {
    /// Admitted and waiting for a worker slot.
    Queued,
    /// Resolving the wanted track to canonical identifiers.
    Resolving,
    /// Searching the acquisition instances in layer order.
    Searching,
    /// Downloading an accepted candidate.
    Downloading,
    /// Verifying the downloaded file.
    Verifying,
    /// Importing the verified file into managed storage.
    Importing,
    /// Imported; the track has a managed file.
    Completed,
    /// No verified file could be produced.
    Failed,
    /// A user or the platform cancelled the acquisition.
    Cancelled,
}

impl AcquisitionState {
    /// Every state, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases. It is the order of the state machine, except that the three
    /// terminal states come last.
    pub const ALL: [AcquisitionState; 9] = [
        AcquisitionState::Queued,
        AcquisitionState::Resolving,
        AcquisitionState::Searching,
        AcquisitionState::Downloading,
        AcquisitionState::Verifying,
        AcquisitionState::Importing,
        AcquisitionState::Completed,
        AcquisitionState::Failed,
        AcquisitionState::Cancelled,
    ];

    /// Returns the literal written to the database and to event payloads.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::AcquisitionState;
    ///
    /// assert_eq!(AcquisitionState::Verifying.as_str(), "verifying");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            AcquisitionState::Queued => "queued",
            AcquisitionState::Resolving => "resolving",
            AcquisitionState::Searching => "searching",
            AcquisitionState::Downloading => "downloading",
            AcquisitionState::Verifying => "verifying",
            AcquisitionState::Importing => "importing",
            AcquisitionState::Completed => "completed",
            AcquisitionState::Failed => "failed",
            AcquisitionState::Cancelled => "cancelled",
        }
    }

    /// Returns whether this state ends the acquisition.
    ///
    /// # Returns
    ///
    /// `true` for [`AcquisitionState::Completed`], [`AcquisitionState::Failed`]
    /// and [`AcquisitionState::Cancelled`], which have no outgoing transitions.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            AcquisitionState::Completed | AcquisitionState::Failed | AcquisitionState::Cancelled
        )
    }

    /// Returns whether the state machine allows `self -> next`.
    ///
    /// The legal transitions are exactly the arrows of the acquisition state
    /// diagram:
    ///
    /// - `queued -> resolving | failed`
    /// - `resolving -> searching | failed`
    /// - `searching -> downloading | failed | cancelled`
    /// - `downloading -> verifying | searching | failed | cancelled`
    /// - `verifying -> importing | failed | cancelled`
    /// - `importing -> completed | failed | cancelled`
    ///
    /// `downloading -> searching` is the retry edge: a candidate that fails to
    /// download or verify sends the job back to search for the next candidate.
    /// Terminal states have no outgoing transitions, and a state never
    /// transitions to itself.
    ///
    /// # Parameters
    ///
    /// - `next`: the state the acquisition would move to.
    ///
    /// # Returns
    ///
    /// `true` when the transition is legal.
    pub const fn can_transition_to(self, next: AcquisitionState) -> bool {
        matches!(
            (self, next),
            (
                AcquisitionState::Queued,
                AcquisitionState::Resolving | AcquisitionState::Failed
            ) | (
                AcquisitionState::Resolving,
                AcquisitionState::Searching | AcquisitionState::Failed
            ) | (
                AcquisitionState::Searching,
                AcquisitionState::Downloading
                    | AcquisitionState::Failed
                    | AcquisitionState::Cancelled
            ) | (
                AcquisitionState::Downloading,
                AcquisitionState::Verifying
                    | AcquisitionState::Searching
                    | AcquisitionState::Failed
                    | AcquisitionState::Cancelled
            ) | (
                AcquisitionState::Verifying,
                AcquisitionState::Importing
                    | AcquisitionState::Failed
                    | AcquisitionState::Cancelled
            ) | (
                AcquisitionState::Importing,
                AcquisitionState::Completed
                    | AcquisitionState::Failed
                    | AcquisitionState::Cancelled
            )
        )
    }
}

impl fmt::Display for AcquisitionState {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`AcquisitionState`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown acquisition state `{0}`")]
pub struct UnknownAcquisitionState(pub String);

impl FromStr for AcquisitionState {
    type Err = UnknownAcquisitionState;

    /// Parses an acquisition state from its literal.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownAcquisitionState`] when `s` is not one of the literals
    /// produced by [`AcquisitionState::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| UnknownAcquisitionState(s.to_owned()))
    }
}

/// The scheduling priority of an acquisition.
///
/// The variants are declared most urgent first, so the derived [`Ord`] treats
/// [`Priority::Play`] as the smallest value: a request that is waiting now sorts
/// before every background run. Each variant has a level, `P0` for
/// [`Priority::Play`] through `P4` for [`Priority::Upgrade`].
///
/// The literals are fixed by the architecture and are the values stored in
/// `acquisitions.priority`, so they must not be renamed.
///
/// # Examples
///
/// ```
/// use hearthsound_core::Priority;
///
/// assert!(Priority::Play < Priority::Background);
/// assert_eq!(Priority::Queue.level(), 2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Priority {
    /// A user is waiting for audio right now.
    Play,
    /// The next queue item.
    Next,
    /// Later queue items, saves and playlist additions.
    Queue,
    /// Scheduled pre-acquisition and discovery runs.
    Background,
    /// Automatic quality upgrades.
    Upgrade,
}

impl Priority {
    /// Every priority, most urgent first.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [Priority; 5] = [
        Priority::Play,
        Priority::Next,
        Priority::Queue,
        Priority::Background,
        Priority::Upgrade,
    ];

    /// Returns the literal written to the database and to event payloads.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::Priority;
    ///
    /// assert_eq!(Priority::Background.as_str(), "background");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            Priority::Play => "play",
            Priority::Next => "next",
            Priority::Queue => "queue",
            Priority::Background => "background",
            Priority::Upgrade => "upgrade",
        }
    }

    /// Returns the priority level, `P0` through `P4`.
    ///
    /// # Returns
    ///
    /// `0` for [`Priority::Play`] up to `4` for [`Priority::Upgrade`]. A lower
    /// level is more urgent.
    pub const fn level(self) -> u8 {
        match self {
            Priority::Play => 0,
            Priority::Next => 1,
            Priority::Queue => 2,
            Priority::Background => 3,
            Priority::Upgrade => 4,
        }
    }
}

impl fmt::Display for Priority {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`Priority`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown acquisition priority `{0}`")]
pub struct UnknownPriority(pub String);

impl FromStr for Priority {
    type Err = UnknownPriority;

    /// Parses a priority from its literal.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownPriority`] when `s` is not one of the literals produced
    /// by [`Priority::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|priority| priority.as_str() == s)
            .ok_or_else(|| UnknownPriority(s.to_owned()))
    }
}

/// Why an acquisition failed.
///
/// Exactly one of these literals is recorded in `acquisitions.failure_reason`
/// when an acquisition ends in the `failed` state, and the same literals are
/// the `code` member of the native API's problem documents, so they must not be
/// renamed.
///
/// # Examples
///
/// ```
/// use hearthsound_core::FailureReason;
///
/// assert_eq!(FailureReason::QuotaExceeded.as_str(), "quota-exceeded");
/// assert_eq!("timeout".parse::<FailureReason>().unwrap(), FailureReason::Timeout);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureReason {
    /// The search completed with no candidate, or no candidate passed scoring.
    NoCandidates,
    /// Candidates existed but none met the quality profile's minimum tier.
    NoAcceptableQuality,
    /// Every attempted candidate failed to download or to verify.
    AllCandidatesFailed,
    /// The file failed verification and no further candidate was available.
    VerificationFailed,
    /// The acquisition instance was unreachable.
    BackendUnavailable,
    /// The user's daily acquisition quota was exhausted.
    QuotaExceeded,
    /// The user lacks the `can_acquire` permission.
    PermissionDenied,
    /// A stall or queue timeout exhausted the candidates or the instances.
    Timeout,
    /// The acquisition was cancelled by a user or by the platform.
    Cancelled,
}

impl FailureReason {
    /// Every failure reason, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [FailureReason; 9] = [
        FailureReason::NoCandidates,
        FailureReason::NoAcceptableQuality,
        FailureReason::AllCandidatesFailed,
        FailureReason::VerificationFailed,
        FailureReason::BackendUnavailable,
        FailureReason::QuotaExceeded,
        FailureReason::PermissionDenied,
        FailureReason::Timeout,
        FailureReason::Cancelled,
    ];

    /// Returns the literal recorded in `acquisitions.failure_reason`.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::FailureReason;
    ///
    /// assert_eq!(FailureReason::NoAcceptableQuality.as_str(), "no-acceptable-quality");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureReason::NoCandidates => "no-candidates",
            FailureReason::NoAcceptableQuality => "no-acceptable-quality",
            FailureReason::AllCandidatesFailed => "all-candidates-failed",
            FailureReason::VerificationFailed => "verification-failed",
            FailureReason::BackendUnavailable => "backend-unavailable",
            FailureReason::QuotaExceeded => "quota-exceeded",
            FailureReason::PermissionDenied => "permission-denied",
            FailureReason::Timeout => "timeout",
            FailureReason::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for FailureReason {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`FailureReason`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown failure reason `{0}`")]
pub struct UnknownFailureReason(pub String);

impl FromStr for FailureReason {
    type Err = UnknownFailureReason;

    /// Parses a failure reason from its literal.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownFailureReason`] when `s` is not one of the literals
    /// produced by [`FailureReason::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|reason| reason.as_str() == s)
            .ok_or_else(|| UnknownFailureReason(s.to_owned()))
    }
}

/// The granularity of an acquisition request.
///
/// A track request acquires one track; an album request either creates a single
/// album-granular job, when the acquisition instance advertises the `album`
/// granularity, or fans out into one track-granular job per album track.
///
/// # Examples
///
/// ```
/// use hearthsound_core::Granularity;
///
/// assert_eq!(Granularity::Album.as_str(), "album");
/// assert_eq!("track".parse::<Granularity>().unwrap(), Granularity::Track);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Granularity {
    /// Acquire a single track.
    Track,
    /// Acquire a whole album.
    Album,
}

impl Granularity {
    /// Every granularity, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [Granularity; 2] = [Granularity::Track, Granularity::Album];

    /// Returns the literal written to the database and to the plugin boundary.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::Granularity;
    ///
    /// assert_eq!(Granularity::Track.as_str(), "track");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            Granularity::Track => "track",
            Granularity::Album => "album",
        }
    }
}

impl fmt::Display for Granularity {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`Granularity`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown acquisition granularity `{0}`")]
pub struct UnknownGranularity(pub String);

impl FromStr for Granularity {
    type Err = UnknownGranularity;

    /// Parses a granularity from its literal.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownGranularity`] when `s` is neither `track` nor `album`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|granularity| granularity.as_str() == s)
            .ok_or_else(|| UnknownGranularity(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_literals_match_the_specification() {
        assert_eq!(
            AcquisitionState::ALL.map(AcquisitionState::as_str),
            [
                "queued",
                "resolving",
                "searching",
                "downloading",
                "verifying",
                "importing",
                "completed",
                "failed",
                "cancelled",
            ]
        );
    }

    #[test]
    fn every_state_transition_is_the_diagram() {
        const LEGAL: &[(AcquisitionState, AcquisitionState)] = &[
            (AcquisitionState::Queued, AcquisitionState::Resolving),
            (AcquisitionState::Queued, AcquisitionState::Failed),
            (AcquisitionState::Resolving, AcquisitionState::Searching),
            (AcquisitionState::Resolving, AcquisitionState::Failed),
            (AcquisitionState::Searching, AcquisitionState::Downloading),
            (AcquisitionState::Searching, AcquisitionState::Failed),
            (AcquisitionState::Searching, AcquisitionState::Cancelled),
            (AcquisitionState::Downloading, AcquisitionState::Verifying),
            (AcquisitionState::Downloading, AcquisitionState::Searching),
            (AcquisitionState::Downloading, AcquisitionState::Failed),
            (AcquisitionState::Downloading, AcquisitionState::Cancelled),
            (AcquisitionState::Verifying, AcquisitionState::Importing),
            (AcquisitionState::Verifying, AcquisitionState::Failed),
            (AcquisitionState::Verifying, AcquisitionState::Cancelled),
            (AcquisitionState::Importing, AcquisitionState::Completed),
            (AcquisitionState::Importing, AcquisitionState::Failed),
            (AcquisitionState::Importing, AcquisitionState::Cancelled),
        ];
        for from in AcquisitionState::ALL {
            for to in AcquisitionState::ALL {
                let expected = LEGAL.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "{from} -> {to} should be {expected}"
                );
            }
        }
    }

    #[test]
    fn terminal_states_have_no_outgoing_transitions() {
        for state in AcquisitionState::ALL {
            let terminal = matches!(
                state,
                AcquisitionState::Completed
                    | AcquisitionState::Failed
                    | AcquisitionState::Cancelled
            );
            assert_eq!(state.is_terminal(), terminal);
            if terminal {
                for next in AcquisitionState::ALL {
                    assert!(!state.can_transition_to(next));
                }
            }
        }
    }

    #[test]
    fn a_failed_candidate_returns_to_search() {
        assert!(AcquisitionState::Downloading.can_transition_to(AcquisitionState::Searching));
        assert!(AcquisitionState::Searching.can_transition_to(AcquisitionState::Downloading));
    }

    #[test]
    fn priorities_order_most_urgent_first() {
        assert_eq!(
            Priority::ALL.map(Priority::as_str),
            ["play", "next", "queue", "background", "upgrade"]
        );
        for (level, priority) in Priority::ALL.iter().enumerate() {
            assert_eq!(usize::from(priority.level()), level);
            assert_eq!(priority.as_str().parse::<Priority>().unwrap(), *priority);
        }
        let mut sorted = Priority::ALL;
        sorted.sort();
        assert_eq!(sorted, Priority::ALL);
        assert!(Priority::Play < Priority::Next);
        assert!(Priority::Next < Priority::Queue);
        assert!(Priority::Queue < Priority::Background);
        assert!(Priority::Background < Priority::Upgrade);
    }

    #[test]
    fn failure_reasons_match_the_specification() {
        assert_eq!(
            FailureReason::ALL.map(FailureReason::as_str),
            [
                "no-candidates",
                "no-acceptable-quality",
                "all-candidates-failed",
                "verification-failed",
                "backend-unavailable",
                "quota-exceeded",
                "permission-denied",
                "timeout",
                "cancelled",
            ]
        );
    }

    #[test]
    fn granularities_match_the_specification() {
        assert_eq!(
            Granularity::ALL.map(Granularity::as_str),
            ["track", "album"]
        );
    }

    #[test]
    fn literals_round_trip_through_text_and_json() {
        fn check<T>(value: T, literal: &str)
        where
            T: Copy + std::fmt::Debug + PartialEq + Serialize + serde::de::DeserializeOwned,
        {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(json, format!("\"{literal}\""));
            assert_eq!(serde_json::from_str::<T>(&json).unwrap(), value);
        }

        for state in AcquisitionState::ALL {
            check(state, state.as_str());
            assert_eq!(state.to_string(), state.as_str());
            assert_eq!(state.as_str().parse::<AcquisitionState>().unwrap(), state);
        }
        for priority in Priority::ALL {
            check(priority, priority.as_str());
            assert_eq!(priority.to_string(), priority.as_str());
        }
        for reason in FailureReason::ALL {
            check(reason, reason.as_str());
            assert_eq!(reason.to_string(), reason.as_str());
        }
        for granularity in Granularity::ALL {
            check(granularity, granularity.as_str());
            assert_eq!(granularity.to_string(), granularity.as_str());
        }
    }

    #[test]
    fn rejects_unknown_literals() {
        assert_eq!(
            "searching".parse::<Priority>(),
            Err(UnknownPriority("searching".to_owned()))
        );
        assert_eq!(
            "urgent".parse::<AcquisitionState>(),
            Err(UnknownAcquisitionState("urgent".to_owned()))
        );
        assert_eq!(
            "no-candidate".parse::<FailureReason>(),
            Err(UnknownFailureReason("no-candidate".to_owned()))
        );
        assert!(matches!(
            "Track".parse::<Granularity>(),
            Err(UnknownGranularity(_))
        ));
        assert!(serde_json::from_str::<Priority>("\"urgent\"").is_err());
    }
}
