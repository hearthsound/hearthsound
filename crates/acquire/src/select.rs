//! Candidate selection: the ETA estimate and the ordered attempt list.
//!
//! Once [`crate::score`] has accepted a candidate and [`crate::profile`] has
//! agreed that its quality is good enough, this module decides what to
//! download and in what order. Every candidate gets an ETA from its size and
//! the source's reported upload speed, and the request class picks the rule:
//!
//! - **interactive** (`play`, `next`): a user is waiting, so the highest
//!   preferred tier whose ETA is at or below
//!   [`SelectionRequest::interactive_eta_target`] comes first; when no
//!   candidate meets that target, the lowest ETA wins. Ties go to the higher
//!   match score, then to the earlier provider instance and candidate position.
//! - **background**: nobody is waiting, so the highest preferred tier wins
//!   first, then the higher match score, then the lower ETA.
//!
//! A candidate whose size or speed is unknown has no estimate at all and is a
//! last resort: it is only tried after every candidate that does have one.
//!
//! The tier order is the profile's preference order, so a host on the `compact`
//! profile gets the best lossy tier before a lossless one even when the
//! lossless file is the higher tier in absolute terms.
//!
//! The result is an [`AttemptPlan`]: the candidates to try in order, capped by
//! [`SelectionRequest::max_candidates`], or the reason why there is nothing to
//! try - `no-candidates` when no candidate was found or scored, and
//! `no-acceptable-quality` when candidates existed but none reached the
//! profile's minimum tier.

use std::cmp::Ordering;
use std::time::Duration;

use hearthsound_core::providers::{Candidate, WantedTrack};
use hearthsound_core::{FailureReason, Priority, QualityTier};

use crate::profile::{QualityProfile, quality_tier};
use crate::score::{
    DEFAULT_MIN_MATCH_SCORE, MatchOutcome, MatchScore, MatchTarget, score_candidate,
};

/// Minimum transfer speed the ETA formula assumes, in bytes per second.
///
/// A source that reports a slower speed or none at all is assumed to move
/// bytes at 50 KB/s, so a slow peer still produces an estimate instead of an
/// infinite one.
pub const MIN_TRANSFER_SPEED_BPS: u64 = 50 * 1024;
/// Seconds the ETA formula adds per queued transfer.
///
/// The term applies to a source that reports no free upload slot: every
/// transfer ahead of this one costs roughly half a minute before the download
/// starts.
pub const QUEUE_STEP_SECS: u64 = 30;
/// Default ETA ceiling for an interactive request (`acquisition.interactive_eta_target_secs`).
pub const DEFAULT_INTERACTIVE_ETA_TARGET: Duration = Duration::from_secs(60);
/// Default cap on the candidates one acquisition attempts (`acquisition.max_candidates`).
pub const DEFAULT_MAX_CANDIDATES: usize = 5;

/// Which selection rules apply to a request.
///
/// The class follows the acquisition priority: a user is waiting for `play` and
/// `next`, while queue, background and upgrade work is latency tolerant.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::select::RequestClass;
/// use hearthsound_core::Priority;
///
/// assert_eq!(RequestClass::from_priority(Priority::Play), RequestClass::Interactive);
/// assert_eq!(RequestClass::from_priority(Priority::Upgrade), RequestClass::Background);
/// assert!(RequestClass::from_priority(Priority::Next).is_interactive());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// A user is waiting for audio: `play` and `next`.
    Interactive,
    /// Queue, background and upgrade work.
    Background,
}

impl RequestClass {
    /// Returns the class of an acquisition priority.
    ///
    /// # Parameters
    ///
    /// - `priority`: the priority the acquisition was admitted with.
    ///
    /// # Returns
    ///
    /// [`RequestClass::Interactive`] for [`Priority::Play`] and
    /// [`Priority::Next`], [`RequestClass::Background`] for every other
    /// priority.
    pub fn from_priority(priority: Priority) -> RequestClass {
        match priority {
            Priority::Play | Priority::Next => RequestClass::Interactive,
            Priority::Queue | Priority::Background | Priority::Upgrade => RequestClass::Background,
        }
    }

    /// Returns whether the class is the interactive one.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::select::RequestClass;
    ///
    /// assert!(RequestClass::Interactive.is_interactive());
    /// assert!(!RequestClass::Background.is_interactive());
    /// ```
    pub fn is_interactive(self) -> bool {
        matches!(self, RequestClass::Interactive)
    }
}

/// One candidate together with where it came from.
///
/// The orchestrator accumulates candidates as the provider instances report
/// them, so the instance order and the position inside that instance's result
/// list are the tie breakers selection uses when two candidates are otherwise
/// equal.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::select::CandidateSlot;
/// use hearthsound_core::providers::{Candidate, SourceInfo};
///
/// let candidate = Candidate {
///     token: "token".to_owned(),
///     title: "Title".to_owned(),
///     path: "Artist/Album/01 - Title.flac".to_owned(),
///     files: Vec::new(),
///     source: SourceInfo {
///         key: "peer".to_owned(),
///         speed_bps: Some(500_000),
///         queue_length: None,
///         free_slot: Some(true),
///     },
/// };
/// let slot = CandidateSlot::new(1, 0, candidate);
/// assert_eq!(slot.instance_index, 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSlot {
    /// The position of the provider instance in the acquisition layer chain,
    /// most preferred first.
    pub instance_index: usize,
    /// The position of the candidate in that instance's result list.
    pub candidate_index: usize,
    /// The candidate itself.
    pub candidate: Candidate,
}

impl CandidateSlot {
    /// Creates a slot from its parts.
    ///
    /// # Parameters
    ///
    /// - `instance_index`: the position of the provider instance in the
    ///   acquisition layer chain, most preferred first.
    /// - `candidate_index`: the position of the candidate in that instance's
    ///   result list.
    /// - `candidate`: the candidate the provider reported.
    ///
    /// # Returns
    ///
    /// The slot those values describe.
    pub fn new(
        instance_index: usize,
        candidate_index: usize,
        candidate: Candidate,
    ) -> CandidateSlot {
        CandidateSlot {
            instance_index,
            candidate_index,
            candidate,
        }
    }
}

/// Everything one selection run needs.
///
/// The composition root fills this from the settings registry:
/// `acquisition.min_match_score`, `acquisition.interactive_eta_target_secs`,
/// `acquisition.quality_profile` and `acquisition.max_candidates`; the
/// remaining fields come from the acquisition itself.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::select::{DEFAULT_MAX_CANDIDATES, RequestClass, SelectionRequest};
/// use hearthsound_acquire::profile::QualityProfile;
/// use hearthsound_acquire::score::MatchTarget;
///
/// let request = SelectionRequest::new(
///     RequestClass::Interactive,
///     MatchTarget::new("Title", vec!["Artist".to_owned()], None, None, None),
///     QualityProfile::lossless_preferred(),
/// );
/// assert_eq!(request.max_candidates, DEFAULT_MAX_CANDIDATES);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionRequest {
    /// Which selection rules apply.
    pub class: RequestClass,
    /// The wanted track every candidate is scored against.
    pub target: MatchTarget,
    /// The server-wide quality profile.
    pub profile: QualityProfile,
    /// The acceptance threshold (`acquisition.min_match_score`).
    pub min_match_score: f64,
    /// The ETA ceiling of an interactive request
    /// (`acquisition.interactive_eta_target_secs`); ignored for background
    /// requests.
    pub interactive_eta_target: Duration,
    /// How many candidates one acquisition attempts
    /// (`acquisition.max_candidates`).
    pub max_candidates: usize,
}

impl SelectionRequest {
    /// Creates a request with the default thresholds.
    ///
    /// # Parameters
    ///
    /// - `class`: which selection rules apply.
    /// - `target`: the wanted track.
    /// - `profile`: the server-wide quality profile.
    ///
    /// # Returns
    ///
    /// A request with [`DEFAULT_MIN_MATCH_SCORE`] as its threshold,
    /// [`DEFAULT_INTERACTIVE_ETA_TARGET`] as its ETA target and
    /// [`DEFAULT_MAX_CANDIDATES`] as its cap. Assign the fields directly to use
    /// the settings registry values instead.
    pub fn new(
        class: RequestClass,
        target: MatchTarget,
        profile: QualityProfile,
    ) -> SelectionRequest {
        SelectionRequest {
            class,
            target,
            profile,
            min_match_score: DEFAULT_MIN_MATCH_SCORE,
            interactive_eta_target: DEFAULT_INTERACTIVE_ETA_TARGET,
            max_candidates: DEFAULT_MAX_CANDIDATES,
        }
    }
}

/// A candidate that passed scoring and the profile minimum.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::select::{AttemptPlan, CandidateSlot, RequestClass, SelectionRequest, select_candidates};
/// use hearthsound_acquire::profile::QualityProfile;
/// use hearthsound_acquire::score::MatchTarget;
/// use hearthsound_core::providers::{Candidate, CandidateFile, SourceInfo};
///
/// let candidate = Candidate {
///     token: "token".to_owned(),
///     title: String::new(),
///     path: "Jeff Buckley/Grace/01 - Mojo Pin.flac".to_owned(),
///     files: vec![CandidateFile {
///         name: "01 - Mojo Pin.flac".to_owned(),
///         size: Some(30_000_000),
///         format: Some("flac".to_owned()),
///         bitrate_kbps: None,
///         sample_rate: Some(44_100),
///         bit_depth: Some(16),
///         duration_ms: Some(320_000),
///         track_index: Some(0),
///     }],
///     source: SourceInfo {
///         key: "peer".to_owned(),
///         speed_bps: Some(250_000),
///         queue_length: None,
///         free_slot: Some(true),
///     },
/// };
/// let request = SelectionRequest::new(
///     RequestClass::Interactive,
///     MatchTarget::new("Mojo Pin", vec!["Jeff Buckley".to_owned()], Some("Grace".to_owned()), Some(320_000), Some(1)),
///     QualityProfile::lossless_preferred(),
/// );
/// let plan: AttemptPlan = select_candidates(&request, vec![CandidateSlot::new(0, 0, candidate)]);
/// assert_eq!(plan.attempts.len(), 1);
/// assert_eq!(plan.failure, None);
/// assert_eq!(plan.attempts[0].eta, Some(std::time::Duration::from_secs(120)));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredCandidate {
    /// The position of the provider instance that produced the candidate.
    pub instance_index: usize,
    /// The position of the candidate in that instance's result list.
    pub candidate_index: usize,
    /// The candidate itself.
    pub candidate: Candidate,
    /// Its per-field similarities and weighted total.
    pub score: MatchScore,
    /// The tier of the candidate's first file.
    pub tier: QualityTier,
    /// The position of that tier in the profile's preference order; lower is
    /// preferred.
    pub tier_preference: usize,
    /// The estimated time to download it, `None` when its size or the source's
    /// speed is unknown.
    pub eta: Option<Duration>,
}

/// The candidates one acquisition should try, in order.
///
/// [`AttemptPlan::attempts`] is empty when nothing can be tried, and
/// [`AttemptPlan::failure`] then says why: `no-candidates` for a search that
/// produced nothing or nothing that scored, `no-acceptable-quality` for
/// candidates that all fell below the profile minimum. A non-empty plan carries
/// no failure.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptPlan {
    /// The candidates to try, best first, capped by the request.
    pub attempts: Vec<ScoredCandidate>,
    /// Why there is nothing to try, when [`AttemptPlan::attempts`] is empty.
    pub failure: Option<FailureReason>,
}

/// How much a mapped file's position in the candidate may differ from the
/// wanted track's duration, in milliseconds, for the mapping to count.
pub const ALBUM_DURATION_TOLERANCE_MS: u32 = 5_000;
/// How many files an album candidate may offer beyond the album's track count.
pub const ALBUM_FILE_SLACK: usize = 2;
/// The share of an album's tracks an accepted candidate has to map.
pub const ALBUM_MAPPING_SHARE: f64 = 0.9;
/// The bonus a file's track number adds to its match score while mapping.
pub const ALBUM_TRACK_AGREEMENT_BONUS: f64 = 0.1;

/// One file of an album candidate and the album track it was matched to.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::select::AlbumFileMapping;
///
/// let mapping = AlbumFileMapping {
///     file_index: 2,
///     track_index: 2,
///     score: 0.94,
///     tier: hearthsound_core::QualityTier::Lossless,
/// };
/// assert_eq!(mapping.track_index + 1, 3);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlbumFileMapping {
    /// The position of the file in the candidate's file list.
    pub file_index: u32,
    /// The position of the track in the album's playing order.
    pub track_index: usize,
    /// The file's match score against that track, from 0 to 1.
    pub score: f64,
    /// The quality tier of the file.
    pub tier: QualityTier,
}

/// One album candidate that can be downloaded as a folder.
///
/// Album candidates are whole folders rather than single tracks, so the plan
/// carries the mapping from each file to a track of the album alongside the
/// candidate itself.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumCandidate {
    /// The position of the provider instance that produced the candidate.
    pub instance_index: usize,
    /// The position of the candidate in that instance's result list.
    pub candidate_index: usize,
    /// The candidate itself.
    pub candidate: Candidate,
    /// The lowest quality tier among the mapped files.
    pub tier: QualityTier,
    /// The position of that tier in the profile's preference order; lower is
    /// preferred.
    pub tier_preference: usize,
    /// The estimated time to download the mapped files, `None` when their size
    /// or the source's speed is unknown.
    pub eta: Option<Duration>,
    /// The mean match score of the mapped files.
    pub mean_score: f64,
    /// The files to download, each mapped to one track, in track order.
    pub mappings: Vec<AlbumFileMapping>,
}

/// The album candidates one acquisition should try, in order.
///
/// [`AlbumAttemptPlan::attempts`] is empty when nothing can be tried, and
/// [`AlbumAttemptPlan::failure`] then says why.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumAttemptPlan {
    /// The candidates to try, best first, capped by the request.
    pub attempts: Vec<AlbumCandidate>,
    /// Why there is nothing to try, when [`AlbumAttemptPlan::attempts`] is empty.
    pub failure: Option<FailureReason>,
}

impl AlbumAttemptPlan {
    /// Returns whether the plan has no candidate to try.
    ///
    /// # Returns
    ///
    /// `true` when [`AlbumAttemptPlan::attempts`] is empty.
    pub fn is_empty(&self) -> bool {
        self.attempts.is_empty()
    }
}

/// Everything one album selection run needs.
///
/// It mirrors [`SelectionRequest`]: the same request class, profile, threshold
/// and cap, plus the album title the per-file scores are compared against.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumSelectionRequest {
    /// Which selection rules apply.
    pub class: RequestClass,
    /// The server-wide quality profile.
    pub profile: QualityProfile,
    /// The acceptance threshold (`acquisition.min_match_score`).
    pub min_match_score: f64,
    /// The ETA ceiling of an interactive request
    /// (`acquisition.interactive_eta_target_secs`).
    pub interactive_eta_target: Duration,
    /// How many candidates one acquisition attempts
    /// (`acquisition.max_candidates`).
    pub max_candidates: usize,
    /// The album title, when the wanted tracks carry one.
    pub album: Option<String>,
}

impl AlbumSelectionRequest {
    /// Creates an album request with the default thresholds.
    ///
    /// # Parameters
    ///
    /// - `class`: which selection rules apply.
    /// - `profile`: the server-wide quality profile.
    /// - `album`: the album title, when known.
    ///
    /// # Returns
    ///
    /// A request with [`DEFAULT_MIN_MATCH_SCORE`] as its threshold,
    /// [`DEFAULT_INTERACTIVE_ETA_TARGET`] as its ETA target and
    /// [`DEFAULT_MAX_CANDIDATES`] as its cap.
    pub fn new(
        class: RequestClass,
        profile: QualityProfile,
        album: Option<String>,
    ) -> AlbumSelectionRequest {
        AlbumSelectionRequest {
            class,
            profile,
            min_match_score: DEFAULT_MIN_MATCH_SCORE,
            interactive_eta_target: DEFAULT_INTERACTIVE_ETA_TARGET,
            max_candidates: DEFAULT_MAX_CANDIDATES,
            album,
        }
    }
}

/// Scores, filters, maps and orders the album candidates of one acquisition.
///
/// A candidate is considered when it offers between `N` and `N + 2` files for
/// an album of `N` tracks. Each file is matched against every track with the
/// per-file match score plus a tenth of the track-number agreement, and the
/// pairs are assigned one-to-one greedily, best first. A mapping counts when the
/// file's score clears [`AlbumSelectionRequest::min_match_score`] and its
/// duration is within [`ALBUM_DURATION_TOLERANCE_MS`] of the track's; a file
/// without a duration counts. A candidate is rejected when fewer than
/// `ceil(0.9 N)` tracks are mapped, or when the lowest mapped tier misses the
/// profile minimum.
///
/// The survivors are ordered with every fully mapped candidate first, then by
/// the lowest mapped tier's preference, then by the mean match score, then by
/// the lower ETA - an unknown ETA last - and finally by instance and candidate
/// position.
///
/// # Parameters
///
/// - `request`: the thresholds, the profile and the album title.
/// - `tracks`: the album's wanted tracks in playing order.
/// - `candidates`: every candidate the provider instances accumulated.
///
/// # Returns
///
/// The album attempt plan, capped by
/// [`AlbumSelectionRequest::max_candidates`].
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::select::{
///     AlbumSelectionRequest, RequestClass, select_album_candidates,
/// };
/// use hearthsound_acquire::profile::QualityProfile;
/// use hearthsound_core::providers::WantedTrack;
///
/// let request = AlbumSelectionRequest::new(
///     RequestClass::Background,
///     QualityProfile::lossless_preferred(),
///     Some("Grace".to_owned()),
/// );
/// let track = WantedTrack {
///     title: "Mojo Pin".to_owned(),
///     artists: vec!["Jeff Buckley".to_owned()],
///     track_number: Some(1),
///     disc_number: Some(1),
///     duration_ms: Some(320_000),
///     isrc: None,
///     ids: Vec::new(),
/// };
/// let plan = select_album_candidates(&request, &[track], Vec::new());
/// assert!(plan.is_empty());
/// ```
pub fn select_album_candidates(
    request: &AlbumSelectionRequest,
    tracks: &[WantedTrack],
    candidates: Vec<CandidateSlot>,
) -> AlbumAttemptPlan {
    let total = tracks.len();
    let mut considered = 0_usize;
    let mut mapped = 0_usize;
    let mut attempts = Vec::new();
    if total == 0 {
        return AlbumAttemptPlan {
            attempts,
            failure: Some(FailureReason::NoCandidates),
        };
    }
    for slot in candidates {
        let file_count = slot.candidate.files.len();
        if file_count < total || file_count > total + ALBUM_FILE_SLACK {
            continue;
        }
        considered += 1;
        let Some((mappings, tier, mean_score)) =
            map_album_candidate(request, tracks, &slot.candidate)
        else {
            continue;
        };
        mapped += 1;
        attempts.push(AlbumCandidate {
            instance_index: slot.instance_index,
            candidate_index: slot.candidate_index,
            tier,
            tier_preference: request.profile.preference(tier),
            eta: estimate_eta(&slot.candidate),
            mean_score,
            mappings,
            candidate: slot.candidate,
        });
    }
    let complete = total;
    attempts.sort_by(|left, right| {
        let left_complete = left.mappings.len() == complete;
        let right_complete = right.mappings.len() == complete;
        right_complete
            .cmp(&left_complete)
            .then_with(|| left.tier_preference.cmp(&right.tier_preference))
            .then_with(|| compare_score(right.mean_score, left.mean_score))
            .then_with(|| compare_eta(left.eta, right.eta))
            .then_with(|| left.instance_index.cmp(&right.instance_index))
            .then_with(|| left.candidate_index.cmp(&right.candidate_index))
    });
    attempts.truncate(request.max_candidates);
    let failure = if attempts.is_empty() {
        Some(if mapped > 0 {
            FailureReason::NoCandidates
        } else if considered > 0 {
            FailureReason::NoAcceptableQuality
        } else {
            FailureReason::NoCandidates
        })
    } else {
        None
    };
    AlbumAttemptPlan { attempts, failure }
}

/// Maps one album candidate's files onto the album's tracks.
///
/// # Returns
///
/// The mappings in track order, the lowest mapped tier and the mean mapped
/// score, or `None` when the candidate maps too few tracks or its lowest tier
/// misses the profile minimum.
fn map_album_candidate(
    request: &AlbumSelectionRequest,
    tracks: &[WantedTrack],
    candidate: &Candidate,
) -> Option<(Vec<AlbumFileMapping>, QualityTier, f64)> {
    let total = tracks.len();
    let mut pairs = Vec::new();
    for (file_index, file) in candidate.files.iter().enumerate() {
        let Some(tier) = quality_tier(file) else {
            continue;
        };
        let single = Candidate {
            token: String::new(),
            title: String::new(),
            path: file.name.clone(),
            files: vec![file.clone()],
            source: candidate.source.clone(),
        };
        for (track_index, track) in tracks.iter().enumerate() {
            let target = MatchTarget::from_track(track, request.album.as_deref());
            let MatchOutcome::Matched(score) = score_candidate(&target, &single) else {
                continue;
            };
            if !score.is_accepted(request.min_match_score) {
                continue;
            }
            if !within_album_tolerance(track.duration_ms, file.duration_ms) {
                continue;
            }
            let agreement = track_number_agreement(file.track_index, track_index);
            pairs.push((
                score.total + ALBUM_TRACK_AGREEMENT_BONUS * agreement,
                score.total,
                file_index,
                track_index,
                tier,
            ));
        }
    }
    pairs.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.3.cmp(&right.3))
    });
    let mut used_files = vec![false; candidate.files.len()];
    let mut used_tracks = vec![false; total];
    let mut mappings = Vec::new();
    for (_, score, file_index, track_index, tier) in pairs {
        if used_files[file_index] || used_tracks[track_index] {
            continue;
        }
        used_files[file_index] = true;
        used_tracks[track_index] = true;
        mappings.push(AlbumFileMapping {
            file_index: u32::try_from(file_index).ok()?,
            track_index,
            score,
            tier,
        });
    }
    let required = ((total as f64) * ALBUM_MAPPING_SHARE).ceil() as usize;
    if mappings.len() < required {
        return None;
    }
    let tier = mappings
        .iter()
        .map(|mapping| mapping.tier)
        .min_by_key(|tier| request.profile.preference(*tier))?;
    if !request.profile.meets_minimum(tier) {
        return None;
    }
    mappings.sort_by_key(|mapping| mapping.track_index);
    let mean_score =
        mappings.iter().map(|mapping| mapping.score).sum::<f64>() / mappings.len() as f64;
    Some((mappings, tier, mean_score))
}

/// Returns whether a file's duration is close enough to a track's to map.
///
/// # Parameters
///
/// - `wanted_ms`: the track's duration, when known.
/// - `found_ms`: the file's duration, when known.
///
/// # Returns
///
/// `true` when either duration is unknown, or when the two are within
/// [`ALBUM_DURATION_TOLERANCE_MS`].
fn within_album_tolerance(wanted_ms: Option<u32>, found_ms: Option<u32>) -> bool {
    match (wanted_ms, found_ms) {
        (Some(wanted), Some(found)) => wanted.abs_diff(found) <= ALBUM_DURATION_TOLERANCE_MS,
        _ => true,
    }
}

/// Returns how well a file's track number agrees with a track's position.
///
/// # Parameters
///
/// - `file_index`: the provider's track index for the file, when it reported
///   one.
/// - `track_index`: the track's position in the album's playing order.
///
/// # Returns
///
/// 1 when the two agree, 0.5 when the file names no track, and 0 otherwise.
fn track_number_agreement(file_index: Option<u32>, track_index: usize) -> f64 {
    match file_index {
        Some(index) if usize::try_from(index).ok() == Some(track_index) => 1.0,
        Some(_) => 0.0,
        None => 0.5,
    }
}

/// Scores, filters and orders the candidates of one acquisition.
///
/// Candidates that fail [`score_candidate`] or fall below
/// [`SelectionRequest::min_match_score`] are dropped, as are candidates whose
/// tier is unknown or below [`QualityProfile::minimum`]. The survivors are
/// ordered by the rule of [`SelectionRequest::class`] and truncated to
/// [`SelectionRequest::max_candidates`].
///
/// # Parameters
///
/// - `request`: the thresholds, the profile and the wanted track.
/// - `candidates`: every candidate the provider instances accumulated, with
///   the order they arrived in.
///
/// # Returns
///
/// The attempt plan. When [`SelectionRequest::max_candidates`] is zero the plan
/// is empty even though candidates were acceptable, and the failure is
/// `no-candidates`, because nothing was attempted.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::select::{CandidateSlot, RequestClass, SelectionRequest, select_candidates};
/// use hearthsound_acquire::profile::QualityProfile;
/// use hearthsound_acquire::score::MatchTarget;
/// use hearthsound_core::FailureReason;
///
/// let request = SelectionRequest::new(
///     RequestClass::Interactive,
///     MatchTarget::new("Title", vec!["Artist".to_owned()], None, None, None),
///     QualityProfile::lossless_preferred(),
/// );
/// let plan = select_candidates(&request, Vec::new());
/// assert!(plan.attempts.is_empty());
/// assert_eq!(plan.failure, Some(FailureReason::NoCandidates));
/// ```
pub fn select_candidates(
    request: &SelectionRequest,
    candidates: Vec<CandidateSlot>,
) -> AttemptPlan {
    let mut scored = Vec::new();
    let mut scored_count = 0_usize;
    let mut acceptable_count = 0_usize;
    for slot in candidates {
        let MatchOutcome::Matched(score) = score_candidate(&request.target, &slot.candidate) else {
            continue;
        };
        if !score.is_accepted(request.min_match_score) {
            continue;
        }
        scored_count += 1;
        let Some(tier) = slot.candidate.files.first().and_then(quality_tier) else {
            continue;
        };
        if !request.profile.meets_minimum(tier) {
            continue;
        }
        acceptable_count += 1;
        let eta = estimate_eta(&slot.candidate);
        scored.push(ScoredCandidate {
            instance_index: slot.instance_index,
            candidate_index: slot.candidate_index,
            candidate: slot.candidate,
            score,
            tier,
            tier_preference: request.profile.preference(tier),
            eta,
        });
    }
    match request.class {
        RequestClass::Interactive => {
            let target = request.interactive_eta_target;
            scored.sort_by(|left, right| compare_interactive(left, right, target));
        }
        RequestClass::Background => scored.sort_by(compare_background),
    }
    scored.truncate(request.max_candidates);
    let failure = if scored.is_empty() {
        Some(if scored_count == 0 {
            FailureReason::NoCandidates
        } else if acceptable_count == 0 {
            FailureReason::NoAcceptableQuality
        } else {
            FailureReason::NoCandidates
        })
    } else {
        None
    };
    AttemptPlan {
        attempts: scored,
        failure,
    }
}

/// Estimates how long downloading a candidate takes.
///
/// The estimate is `size / max(speed, 50 KB/s)` plus 30 s for every transfer
/// already queued ahead of it, when the source reports no free upload slot. The
/// size is the sum of the candidate's file sizes.
///
/// # Parameters
///
/// - `candidate`: the candidate to estimate.
///
/// # Returns
///
/// The estimate, or `None` when the candidate has no files, any of its file
/// sizes is unknown, or the source did not report its upload speed. Such a
/// candidate is a last resort rather than an impossibility.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::select::estimate_eta;
/// use hearthsound_core::providers::{Candidate, CandidateFile, SourceInfo};
///
/// let candidate = Candidate {
///     token: "token".to_owned(),
///     title: String::new(),
///     path: "01 - Title.flac".to_owned(),
///     files: vec![CandidateFile {
///         name: "01 - Title.flac".to_owned(),
///         size: Some(10_000_000),
///         format: Some("flac".to_owned()),
///         bitrate_kbps: None,
///         sample_rate: None,
///         bit_depth: None,
///         duration_ms: None,
///         track_index: None,
///     }],
///     source: SourceInfo {
///         key: "peer".to_owned(),
///         speed_bps: Some(100_000),
///         queue_length: Some(3),
///         free_slot: Some(false),
///     },
/// };
/// assert_eq!(estimate_eta(&candidate), Some(std::time::Duration::from_secs(190)));
/// ```
pub fn estimate_eta(candidate: &Candidate) -> Option<Duration> {
    let size = total_size(candidate)?;
    let speed = candidate.source.speed_bps?;
    let effective_speed = speed.max(MIN_TRANSFER_SPEED_BPS);
    let mut seconds = size as f64 / effective_speed as f64;
    if candidate.source.free_slot == Some(false)
        && let Some(queue_length) = candidate.source.queue_length
    {
        seconds += QUEUE_STEP_SECS as f64 * f64::from(queue_length);
    }
    Some(Duration::from_secs_f64(seconds))
}

/// Returns the sum of a candidate's file sizes, or `None` when one is unknown
/// or the candidate has no file at all.
fn total_size(candidate: &Candidate) -> Option<u64> {
    if candidate.files.is_empty() {
        return None;
    }
    candidate
        .files
        .iter()
        .try_fold(0_u64, |total, file| total.checked_add(file.size?))
}

/// Orders two candidates for an interactive request.
///
/// A candidate whose ETA meets the target comes before one whose ETA does not
/// or is unknown; among the first group the higher preferred tier wins, ties go
/// to the higher match score, then to the earlier provider instance and
/// candidate position; among the second group the lower ETA wins, with the same
/// tie breakers, and an unknown ETA comes last.
fn compare_interactive(
    left: &ScoredCandidate,
    right: &ScoredCandidate,
    eta_target: Duration,
) -> Ordering {
    let group = |candidate: &ScoredCandidate| match candidate.eta {
        Some(eta) if eta <= eta_target => 0_u8,
        _ => 1_u8,
    };
    let left_group = group(left);
    let right_group = group(right);
    left_group
        .cmp(&right_group)
        .then_with(|| {
            if left_group == 0 {
                left.tier_preference.cmp(&right.tier_preference)
            } else {
                compare_eta(left.eta, right.eta)
            }
        })
        .then_with(|| compare_score(right.score.total, left.score.total))
        .then_with(|| left.instance_index.cmp(&right.instance_index))
        .then_with(|| left.candidate_index.cmp(&right.candidate_index))
}

/// Orders two candidates for a background request.
///
/// The higher preferred tier wins, then the higher match score, then the lower
/// ETA - an unknown ETA last - then the earlier provider instance and candidate
/// position.
fn compare_background(left: &ScoredCandidate, right: &ScoredCandidate) -> Ordering {
    left.tier_preference
        .cmp(&right.tier_preference)
        .then_with(|| compare_score(right.score.total, left.score.total))
        .then_with(|| compare_eta(left.eta, right.eta))
        .then_with(|| left.instance_index.cmp(&right.instance_index))
        .then_with(|| left.candidate_index.cmp(&right.candidate_index))
}

/// Orders two match scores, higher first.
fn compare_score(left: f64, right: f64) -> Ordering {
    left.total_cmp(&right)
}

/// Orders two ETAs, lower first, with an unknown ETA after every known one.
fn compare_eta(left: Option<Duration>, right: Option<Duration>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_core::providers::{CandidateFile, SourceInfo};

    /// Builds a candidate file with the attributes tests care about.
    fn file(
        name: &str,
        format: Option<&str>,
        bitrate_kbps: Option<u32>,
        size: Option<u64>,
        duration_ms: Option<u32>,
    ) -> CandidateFile {
        CandidateFile {
            name: name.to_owned(),
            size,
            format: format.map(str::to_owned),
            bitrate_kbps,
            sample_rate: None,
            bit_depth: None,
            duration_ms,
            track_index: None,
        }
    }

    /// Builds a candidate with one file.
    fn candidate(
        path: &str,
        file: CandidateFile,
        speed_bps: Option<u64>,
        queue_length: Option<u32>,
        free_slot: Option<bool>,
    ) -> Candidate {
        Candidate {
            token: format!("token:{path}"),
            title: String::new(),
            path: path.to_owned(),
            files: vec![file],
            source: SourceInfo {
                key: "peer".to_owned(),
                speed_bps,
                queue_length,
                free_slot,
            },
        }
    }

    /// Builds a lossless candidate for `Mojo Pin`.
    fn flac(path: &str, size: Option<u64>, speed_bps: Option<u64>) -> Candidate {
        candidate(
            path,
            file(
                "01 - Mojo Pin.flac",
                Some("flac"),
                None,
                size,
                Some(320_000),
            ),
            speed_bps,
            Some(0),
            Some(true),
        )
    }

    /// Builds an MP3 candidate for `Mojo Pin` at `bitrate` kbps.
    fn mp3(path: &str, bitrate: u32, size: Option<u64>, speed_bps: Option<u64>) -> Candidate {
        candidate(
            path,
            file(
                "01 - Mojo Pin.mp3",
                Some("mp3"),
                Some(bitrate),
                size,
                Some(320_000),
            ),
            speed_bps,
            Some(0),
            Some(true),
        )
    }

    /// Builds the request every test starts from.
    fn request(class: RequestClass, profile: QualityProfile) -> SelectionRequest {
        SelectionRequest::new(
            class,
            MatchTarget::new(
                "Mojo Pin",
                vec!["Jeff Buckley".to_owned()],
                Some("Grace".to_owned()),
                Some(320_000),
                Some(1),
            ),
            profile,
        )
    }

    /// Builds the default request used by ordering tests.
    fn interactive() -> SelectionRequest {
        request(
            RequestClass::Interactive,
            QualityProfile::lossless_preferred(),
        )
    }

    #[test]
    fn eta_divides_size_by_speed() {
        let eta = estimate_eta(&flac(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            Some(10_000_000),
            Some(100_000),
        ));
        assert_eq!(eta, Some(Duration::from_secs(100)));
    }

    #[test]
    fn eta_assumes_a_speed_floor() {
        let slow = flac(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            Some(10_000_000),
            Some(10_000),
        );
        let floor = estimate_eta(&slow).unwrap();
        assert_eq!(floor, Duration::from_secs_f64(10_000_000.0 / 51_200.0));
        assert!(floor > Duration::from_secs(195));
        let zero = flac(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            Some(10_000_000),
            Some(0),
        );
        assert_eq!(estimate_eta(&zero), Some(floor));
    }

    #[test]
    fn eta_adds_thirty_seconds_per_queued_transfer() {
        let mut queued = flac(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            Some(10_000_000),
            Some(100_000),
        );
        queued.source.free_slot = Some(false);
        queued.source.queue_length = Some(3);
        assert_eq!(estimate_eta(&queued), Some(Duration::from_secs(190)));

        queued.source.queue_length = Some(0);
        assert_eq!(estimate_eta(&queued), Some(Duration::from_secs(100)));
    }

    #[test]
    fn eta_ignores_the_queue_when_a_slot_is_free_or_unknown() {
        let mut free = flac(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            Some(10_000_000),
            Some(100_000),
        );
        free.source.free_slot = Some(true);
        free.source.queue_length = Some(5);
        assert_eq!(estimate_eta(&free), Some(Duration::from_secs(100)));

        let mut unknown = flac(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            Some(10_000_000),
            Some(100_000),
        );
        unknown.source.free_slot = None;
        unknown.source.queue_length = Some(5);
        assert_eq!(estimate_eta(&unknown), Some(Duration::from_secs(100)));

        let mut no_position = flac(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            Some(10_000_000),
            Some(100_000),
        );
        no_position.source.free_slot = Some(false);
        no_position.source.queue_length = None;
        assert_eq!(estimate_eta(&no_position), Some(Duration::from_secs(100)));
    }

    #[test]
    fn eta_sums_the_files_of_a_candidate() {
        let mut multi = flac("Album/01 - Mojo Pin.flac", Some(10_000_000), Some(100_000));
        multi.files.push(file(
            "Album/02 - Grace.flac",
            Some("flac"),
            None,
            Some(5_000_000),
            None,
        ));
        assert_eq!(estimate_eta(&multi), Some(Duration::from_secs(150)));
    }

    #[test]
    fn eta_is_unknown_without_a_size_or_a_speed() {
        assert_eq!(
            estimate_eta(&flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                None,
                Some(100_000)
            )),
            None
        );
        assert_eq!(
            estimate_eta(&flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(10_000_000),
                None
            )),
            None
        );
        let mut partial = flac(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            Some(10_000_000),
            Some(100_000),
        );
        partial
            .files
            .push(file("Grace/02.flac", Some("flac"), None, None, None));
        assert_eq!(estimate_eta(&partial), None);
        let mut empty = flac(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            Some(10_000_000),
            Some(100_000),
        );
        empty.files.clear();
        assert_eq!(estimate_eta(&empty), None);
    }

    #[test]
    fn an_empty_search_reports_no_candidates() {
        let plan = select_candidates(&interactive(), Vec::new());
        assert!(plan.attempts.is_empty());
        assert_eq!(plan.failure, Some(FailureReason::NoCandidates));
    }

    #[test]
    fn candidates_that_do_not_score_report_no_candidates() {
        let other = candidate(
            "Jeff Buckley/Grace/05 - Last Goodbye.flac",
            file(
                "05 - Last Goodbye.flac",
                Some("flac"),
                None,
                Some(10_000_000),
                Some(315_000),
            ),
            Some(100_000),
            None,
            Some(true),
        );
        let far_off = candidate(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            file(
                "01 - Mojo Pin.flac",
                Some("flac"),
                None,
                Some(10_000_000),
                Some(200_000),
            ),
            Some(100_000),
            None,
            Some(true),
        );
        let plan = select_candidates(
            &interactive(),
            vec![
                CandidateSlot::new(0, 0, other),
                CandidateSlot::new(0, 1, far_off),
            ],
        );
        assert!(plan.attempts.is_empty());
        assert_eq!(plan.failure, Some(FailureReason::NoCandidates));
    }

    #[test]
    fn candidates_below_the_profile_minimum_report_no_acceptable_quality() {
        let low = mp3(
            "Jeff Buckley/Grace/01 - Mojo Pin.mp3",
            128,
            Some(1_000_000),
            Some(100_000),
        );
        let unknown = candidate(
            "Jeff Buckley/Grace/01 - Mojo Pin.ape",
            file(
                "01 - Mojo Pin.ape",
                None,
                None,
                Some(10_000_000),
                Some(320_000),
            ),
            Some(100_000),
            None,
            Some(true),
        );
        let plan = select_candidates(
            &interactive(),
            vec![
                CandidateSlot::new(0, 0, low),
                CandidateSlot::new(0, 1, unknown),
            ],
        );
        assert!(plan.attempts.is_empty());
        assert_eq!(plan.failure, Some(FailureReason::NoAcceptableQuality));
    }

    #[test]
    fn a_zero_cap_attempts_nothing() {
        let mut request = interactive();
        request.max_candidates = 0;
        let plan = select_candidates(
            &request,
            vec![CandidateSlot::new(
                0,
                0,
                flac(
                    "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                    Some(1_000_000),
                    Some(100_000),
                ),
            )],
        );
        assert!(plan.attempts.is_empty());
        assert_eq!(plan.failure, Some(FailureReason::NoCandidates));
    }

    #[test]
    fn candidates_below_the_threshold_are_dropped() {
        let mut request = interactive();
        request.min_match_score = 0.9;
        let imperfect = candidate(
            "Jeff Buckley/Grace/01 - Mojo Pin.flac",
            file(
                "01 - Mojo Pin.flac",
                Some("flac"),
                None,
                Some(1_000_000),
                Some(330_000),
            ),
            Some(100_000),
            None,
            Some(true),
        );
        let plan = select_candidates(&request, vec![CandidateSlot::new(0, 0, imperfect)]);
        assert_eq!(plan.failure, Some(FailureReason::NoCandidates));

        let mut lax = interactive();
        lax.min_match_score = 0.8;
        let plan = select_candidates(
            &lax,
            vec![CandidateSlot::new(
                0,
                0,
                candidate(
                    "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                    file(
                        "01 - Mojo Pin.flac",
                        Some("flac"),
                        None,
                        Some(1_000_000),
                        Some(330_000),
                    ),
                    Some(100_000),
                    None,
                    Some(true),
                ),
            )],
        );
        assert_eq!(plan.attempts.len(), 1);
        assert_eq!(plan.failure, None);
    }

    #[test]
    fn interactive_prefers_the_best_tier_within_the_eta_target() {
        let request = interactive();
        let fast_lossy = CandidateSlot::new(
            0,
            1,
            mp3(
                "Jeff Buckley/Grace/01 - Mojo Pin.mp3",
                320,
                Some(10_000_000),
                Some(1_000_000),
            ),
        );
        let slow_lossless = CandidateSlot::new(
            0,
            0,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(100_000_000),
                Some(1_000_000),
            ),
        );
        let plan = select_candidates(&request, vec![slow_lossless, fast_lossy]);
        assert_eq!(plan.failure, None);
        assert_eq!(plan.attempts.len(), 2);
        assert_eq!(plan.attempts[0].tier, QualityTier::LossyHigh);
        assert_eq!(plan.attempts[0].eta, Some(Duration::from_secs(10)));
        assert_eq!(plan.attempts[1].tier, QualityTier::Lossless);
        assert_eq!(plan.attempts[1].eta, Some(Duration::from_secs(100)));
    }

    #[test]
    fn interactive_prefers_the_lossless_tier_when_both_fit() {
        let request = interactive();
        let mut hires_file = file(
            "01 - Mojo Pin.flac",
            Some("flac"),
            None,
            Some(10_000_000),
            Some(320_000),
        );
        hires_file.bit_depth = Some(24);
        let hires = CandidateSlot::new(
            0,
            0,
            candidate(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                hires_file,
                Some(1_000_000),
                None,
                Some(true),
            ),
        );
        let lossless = CandidateSlot::new(
            0,
            1,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(10_000_000),
                Some(1_000_000),
            ),
        );
        let plan = select_candidates(&request, vec![hires, lossless]);
        assert_eq!(plan.attempts[0].tier, QualityTier::Lossless);
        assert_eq!(plan.attempts[1].tier, QualityTier::LosslessHires);
    }

    #[test]
    fn interactive_falls_back_to_the_lowest_eta() {
        let request = interactive();
        let slow_lossless = CandidateSlot::new(
            0,
            0,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(300_000_000),
                Some(100_000),
            ),
        );
        let faster_lossless = CandidateSlot::new(
            1,
            0,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(100_000_000),
                Some(100_000),
            ),
        );
        let plan = select_candidates(&request, vec![slow_lossless, faster_lossless]);
        assert_eq!(plan.attempts.len(), 2);
        assert_eq!(plan.attempts[0].instance_index, 1);
        assert_eq!(plan.attempts[0].eta, Some(Duration::from_secs(1000)));
        assert_eq!(plan.attempts[1].eta, Some(Duration::from_secs(3000)));
    }

    #[test]
    fn interactive_puts_unknown_etas_last() {
        let request = interactive();
        let unknown = CandidateSlot::new(
            0,
            0,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(500_000_000),
                None,
            ),
        );
        let known = CandidateSlot::new(
            0,
            1,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(500_000_000),
                Some(100_000),
            ),
        );
        let plan = select_candidates(&request, vec![unknown, known]);
        assert_eq!(plan.attempts[0].eta, Some(Duration::from_secs(5000)));
        assert_eq!(plan.attempts[1].eta, None);
    }

    #[test]
    fn interactive_breaks_ties_by_score_then_instance() {
        let request = interactive();
        let better_title = CandidateSlot::new(
            2,
            7,
            candidate(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                file(
                    "01 - Mojo Pin.flac",
                    Some("flac"),
                    None,
                    Some(10_000_000),
                    Some(320_000),
                ),
                Some(100_000),
                None,
                Some(true),
            ),
        );
        let worse_title = CandidateSlot::new(
            0,
            0,
            candidate(
                "Jeff Buckley/Grace/01 - Mojo Pin (Remaster).flac",
                file(
                    "01 - Mojo Pin (Remaster).flac",
                    Some("flac"),
                    None,
                    Some(10_000_000),
                    Some(330_000),
                ),
                Some(100_000),
                None,
                Some(true),
            ),
        );
        let plan = select_candidates(&request, vec![worse_title, better_title.clone()]);
        assert!(plan.attempts[0].score.total > plan.attempts[1].score.total);
        assert_eq!(plan.attempts[0].instance_index, 2);

        let same_score = CandidateSlot::new(
            1,
            4,
            candidate(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                file(
                    "01 - Mojo Pin.flac",
                    Some("flac"),
                    None,
                    Some(10_000_000),
                    Some(320_000),
                ),
                Some(100_000),
                None,
                Some(true),
            ),
        );
        let plan = select_candidates(&request, vec![better_title, same_score]);
        assert_eq!(plan.attempts[0].instance_index, 1);
        assert_eq!(plan.attempts[0].candidate_index, 4);
    }

    #[test]
    fn background_prefers_quality_over_latency() {
        let request = request(
            RequestClass::Background,
            QualityProfile::lossless_preferred(),
        );
        let fast_lossy = CandidateSlot::new(
            0,
            0,
            mp3(
                "Jeff Buckley/Grace/01 - Mojo Pin.mp3",
                320,
                Some(10_000_000),
                Some(1_000_000),
            ),
        );
        let slow_lossless = CandidateSlot::new(
            0,
            1,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(100_000_000),
                Some(100_000),
            ),
        );
        let plan = select_candidates(&request, vec![fast_lossy, slow_lossless]);
        assert_eq!(plan.attempts[0].tier, QualityTier::Lossless);
        assert_eq!(plan.attempts[1].tier, QualityTier::LossyHigh);
    }

    #[test]
    fn background_orders_by_score_then_eta() {
        let request = request(
            RequestClass::Background,
            QualityProfile::lossless_preferred(),
        );
        let slower = CandidateSlot::new(
            0,
            0,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(100_000_000),
                Some(100_000),
            ),
        );
        let faster = CandidateSlot::new(
            3,
            0,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(10_000_000),
                Some(100_000),
            ),
        );
        let plan = select_candidates(&request, vec![slower, faster]);
        assert_eq!(plan.attempts[0].instance_index, 3);
        assert_eq!(plan.attempts[0].eta, Some(Duration::from_secs(100)));
        assert_eq!(plan.attempts[1].eta, Some(Duration::from_secs(1000)));
    }

    #[test]
    fn the_compact_profile_prefers_lossy_high() {
        let request = request(RequestClass::Background, QualityProfile::compact());
        let lossless = CandidateSlot::new(
            0,
            0,
            flac(
                "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                Some(10_000_000),
                Some(100_000),
            ),
        );
        let lossy = CandidateSlot::new(
            0,
            1,
            mp3(
                "Jeff Buckley/Grace/01 - Mojo Pin.mp3",
                320,
                Some(10_000_000),
                Some(100_000),
            ),
        );
        let plan = select_candidates(&request, vec![lossless, lossy]);
        assert_eq!(plan.attempts[0].tier, QualityTier::LossyHigh);
        assert_eq!(plan.attempts[1].tier, QualityTier::Lossless);
    }

    #[test]
    fn the_attempt_list_is_capped() {
        let mut request = interactive();
        request.max_candidates = 2;
        let slots = (0..4)
            .map(|index| {
                CandidateSlot::new(
                    index,
                    0,
                    flac(
                        "Jeff Buckley/Grace/01 - Mojo Pin.flac",
                        Some(1_000_000),
                        Some(100_000),
                    ),
                )
            })
            .collect();
        let plan = select_candidates(&request, slots);
        assert_eq!(plan.attempts.len(), 2);
        assert_eq!(plan.attempts[0].instance_index, 0);
        assert_eq!(plan.attempts[1].instance_index, 1);
        assert_eq!(plan.failure, None);
    }

    #[test]
    fn request_classes_follow_the_priority() {
        assert_eq!(
            RequestClass::from_priority(Priority::Play),
            RequestClass::Interactive
        );
        assert_eq!(
            RequestClass::from_priority(Priority::Next),
            RequestClass::Interactive
        );
        for priority in [Priority::Queue, Priority::Background, Priority::Upgrade] {
            assert_eq!(
                RequestClass::from_priority(priority),
                RequestClass::Background,
                "{priority}"
            );
        }
    }

    #[test]
    fn a_request_starts_from_the_documented_defaults() {
        let request = interactive();
        assert_eq!(request.min_match_score, DEFAULT_MIN_MATCH_SCORE);
        assert_eq!(request.interactive_eta_target, Duration::from_secs(60));
        assert_eq!(request.max_candidates, DEFAULT_MAX_CANDIDATES);
    }

    /// Builds the wanted tracks of a release titled `Album` with `count` tracks.
    fn album_tracks(count: usize) -> Vec<WantedTrack> {
        (1..=count)
            .map(|number| WantedTrack {
                title: format!("Album {number}"),
                artists: vec!["Hello Artist".to_owned()],
                track_number: u16::try_from(number).ok(),
                disc_number: Some(1),
                duration_ms: Some(200_000),
                isrc: None,
                ids: Vec::new(),
            })
            .collect()
    }

    /// Builds an album folder whose files are named after the release's tracks.
    fn album_candidate(
        token: &str,
        tracks: usize,
        format: &str,
        bitrate_kbps: Option<u32>,
        speed_bps: Option<u64>,
    ) -> CandidateSlot {
        let files = (1..=tracks)
            .map(|number| CandidateFile {
                name: format!("{number:02} - Album {number}.{format}"),
                size: Some(1_000_000),
                format: Some(format.to_owned()),
                bitrate_kbps,
                sample_rate: Some(44_100),
                bit_depth: None,
                duration_ms: Some(200_000),
                track_index: Some(u32::try_from(number - 1).unwrap()),
            })
            .collect();
        CandidateSlot::new(
            0,
            0,
            Candidate {
                token: token.to_owned(),
                title: "Album".to_owned(),
                path: "Hello Artist/Album".to_owned(),
                files,
                source: SourceInfo {
                    key: "peer".to_owned(),
                    speed_bps,
                    queue_length: Some(0),
                    free_slot: Some(true),
                },
            },
        )
    }

    /// An album request with the default thresholds.
    fn album_request() -> AlbumSelectionRequest {
        AlbumSelectionRequest::new(
            RequestClass::Background,
            QualityProfile::lossless_preferred(),
            Some("Album".to_owned()),
        )
    }

    #[test]
    fn a_complete_folder_beats_a_partial_higher_tier_folder() {
        let mut partial = album_candidate("partial", 2, "flac", None, None);
        // The partial folder only offers the first two tracks of three.
        partial.candidate.files.truncate(2);
        let complete = album_candidate("complete", 3, "mp3", Some(320), None);
        let plan =
            select_album_candidates(&album_request(), &album_tracks(3), vec![partial, complete]);
        assert_eq!(plan.attempts.len(), 1, "the partial folder is rejected");
        assert_eq!(plan.attempts[0].candidate.token, "complete");
        assert_eq!(plan.attempts[0].mappings.len(), 3);
        assert_eq!(plan.failure, None);
    }

    #[test]
    fn a_folder_with_the_wrong_file_count_is_rejected() {
        let too_few = album_candidate("too-few", 1, "flac", None, None);
        let too_many = album_candidate("too-many", 5, "flac", None, None);
        let plan =
            select_album_candidates(&album_request(), &album_tracks(2), vec![too_few, too_many]);
        assert!(plan.attempts.is_empty());
        assert_eq!(plan.failure, Some(FailureReason::NoCandidates));
    }

    #[test]
    fn a_folder_below_the_mapping_share_is_rejected() {
        let mut slots = Vec::new();
        // Eight of ten tracks are named correctly; the rest are unrelated.
        for number in 1..=8 {
            slots.push(CandidateSlot::new(
                0,
                number,
                Candidate {
                    token: format!("partial-{number}"),
                    title: String::new(),
                    path: "Hello Artist/Album".to_owned(),
                    files: vec![CandidateFile {
                        name: format!("{number:02} - Album {number}.flac"),
                        size: Some(1_000_000),
                        format: Some("flac".to_owned()),
                        bitrate_kbps: None,
                        sample_rate: Some(44_100),
                        bit_depth: None,
                        duration_ms: Some(200_000),
                        track_index: Some(u32::try_from(number - 1).unwrap()),
                    }],
                    source: SourceInfo {
                        key: "peer".to_owned(),
                        speed_bps: None,
                        queue_length: None,
                        free_slot: None,
                    },
                },
            ));
        }
        let plan = select_album_candidates(&album_request(), &album_tracks(10), slots);
        assert!(plan.attempts.is_empty());
    }

    #[test]
    fn a_folder_below_the_profile_minimum_is_rejected() {
        let mut request = album_request();
        request.profile.minimum = hearthsound_core::QualityTier::LossyHigh;
        let candidate = album_candidate("low", 2, "mp3", Some(128), None);
        let plan = select_album_candidates(&request, &album_tracks(2), vec![candidate]);
        assert!(plan.attempts.is_empty());
        assert_eq!(plan.failure, Some(FailureReason::NoAcceptableQuality));
    }

    #[test]
    fn the_mapping_follows_the_file_names() {
        let mut candidate = album_candidate("shuffled", 2, "flac", None, None);
        candidate.candidate.files.swap(0, 1);
        let plan =
            select_album_candidates(&album_request(), &album_tracks(2), vec![candidate.clone()]);
        assert_eq!(plan.attempts.len(), 1);
        let mappings = &plan.attempts[0].mappings;
        assert_eq!(mappings.len(), 2);
        for mapping in mappings {
            let file = &candidate.candidate.files[mapping.file_index as usize];
            assert!(
                file.name
                    .contains(&format!("Album {}", mapping.track_index + 1)),
                "`{}` is mapped to track {}",
                file.name,
                mapping.track_index + 1
            );
        }
    }
}
