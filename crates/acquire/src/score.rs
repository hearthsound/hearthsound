//! The match score of a candidate against the wanted track.
//!
//! Scoring is a weighted sum of five field similarities:
//!
//! | Field | Weight | Similarity |
//! |---|---|---|
//! | title | 0.35 | Jaro-Winkler on the normalized titles |
//! | artist | 0.25 | Jaro-Winkler on the normalized artist names |
//! | album | 0.10 | Jaro-Winkler on the normalized album titles |
//! | duration | 0.20 | [`duration_similarity`] of the two durations |
//! | track number | 0.10 | 1 when equal, 0.5 when the candidate's is unknown, 0 when they differ |
//!
//! A field the candidate does not carry - a Soulseek file name rarely names an
//! album - is left out and the remaining weights are renormalized, so a
//! candidate is judged on the evidence it brings rather than punished for a
//! missing tag. The track number is the exception: it is part of the score
//! whenever the target knows it, because a file that sits at a different
//! position in the album is strong evidence against the match.
//!
//! A candidate whose duration is more than
//! [`DURATION_REJECT_MS`] away from the target is rejected before the weighted
//! sum is computed ([`MatchOutcome::DurationMismatch`]). A candidate whose file
//! name or path carries an alternate-version qualifier the target does not
//! carry has its score halved (see [`candidate_qualifiers`]).
//!
//! [`MatchScore::is_accepted`] applies the acceptance threshold, which the
//! composition root reads from `acquisition.min_match_score` and hands in as a
//! parameter ([`DEFAULT_MIN_MATCH_SCORE`] is its default).

use std::collections::BTreeSet;

use hearthsound_core::providers::{Candidate, WantedTrack};

use crate::normalize::{self, Qualifier};
use crate::path;

/// Weight of the title similarity in the match score.
pub const WEIGHT_TITLE: f64 = 0.35;
/// Weight of the artist similarity in the match score.
pub const WEIGHT_ARTIST: f64 = 0.25;
/// Weight of the album similarity in the match score.
pub const WEIGHT_ALBUM: f64 = 0.10;
/// Weight of the duration similarity in the match score.
pub const WEIGHT_DURATION: f64 = 0.20;
/// Weight of the track number similarity in the match score.
pub const WEIGHT_TRACK_NUMBER: f64 = 0.10;
/// Duration difference at or below which the durations match perfectly, in
/// milliseconds.
pub const DURATION_PERFECT_MS: u32 = 2_000;
/// Duration difference at and beyond which the duration similarity is zero, in
/// milliseconds.
pub const DURATION_ZERO_MS: u32 = 10_000;
/// Duration difference beyond which a candidate is rejected outright, in
/// milliseconds.
pub const DURATION_REJECT_MS: u32 = 15_000;
/// Factor applied to the score of a candidate carrying an unwanted qualifier.
pub const QUALIFIER_PENALTY: f64 = 0.5;
/// Default acceptance threshold (`acquisition.min_match_score`).
pub const DEFAULT_MIN_MATCH_SCORE: f64 = 0.80;

/// The track a candidate is compared against.
///
/// The target carries the fields the catalog and the metadata providers know
/// about the wanted track. `album` and `duration_ms` are optional because a
/// track request may not know the album it belongs to, and a field the target
/// does not carry is not part of the comparison.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::score::MatchTarget;
///
/// let target = MatchTarget::new(
///     "Mojo Pin",
///     vec!["Jeff Buckley".to_owned()],
///     Some("Grace".to_owned()),
///     Some(320_000),
///     Some(1),
/// );
/// assert_eq!(target.title, "Mojo Pin");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchTarget {
    /// The wanted title.
    pub title: String,
    /// The credited artists, in credit order.
    pub artists: Vec<String>,
    /// The album the track belongs to, when known.
    pub album: Option<String>,
    /// The track duration in milliseconds, when known.
    pub duration_ms: Option<u32>,
    /// The track number within its disc, when known.
    pub track_number: Option<u16>,
}

impl MatchTarget {
    /// Creates a target from its parts.
    ///
    /// # Parameters
    ///
    /// - `title`: the wanted title.
    /// - `artists`: the credited artists, in credit order.
    /// - `album`: the album the track belongs to, when known.
    /// - `duration_ms`: the track duration in milliseconds, when known.
    /// - `track_number`: the track number within its disc, when known.
    ///
    /// # Returns
    ///
    /// The target those values describe.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::score::MatchTarget;
    ///
    /// let target = MatchTarget::new("Title", vec!["Artist".to_owned()], None, None, None);
    /// assert_eq!(target.artists, ["Artist"]);
    /// ```
    pub fn new(
        title: impl Into<String>,
        artists: Vec<String>,
        album: Option<String>,
        duration_ms: Option<u32>,
        track_number: Option<u16>,
    ) -> MatchTarget {
        MatchTarget {
            title: title.into(),
            artists,
            album,
            duration_ms,
            track_number,
        }
    }

    /// Creates a target from a provider's wanted track and its album.
    ///
    /// # Parameters
    ///
    /// - `track`: the wanted track as an acquisition provider described it.
    /// - `album`: the album the track belongs to, when the caller knows it; a
    ///   track request usually does not.
    ///
    /// # Returns
    ///
    /// A target carrying the track's title, artists, duration and track number
    /// plus `album`. The disc number, the ISRC and the external ids take no part
    /// in scoring.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::score::MatchTarget;
    /// use hearthsound_core::providers::WantedTrack;
    ///
    /// let track = WantedTrack {
    ///     title: "Mojo Pin".to_owned(),
    ///     artists: vec!["Jeff Buckley".to_owned()],
    ///     track_number: Some(1),
    ///     disc_number: None,
    ///     duration_ms: Some(320_000),
    ///     isrc: None,
    ///     ids: Vec::new(),
    /// };
    /// let target = MatchTarget::from_track(&track, Some("Grace"));
    /// assert_eq!(target.track_number, Some(1));
    /// ```
    pub fn from_track(track: &WantedTrack, album: Option<&str>) -> MatchTarget {
        MatchTarget {
            title: track.title.clone(),
            artists: track.artists.clone(),
            album: album.map(str::to_owned),
            duration_ms: track.duration_ms,
            track_number: track.track_number,
        }
    }
}

/// The comparable fields of a candidate.
///
/// The title comes from the file name when the name carries one and from the
/// backend's title otherwise; the artist, album and track number come from the
/// file name, from its parent directories, or from a backend title that reads
/// `Artist - Title`.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::score::{CandidateFields, candidate_fields};
/// use hearthsound_core::providers::{Candidate, SourceInfo};
///
/// let candidate = Candidate {
///     token: "token".to_owned(),
///     title: String::new(),
///     path: r"Music\Air\Moon Safari\01 - La Femme d'Argent.flac".to_owned(),
///     files: Vec::new(),
///     source: SourceInfo {
///         key: "peer".to_owned(),
///         speed_bps: None,
///         queue_length: None,
///         free_slot: None,
///     },
/// };
/// let fields: CandidateFields = candidate_fields(&candidate);
/// assert_eq!(fields.artist.as_deref(), Some("Air"));
/// assert_eq!(fields.track_number, Some(1));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateFields {
    /// The candidate's title.
    pub title: String,
    /// The artist the candidate names.
    pub artist: Option<String>,
    /// The album the candidate names.
    pub album: Option<String>,
    /// The track number the candidate names.
    pub track_number: Option<u16>,
}

/// The per-field similarities of one candidate and the weighted total.
///
/// A field is `None` when the candidate or the target does not carry it and it
/// was therefore left out of the sum; [`MatchScore::total`] is the weighted
/// average over the fields that took part, after the qualifier penalty.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::score::{MatchOutcome, MatchTarget, score_candidate};
/// use hearthsound_core::providers::{Candidate, SourceInfo};
///
/// let candidate = Candidate {
///     token: "token".to_owned(),
///     title: "Mojo Pin".to_owned(),
///     path: "Grace/01 - Mojo Pin.flac".to_owned(),
///     files: Vec::new(),
///     source: SourceInfo {
///         key: "peer".to_owned(),
///         speed_bps: None,
///         queue_length: None,
///         free_slot: None,
///     },
/// };
/// let target = MatchTarget::new("Mojo Pin", Vec::new(), None, None, None);
/// let MatchOutcome::Matched(score) = score_candidate(&target, &candidate) else {
///     panic!("the candidate does not have a duration, so it cannot be rejected");
/// };
/// assert_eq!(score.total, 1.0);
/// assert!(score.is_accepted(0.8));
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchScore {
    /// The normalized title similarity.
    pub title: Option<f64>,
    /// The best normalized artist similarity, when both sides name an artist.
    pub artist: Option<f64>,
    /// The normalized album similarity, when both sides name an album.
    pub album: Option<f64>,
    /// The duration similarity, when both durations are known.
    pub duration: Option<f64>,
    /// The track number similarity, when the target knows its track number.
    pub track_number: Option<f64>,
    /// The weighted total, after the qualifier penalty.
    pub total: f64,
    /// Whether an unwanted qualifier halved the total.
    pub penalized: bool,
}

impl MatchScore {
    /// Returns whether the score clears an acceptance threshold.
    ///
    /// # Parameters
    ///
    /// - `min_match_score`: the threshold, read from
    ///   `acquisition.min_match_score` by the composition root
    ///   ([`DEFAULT_MIN_MATCH_SCORE`] by default).
    ///
    /// # Returns
    ///
    /// `true` when [`MatchScore::total`] is at or above the threshold, so a
    /// candidate that scores exactly the threshold is accepted.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::score::{DEFAULT_MIN_MATCH_SCORE, MatchOutcome, MatchTarget, score_candidate};
    /// use hearthsound_core::providers::{Candidate, SourceInfo};
    ///
    /// let candidate = Candidate {
    ///     token: "token".to_owned(),
    ///     title: "Title (Live)".to_owned(),
    ///     path: String::new(),
    ///     files: Vec::new(),
    ///     source: SourceInfo {
    ///         key: "peer".to_owned(),
    ///         speed_bps: None,
    ///         queue_length: None,
    ///         free_slot: None,
    ///     },
    /// };
    /// let target = MatchTarget::new("Title", Vec::new(), None, None, None);
    /// let MatchOutcome::Matched(score) = score_candidate(&target, &candidate) else {
    ///     panic!("no duration is known, so nothing can be rejected");
    /// };
    /// assert!(!score.is_accepted(DEFAULT_MIN_MATCH_SCORE));
    /// ```
    pub fn is_accepted(&self, min_match_score: f64) -> bool {
        self.total >= min_match_score
    }
}

/// What scoring did with a candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MatchOutcome {
    /// The candidate was scored.
    Matched(MatchScore),
    /// The candidate's duration is more than [`DURATION_REJECT_MS`] away from
    /// the target's, so no score is computed.
    DurationMismatch,
}

/// Scores one candidate against the wanted track.
///
/// # Parameters
///
/// - `target`: the wanted track.
/// - `candidate`: the candidate as the acquisition provider reported it.
///
/// # Returns
///
/// [`MatchOutcome::Matched`] with the per-field similarities and the weighted
/// total, or [`MatchOutcome::DurationMismatch`] when both durations are known
/// and differ by more than [`DURATION_REJECT_MS`].
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::score::{MatchOutcome, MatchTarget, score_candidate};
/// use hearthsound_core::providers::{Candidate, CandidateFile, SourceInfo};
///
/// let candidate = Candidate {
///     token: "token".to_owned(),
///     title: String::new(),
///     path: "Grace/01 - Mojo Pin.flac".to_owned(),
///     files: vec![CandidateFile {
///         name: "01 - Mojo Pin.flac".to_owned(),
///         size: Some(30_000_000),
///         format: Some("flac".to_owned()),
///         bitrate_kbps: None,
///         sample_rate: Some(44_100),
///         bit_depth: Some(16),
///         duration_ms: Some(320_400),
///         track_index: Some(0),
///     }],
///     source: SourceInfo {
///         key: "peer".to_owned(),
///         speed_bps: None,
///         queue_length: None,
///         free_slot: None,
///     },
/// };
/// let target = MatchTarget::new("Mojo Pin", Vec::new(), None, Some(320_000), Some(1));
///
/// let MatchOutcome::Matched(score) = score_candidate(&target, &candidate) else {
///     panic!("the durations are within the reject window");
/// };
/// assert_eq!(score.duration, Some(1.0));
/// ```
pub fn score_candidate(target: &MatchTarget, candidate: &Candidate) -> MatchOutcome {
    let fields = candidate_fields(candidate);
    let duration_ms = candidate.files.first().and_then(|file| file.duration_ms);
    let duration = match (target.duration_ms, duration_ms) {
        (Some(wanted), Some(found)) => match duration_similarity(wanted, found) {
            Some(score) => Some(score),
            None => return MatchOutcome::DurationMismatch,
        },
        _ => None,
    };
    let title = match (target.title.trim(), fields.title.trim()) {
        ("", _) | (_, "") => None,
        (wanted, found) => Some(normalize::similarity(found, wanted)),
    };
    let artist = match fields.artist.as_deref().map(str::trim) {
        Some(found) if !found.is_empty() && !target.artists.is_empty() => Some(
            target
                .artists
                .iter()
                .map(|wanted| normalize::similarity(found, wanted))
                .fold(0.0_f64, f64::max),
        ),
        _ => None,
    };
    let album = match (
        fields.album.as_deref().map(str::trim),
        target.album.as_deref(),
    ) {
        (Some(found), Some(wanted)) if !found.is_empty() && !wanted.trim().is_empty() => {
            Some(normalize::similarity(found, wanted))
        }
        _ => None,
    };
    let track_number = target.track_number.map(|wanted| match fields.track_number {
        Some(found) if found == wanted => 1.0,
        Some(_) => 0.0,
        None => 0.5,
    });

    let mut weighted = 0.0;
    let mut total_weight = 0.0;
    for (weight, part) in [
        (WEIGHT_TITLE, title),
        (WEIGHT_ARTIST, artist),
        (WEIGHT_ALBUM, album),
        (WEIGHT_DURATION, duration),
        (WEIGHT_TRACK_NUMBER, track_number),
    ] {
        if let Some(part) = part {
            weighted += weight * part;
            total_weight += weight;
        }
    }
    let mut total = if total_weight > 0.0 {
        weighted / total_weight
    } else {
        0.0
    };
    let penalized = is_penalized(target, candidate);
    if penalized {
        total *= QUALIFIER_PENALTY;
    }
    MatchOutcome::Matched(MatchScore {
        title,
        artist,
        album,
        duration,
        track_number,
        total,
        penalized,
    })
}

/// Returns the duration similarity of two known durations.
///
/// The similarity is 1 when the durations differ by no more than
/// [`DURATION_PERFECT_MS`], falls linearly to 0 at [`DURATION_ZERO_MS`], and
/// stays 0 until the reject window ends.
///
/// # Parameters
///
/// - `wanted_ms`: the target duration in milliseconds.
/// - `found_ms`: the candidate duration in milliseconds.
///
/// # Returns
///
/// `Some(similarity)` in `0.0..=1.0`, or `None` when the two durations are more
/// than [`DURATION_REJECT_MS`] apart and the candidate must be discarded.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::score::duration_similarity;
///
/// assert_eq!(duration_similarity(320_000, 321_000), Some(1.0));
/// assert_eq!(duration_similarity(320_000, 330_000), Some(0.0));
/// assert_eq!(duration_similarity(320_000, 336_000), None);
/// ```
pub fn duration_similarity(wanted_ms: u32, found_ms: u32) -> Option<f64> {
    let difference = wanted_ms.abs_diff(found_ms);
    if difference > DURATION_REJECT_MS {
        return None;
    }
    if difference <= DURATION_PERFECT_MS {
        return Some(1.0);
    }
    if difference >= DURATION_ZERO_MS {
        return Some(0.0);
    }
    let span = f64::from(DURATION_ZERO_MS - DURATION_PERFECT_MS);
    Some(1.0 - f64::from(difference - DURATION_PERFECT_MS) / span)
}

/// Returns the comparable fields of a candidate.
///
/// The candidate's path is parsed first, because a Soulseek file name carries
/// more than the backend's title; a backend title that reads `Artist - Title`
/// then fills whatever the path did not name.
///
/// # Parameters
///
/// - `candidate`: the candidate as the acquisition provider reported it. When
///   its path is empty, the first file's name is parsed instead.
///
/// # Returns
///
/// The fields the comparison uses. The title is never empty unless neither the
/// path nor the backend named one.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::score::candidate_fields;
/// use hearthsound_core::providers::{Candidate, SourceInfo};
///
/// let candidate = Candidate {
///     token: "token".to_owned(),
///     title: "Air - La Femme d'Argent".to_owned(),
///     path: "Air/Moon Safari/01.flac".to_owned(),
///     files: Vec::new(),
///     source: SourceInfo {
///         key: "peer".to_owned(),
///         speed_bps: None,
///         queue_length: None,
///         free_slot: None,
///     },
/// };
/// let fields = candidate_fields(&candidate);
/// assert_eq!(fields.artist.as_deref(), Some("Air"));
/// assert_eq!(fields.title, "La Femme d'Argent");
/// assert_eq!(fields.album.as_deref(), Some("Moon Safari"));
/// ```
pub fn candidate_fields(candidate: &Candidate) -> CandidateFields {
    let path = if candidate.path.trim().is_empty() {
        candidate
            .files
            .first()
            .map(|file| file.name.as_str())
            .unwrap_or("")
    } else {
        candidate.path.as_str()
    };
    let from_path = path::derive_tags(path);
    let from_title = if candidate.title.trim().is_empty() {
        None
    } else {
        Some(path::derive_tags(&candidate.title))
    };
    let title = from_path
        .title
        .or_else(|| from_title.as_ref().and_then(|tags| tags.title.clone()))
        .unwrap_or_else(|| candidate.title.trim().to_owned());
    CandidateFields {
        title,
        artist: from_path
            .artist
            .or_else(|| from_title.as_ref().and_then(|tags| tags.artist.clone())),
        album: from_path
            .album
            .or_else(|| from_title.as_ref().and_then(|tags| tags.album.clone())),
        track_number: from_path
            .track_number
            .or_else(|| from_title.as_ref().and_then(|tags| tags.track_number)),
    }
}

/// Returns the name a log line or a ledger message calls a candidate by.
///
/// A backend that downloads one file at a time has no directory to name, so it
/// reports its candidates with an empty path. Those candidates are named by the
/// backend's title instead, and by the first file's name when the backend named
/// no title either.
///
/// # Parameters
///
/// - `candidate`: the candidate as the acquisition provider reported it.
///
/// # Returns
///
/// The candidate's path, its title, or its first file's name; an empty string
/// when the provider named none of them.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::score::candidate_name;
/// use hearthsound_core::providers::{Candidate, CandidateFile, SourceInfo};
///
/// let candidate = Candidate {
///     token: "token".to_owned(),
///     title: "Creep".to_owned(),
///     path: String::new(),
///     files: vec![CandidateFile {
///         name: "2f1c.opus".to_owned(),
///         size: None,
///         format: Some("opus".to_owned()),
///         bitrate_kbps: None,
///         sample_rate: None,
///         bit_depth: None,
///         duration_ms: None,
///         track_index: None,
///     }],
///     source: SourceInfo {
///         key: "yt-dlp".to_owned(),
///         speed_bps: None,
///         queue_length: None,
///         free_slot: None,
///     },
/// };
/// assert_eq!(candidate_name(&candidate), "Creep");
/// ```
pub fn candidate_name(candidate: &Candidate) -> &str {
    if !candidate.path.trim().is_empty() {
        return candidate.path.as_str();
    }
    if !candidate.title.trim().is_empty() {
        return candidate.title.as_str();
    }
    candidate
        .files
        .first()
        .map(|file| file.name.as_str())
        .unwrap_or_default()
}

/// Returns the alternate-version qualifiers a candidate carries.
///
/// The candidate's title, its path and each of its file names are scanned.
///
/// # Parameters
///
/// - `candidate`: the candidate as the acquisition provider reported it.
///
/// # Returns
///
/// Every qualifier found, empty when the candidate carries none.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::normalize::Qualifier;
/// use hearthsound_acquire::score::candidate_qualifiers;
/// use hearthsound_core::providers::{Candidate, SourceInfo};
///
/// let candidate = Candidate {
///     token: "token".to_owned(),
///     title: String::new(),
///     path: "Live at Wembley/01 - Title.flac".to_owned(),
///     files: Vec::new(),
///     source: SourceInfo {
///         key: "peer".to_owned(),
///         speed_bps: None,
///         queue_length: None,
///         free_slot: None,
///     },
/// };
/// assert!(candidate_qualifiers(&candidate).contains(&Qualifier::Live));
/// ```
pub fn candidate_qualifiers(candidate: &Candidate) -> BTreeSet<Qualifier> {
    let mut found = normalize::qualifiers_in(&candidate.title);
    found.extend(normalize::qualifiers_in(&candidate.path));
    for file in &candidate.files {
        found.extend(normalize::qualifiers_in(&file.name));
    }
    found
}

/// Returns the alternate-version qualifiers a target asks for.
///
/// A candidate is only penalized for a qualifier the target does not carry
/// itself, so an acquisition of a live album keeps its live recordings.
///
/// # Parameters
///
/// - `target`: the wanted track.
///
/// # Returns
///
/// Every qualifier the target's title, artists or album carries.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::normalize::Qualifier;
/// use hearthsound_acquire::score::{MatchTarget, target_qualifiers};
///
/// let target = MatchTarget::new("Title (Acoustic)", Vec::new(), None, None, None);
/// assert!(target_qualifiers(&target).contains(&Qualifier::Acoustic));
/// ```
pub fn target_qualifiers(target: &MatchTarget) -> BTreeSet<Qualifier> {
    let mut found = normalize::qualifiers_in(&target.title);
    for artist in &target.artists {
        found.extend(normalize::qualifiers_in(artist));
    }
    if let Some(album) = &target.album {
        found.extend(normalize::qualifiers_in(album));
    }
    found
}

/// Returns whether a candidate carries a qualifier the target does not.
fn is_penalized(target: &MatchTarget, candidate: &Candidate) -> bool {
    let found = candidate_qualifiers(candidate);
    if found.is_empty() {
        return false;
    }
    !found
        .difference(&target_qualifiers(target))
        .next()
        .is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_core::providers::{CandidateFile, SourceInfo};

    /// Builds a candidate file with the attributes a lossless rip reports.
    fn flac(name: &str, duration_ms: Option<u32>) -> CandidateFile {
        CandidateFile {
            name: name.to_owned(),
            size: Some(30_000_000),
            format: Some("flac".to_owned()),
            bitrate_kbps: None,
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            duration_ms,
            track_index: Some(0),
        }
    }

    /// Builds an untagged candidate, the way a Soulseek peer reports one.
    fn candidate(path: &str, duration_ms: Option<u32>) -> Candidate {
        Candidate {
            token: format!("token:{path}"),
            title: String::new(),
            path: path.to_owned(),
            files: vec![flac(
                path.rsplit(['/', '\\']).next().unwrap_or(path),
                duration_ms,
            )],
            source: SourceInfo {
                key: "peer".to_owned(),
                speed_bps: Some(1_000_000),
                queue_length: Some(0),
                free_slot: Some(true),
            },
        }
    }

    /// Builds a target for a fully known track.
    fn target() -> MatchTarget {
        MatchTarget::new(
            "Mojo Pin",
            vec!["Jeff Buckley".to_owned()],
            Some("Grace".to_owned()),
            Some(320_000),
            Some(1),
        )
    }

    /// Unwraps a matched outcome.
    fn scored(target: &MatchTarget, candidate: &Candidate) -> MatchScore {
        match score_candidate(target, candidate) {
            MatchOutcome::Matched(score) => score,
            MatchOutcome::DurationMismatch => panic!("unexpected duration mismatch"),
        }
    }

    /// Asserts a weighted total, which is a ratio of floating-point sums.
    fn assert_total(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected a total of {expected}, got {actual}"
        );
    }

    #[test]
    fn a_fully_tagged_rip_scores_one() {
        let score = scored(
            &target(),
            &candidate(
                r"Music\Jeff Buckley\Grace\01 - Mojo Pin.flac",
                Some(320_000),
            ),
        );
        assert_eq!(score.title, Some(1.0));
        assert_eq!(score.artist, Some(1.0));
        assert_eq!(score.album, Some(1.0));
        assert_eq!(score.duration, Some(1.0));
        assert_eq!(score.track_number, Some(1.0));
        assert_total(score.total, 1.0);
        assert!(!score.penalized);
        assert!(score.is_accepted(DEFAULT_MIN_MATCH_SCORE));
    }

    #[test]
    fn a_backend_title_fills_the_fields_the_path_did_not_name() {
        let mut from_title = candidate("Jeff Buckley/Grace/01.flac", Some(320_000));
        from_title.title = "Mojo Pin".to_owned();
        let score = scored(&target(), &from_title);
        assert_eq!(score.title, Some(1.0));
        assert_eq!(score.artist, Some(1.0));
        assert_eq!(score.album, Some(1.0));
        assert_eq!(score.track_number, Some(1.0));
        assert_total(score.total, 1.0);
    }

    #[test]
    fn a_backend_title_that_names_the_artist_scores_the_artist() {
        let mut from_title = candidate("01.flac", Some(320_000));
        from_title.title = "Jeff Buckley - Mojo Pin".to_owned();
        let score = scored(&target(), &from_title);
        assert_eq!(score.title, Some(1.0));
        assert_eq!(score.artist, Some(1.0));
        assert_eq!(score.album, None);
        assert_eq!(score.track_number, Some(1.0));
        assert_total(score.total, 1.0);
    }

    #[test]
    fn a_path_only_candidate_is_judged_on_what_it_carries() {
        let path_only = candidate("Music/Air/Moon Safari/01 - La Femme d'Argent.flac", None);
        let target = MatchTarget::new(
            "La Femme d'Argent",
            vec!["Air".to_owned()],
            Some("Moon Safari".to_owned()),
            None,
            Some(1),
        );
        let score = scored(&target, &path_only);
        assert_total(score.total, 1.0);
        assert_eq!(score.duration, None);
    }

    #[test]
    fn unknown_fields_are_renormalized() {
        let target = target();
        let score = scored(&target, &candidate("Mojo Pin.flac", Some(320_000)));
        assert_eq!(score.album, None);
        assert_eq!(score.artist, None);
        let expected = (WEIGHT_TITLE + WEIGHT_DURATION + WEIGHT_TRACK_NUMBER * 0.5)
            / (WEIGHT_TITLE + WEIGHT_DURATION + WEIGHT_TRACK_NUMBER);
        assert_total(score.total, expected);
    }

    #[test]
    fn a_wrong_track_number_loses_its_weight() {
        let score = scored(
            &target(),
            &candidate(
                r"Music\Jeff Buckley\Grace\07 - Mojo Pin.flac",
                Some(320_000),
            ),
        );
        assert_eq!(score.track_number, Some(0.0));
        assert_total(score.total, 0.9);
        assert!(score.is_accepted(DEFAULT_MIN_MATCH_SCORE));
    }

    #[test]
    fn an_unknown_track_number_scores_half() {
        let score = scored(&target(), &candidate("Mojo Pin.flac", Some(320_000)));
        assert_eq!(score.track_number, Some(0.5));
    }

    #[test]
    fn duration_boundaries() {
        assert_eq!(duration_similarity(300_000, 302_000), Some(1.0));
        assert_eq!(duration_similarity(300_000, 298_000), Some(1.0));
        let just_over = duration_similarity(300_000, 302_001).unwrap();
        assert!(just_over < 1.0 && just_over > 0.9998, "got {just_over}");
        assert_eq!(duration_similarity(300_000, 306_000), Some(0.5));
        assert_eq!(duration_similarity(300_000, 310_000), Some(0.0));
        assert_eq!(duration_similarity(300_000, 315_000), Some(0.0));
        assert_eq!(duration_similarity(300_000, 315_001), None);
    }

    #[test]
    fn a_duration_beyond_the_window_is_rejected() {
        let outcome = score_candidate(
            &target(),
            &candidate(
                r"Music\Jeff Buckley\Grace\01 - Mojo Pin.flac",
                Some(200_000),
            ),
        );
        assert_eq!(outcome, MatchOutcome::DurationMismatch);
    }

    #[test]
    fn a_duration_ten_seconds_off_sits_exactly_on_the_threshold() {
        let score = scored(
            &target(),
            &candidate(
                r"Music\Jeff Buckley\Grace\01 - Mojo Pin.flac",
                Some(330_000),
            ),
        );
        assert_eq!(score.duration, Some(0.0));
        assert_total(score.total, 0.8);
        assert!(score.is_accepted(DEFAULT_MIN_MATCH_SCORE));
        let worse = scored(
            &target(),
            &candidate(r"Music\Other Act\Grace\01 - Mojo Pin.flac", Some(330_000)),
        );
        assert!(worse.total < DEFAULT_MIN_MATCH_SCORE);
        assert!(!worse.is_accepted(DEFAULT_MIN_MATCH_SCORE));
    }

    #[test]
    fn an_unknown_duration_leaves_the_other_fields() {
        let score = scored(
            &target(),
            &candidate(r"Music\Jeff Buckley\Grace\01 - Mojo Pin.flac", None),
        );
        assert_eq!(score.duration, None);
        assert_total(score.total, 1.0);
    }

    #[test]
    fn unwanted_qualifiers_halve_the_score() {
        let score = scored(
            &target(),
            &candidate(
                r"Music\Jeff Buckley\Grace\01 - Mojo Pin (Live).flac",
                Some(320_000),
            ),
        );
        assert!(score.penalized);
        assert_total(score.total, 0.5);
        assert!(!score.is_accepted(DEFAULT_MIN_MATCH_SCORE));
    }

    #[test]
    fn every_qualifier_halves_the_score() {
        for qualifier in Qualifier::ALL {
            let path = format!(
                "Music/Jeff Buckley/Grace/01 - Mojo Pin ({}).flac",
                qualifier.as_str()
            );
            let score = scored(&target(), &candidate(&path, Some(320_000)));
            assert!(score.penalized, "{} was not penalized", qualifier.as_str());
            assert_total(score.total, 0.5);
        }
    }

    #[test]
    fn a_qualifier_the_target_asks_for_is_kept() {
        let target = MatchTarget::new(
            "Mojo Pin (Live)",
            vec!["Jeff Buckley".to_owned()],
            Some("Grace".to_owned()),
            Some(320_000),
            Some(1),
        );
        let score = scored(
            &target,
            &candidate(
                r"Music\Jeff Buckley\Grace\01 - Mojo Pin (Live).flac",
                Some(320_000),
            ),
        );
        assert!(!score.penalized);
        assert_eq!(score.title, Some(1.0));
        assert_total(score.total, 1.0);
    }

    #[test]
    fn a_qualifier_in_a_directory_or_a_file_name_is_penalized() {
        let in_directory = candidate(
            r"Music\Jeff Buckley\Grace\Live at Wembley\01 - Mojo Pin.flac",
            Some(320_000),
        );
        assert!(candidate_qualifiers(&in_directory).contains(&Qualifier::Live));

        let mut in_file_name = candidate(
            r"Music\Jeff Buckley\Grace\01 - Mojo Pin.flac",
            Some(320_000),
        );
        in_file_name.files[0].name = "01 - Mojo Pin [Nightcore].flac".to_owned();
        let score = scored(&target(), &in_file_name);
        assert!(score.penalized);
        assert_total(score.total, 0.5);
    }

    #[test]
    fn a_different_track_is_rejected_by_the_threshold() {
        let score = scored(
            &target(),
            &candidate(
                r"Music\Jeff Buckley\Grace\05 - Last Goodbye.flac",
                Some(315_000),
            ),
        );
        assert!(!score.is_accepted(DEFAULT_MIN_MATCH_SCORE));
    }

    #[test]
    fn a_remastered_take_of_the_same_track_is_accepted() {
        let score = scored(
            &target(),
            &candidate(
                r"Music\Jeff Buckley\Grace\01 - Mojo Pin (Remastered 2015).flac",
                Some(320_200),
            ),
        );
        assert!(!score.penalized);
        assert!(score.is_accepted(DEFAULT_MIN_MATCH_SCORE));
    }

    #[test]
    fn an_accented_candidate_matches_an_ascii_target() {
        let target = MatchTarget::new("Hoppipolla", vec!["Sigur Ros".to_owned()], None, None, None);
        let score = scored(
            &target,
            &candidate("Sigur R\u{f3}s/01 - Hopp\u{ed}polla.flac", None),
        );
        assert_eq!(score.title, Some(1.0));
        assert_eq!(score.artist, Some(1.0));
        assert_total(score.total, 1.0);
    }

    #[test]
    fn fields_fall_back_to_the_first_file_name() {
        let mut empty = candidate("", None);
        empty.files[0].name = "Air/Moon Safari/01 - La Femme d'Argent.flac".to_owned();
        let fields = candidate_fields(&empty);
        assert_eq!(fields.artist.as_deref(), Some("Air"));
        assert_eq!(fields.title, "La Femme d'Argent");
    }

    #[test]
    fn a_candidate_without_a_path_is_named_by_its_title_or_file() {
        let mut unnamed = candidate("", None);
        unnamed.title = "Air - La Femme d'Argent".to_owned();
        unnamed.files[0].name = "2f1c.opus".to_owned();
        assert_eq!(candidate_name(&unnamed), "Air - La Femme d'Argent");

        unnamed.title.clear();
        assert_eq!(candidate_name(&unnamed), "2f1c.opus");

        unnamed.files.clear();
        assert_eq!(candidate_name(&unnamed), "");

        let named = candidate("Air/Moon Safari/01.flac", None);
        assert_eq!(candidate_name(&named), "Air/Moon Safari/01.flac");
    }

    #[test]
    fn a_candidate_without_a_file_has_no_duration() {
        let mut no_files = candidate("Grace/01 - Mojo Pin.flac", None);
        no_files.files.clear();
        let score = scored(&target(), &no_files);
        assert_eq!(score.duration, None);
        assert_eq!(score.title, Some(1.0));
    }

    #[test]
    fn an_empty_target_title_only_scores_the_other_fields() {
        let target = MatchTarget::new("", Vec::new(), None, Some(320_000), None);
        let score = scored(&target, &candidate("Mojo Pin.flac", Some(320_000)));
        assert_eq!(score.title, None);
        assert_total(score.total, 1.0);
    }
}
