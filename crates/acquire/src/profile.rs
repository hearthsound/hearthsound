//! Quality tiers and the named quality profiles.
//!
//! A tier is a bucket of formats and bit depths, not a specific format; the
//! core assigns it from the raw attributes the provider reports, so every
//! plugin can stay a thin adapter. [`quality_tier`] implements the buckets of
//! the acquisition design:
//!
//! | Tier | Definition |
//! |---|---|
//! | `lossless-hires` | lossless with a 24-bit depth, or a sample rate above 48 kHz |
//! | `lossless` | FLAC, ALAC, WAV, AIFF or WavPack, 16-bit, at or below 48 kHz |
//! | `lossy-high` | MP3 320 CBR or V0, AAC at or above 256, Opus at or above 160, Vorbis at or above 256 |
//! | `lossy-mid` | MP3 at or above 192, AAC at or above 160, Opus at or above 112, Vorbis at or above 160 |
//! | `lossy-low` | anything below `lossy-mid` |
//!
//! A quality profile is a search order of tiers plus the two bounds the
//! acquisition uses: [`QualityProfile::minimum`], below which a candidate is
//! rejected, and [`QualityProfile::cutoff`], at or above which no upgrade is
//! attempted. The two built-in profiles are `lossless-preferred` (the default)
//! and `compact`; [`QualityProfile::builtin`] looks one up by the name the
//! `acquisition.quality_profile` setting holds.
//!
//! The tier comparison is the absolute quality order, not the profile order:
//! `lossless-hires` is above `lossless` even in a profile that prefers
//! `lossless` first. The profile order only decides which tier the selection
//! tries first (see [`crate::select`]).

use hearthsound_core::QualityTier;
use hearthsound_core::providers::CandidateFile;

/// Bit depth at or above which a lossless file is hi-res.
pub const HIRES_BIT_DEPTH: u8 = 24;
/// Sample rate above which a lossless file is hi-res.
pub const HIRES_SAMPLE_RATE: u32 = 48_000;
/// Reported MP3 bitrate at or above which a VBR file counts as V0.
///
/// A backend reports the average bitrate of a VBR file, and MP3 V0 averages
/// about 245 kbps, so the lossy-high boundary for MP3 sits there instead of at
/// 320: without it every V0 rip would be misfiled as `lossy-mid`. The cost is
/// that a 256 kbps CBR file is read as `lossy-high` too, which is the tier the
/// design would otherwise leave undefined for it.
pub const MP3_V0_BITRATE_KBPS: u32 = 245;
/// MP3 bitrate at or above which a file is `lossy-mid`.
pub const MP3_MID_BITRATE_KBPS: u32 = 192;
/// AAC bitrate at or above which a file is `lossy-high`.
pub const AAC_HIGH_BITRATE_KBPS: u32 = 256;
/// AAC bitrate at or above which a file is `lossy-mid`.
pub const AAC_MID_BITRATE_KBPS: u32 = 160;
/// Opus bitrate at or above which a file is `lossy-high`.
pub const OPUS_HIGH_BITRATE_KBPS: u32 = 160;
/// Opus bitrate at or above which a file is `lossy-mid`.
pub const OPUS_MID_BITRATE_KBPS: u32 = 112;
/// Vorbis bitrate at or above which a file is `lossy-high`.
pub const VORBIS_HIGH_BITRATE_KBPS: u32 = 256;
/// Vorbis bitrate at or above which a file is `lossy-mid`.
pub const VORBIS_MID_BITRATE_KBPS: u32 = 160;

/// Returns the absolute quality order of a tier, worst first.
///
/// # Parameters
///
/// - `tier`: the tier to rank.
///
/// # Returns
///
/// `0` for [`QualityTier::LossyLow`] through `4` for
/// [`QualityTier::LosslessHires`]. Two tiers compare by their rank.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::profile::quality_rank;
/// use hearthsound_core::QualityTier;
///
/// assert!(quality_rank(QualityTier::LosslessHires) > quality_rank(QualityTier::Lossless));
/// assert!(quality_rank(QualityTier::Lossless) > quality_rank(QualityTier::LossyHigh));
/// assert_eq!(quality_rank(QualityTier::LossyLow), 0);
/// ```
pub const fn quality_rank(tier: QualityTier) -> u8 {
    match tier {
        QualityTier::LossyLow => 0,
        QualityTier::LossyMid => 1,
        QualityTier::LossyHigh => 2,
        QualityTier::Lossless => 3,
        QualityTier::LosslessHires => 4,
    }
}

/// Returns the quality tier of a candidate file.
///
/// The tier comes from the file's format, bitrate, bit depth and sample rate.
/// The format is the reported one, or the file name's extension when the
/// provider reported none, so a Soulseek result whose extension is all the
/// backend knows still gets a tier.
///
/// # Parameters
///
/// - `file`: the candidate file as the acquisition provider reported it.
///
/// # Returns
///
/// The best tier the attributes allow, or `None` when they do not decide:
///
/// - a format outside the families of the table above (`.ape`, `.wma`, a MIME
///   type the canonicalizer does not know) has no defined tier;
/// - a lossy file without a reported bitrate (or with a reported bitrate of
///   zero) has none either, and neither has a file with no format at all;
/// - a lossless file defaults to [`QualityTier::Lossless`] when neither its bit
///   depth nor its sample rate indicates hi-res, because a 16-bit 44.1 kHz read
///   is what a lossless file without those attributes behaves like.
///
/// A `.m4a` or `.mp4` file is read as AAC unless the provider names `alac`,
/// because the container alone does not say which codec is inside it.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::profile::quality_tier;
/// use hearthsound_core::QualityTier;
/// use hearthsound_core::providers::CandidateFile;
///
/// let file = CandidateFile {
///     name: "01 - Title.flac".to_owned(),
///     size: Some(30_000_000),
///     format: None,
///     bitrate_kbps: None,
///     sample_rate: Some(96_000),
///     bit_depth: Some(24),
///     duration_ms: Some(300_000),
///     track_index: None,
/// };
/// assert_eq!(quality_tier(&file), Some(QualityTier::LosslessHires));
/// ```
pub fn quality_tier(file: &CandidateFile) -> Option<QualityTier> {
    match file_format(file)?.as_str() {
        "flac" | "alac" | "wav" | "aiff" | "wv" => {
            Some(lossless_tier(file.bit_depth, file.sample_rate))
        }
        "mp3" => bitrate_tier(file.bitrate_kbps, MP3_V0_BITRATE_KBPS, MP3_MID_BITRATE_KBPS),
        "aac" | "m4a" | "mp4" => bitrate_tier(
            file.bitrate_kbps,
            AAC_HIGH_BITRATE_KBPS,
            AAC_MID_BITRATE_KBPS,
        ),
        "opus" => bitrate_tier(
            file.bitrate_kbps,
            OPUS_HIGH_BITRATE_KBPS,
            OPUS_MID_BITRATE_KBPS,
        ),
        "ogg" => bitrate_tier(
            file.bitrate_kbps,
            VORBIS_HIGH_BITRATE_KBPS,
            VORBIS_MID_BITRATE_KBPS,
        ),
        _ => None,
    }
}

/// Returns the canonical format of a candidate file.
///
/// # Parameters
///
/// - `file`: the candidate file. Its `format` field wins; the file name's
///   extension is the fallback.
///
/// # Returns
///
/// A lowercase name such as `flac`, `mp3` or `ogg`, after stripping an
/// `audio/x-` style MIME prefix and folding the aliases (`mpeg` to `mp3`,
/// `wave` to `wav`, `vorbis` to `ogg`, `wavpack` to `wv`, `aifc` and `aif` to
/// `aiff`, `mp4` to `m4a`). `None` when neither source names a format.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::profile::file_format;
/// use hearthsound_core::providers::CandidateFile;
///
/// let file = CandidateFile {
///     name: "01 - Title.FLAC".to_owned(),
///     size: None,
///     format: None,
///     bitrate_kbps: None,
///     sample_rate: None,
///     bit_depth: None,
///     duration_ms: None,
///     track_index: None,
/// };
/// assert_eq!(file_format(&file).as_deref(), Some("flac"));
/// ```
pub fn file_format(file: &CandidateFile) -> Option<String> {
    let reported = file
        .format
        .as_deref()
        .map(str::trim)
        .filter(|format| !format.is_empty());
    let raw = match reported {
        Some(format) => format,
        None => extension_of(&file.name)?,
    };
    let lowered = raw.to_lowercase();
    let lowered = lowered.strip_prefix("audio/").unwrap_or(&lowered);
    let lowered = lowered.strip_prefix("x-").unwrap_or(lowered);
    Some(
        match lowered {
            "mpeg" | "mpeg1" | "mpeg-1" | "mp3" => "mp3",
            "wave" => "wav",
            "vorbis" => "ogg",
            "aif" | "aifc" => "aiff",
            "wavpack" => "wv",
            "mp4" => "m4a",
            other => other,
        }
        .to_owned(),
    )
}

/// Returns the tier of a lossless file.
fn lossless_tier(bit_depth: Option<u8>, sample_rate: Option<u32>) -> QualityTier {
    let hi_res = bit_depth.is_some_and(|depth| depth >= HIRES_BIT_DEPTH)
        || sample_rate.is_some_and(|rate| rate > HIRES_SAMPLE_RATE);
    if hi_res {
        QualityTier::LosslessHires
    } else {
        QualityTier::Lossless
    }
}

/// Returns the tier a lossy file's bitrate implies.
///
/// # Parameters
///
/// - `bitrate_kbps`: the reported bitrate; `None` and zero mean unknown.
/// - `high_kbps`: the bitrate at or above which the file is `lossy-high`.
/// - `mid_kbps`: the bitrate at or above which the file is `lossy-mid`.
fn bitrate_tier(bitrate_kbps: Option<u32>, high_kbps: u32, mid_kbps: u32) -> Option<QualityTier> {
    let bitrate = bitrate_kbps.filter(|kbps| *kbps > 0)?;
    Some(if bitrate >= high_kbps {
        QualityTier::LossyHigh
    } else if bitrate >= mid_kbps {
        QualityTier::LossyMid
    } else {
        QualityTier::LossyLow
    })
}

/// Returns a file name's extension, when it looks like one.
fn extension_of(name: &str) -> Option<&str> {
    let (stem, extension) = name.rsplit_once('.')?;
    let plausible = !stem.is_empty()
        && (1..=5).contains(&extension.len())
        && extension.chars().all(|c| c.is_ascii_alphanumeric());
    plausible.then_some(extension)
}

/// A named quality profile.
///
/// A profile is the ordered preference list of tiers the selection walks, plus
/// the minimum tier a candidate has to reach and the cutoff tier above which
/// the track needs no upgrade. The profile is server-wide; the admin selects it
/// through `acquisition.quality_profile`.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::profile::QualityProfile;
/// use hearthsound_core::QualityTier;
///
/// let profile = QualityProfile::builtin("compact").expect("a built-in profile");
/// assert_eq!(profile.name, "compact");
/// assert_eq!(profile.order.first(), Some(&QualityTier::LossyHigh));
/// assert!(profile.meets_minimum(QualityTier::LossyHigh));
/// assert!(!profile.meets_minimum(QualityTier::LossyLow));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityProfile {
    /// The profile name, as `acquisition.quality_profile` spells it.
    pub name: String,
    /// The tiers in preference order, best first.
    pub order: Vec<QualityTier>,
    /// The tier below which candidates are rejected.
    pub minimum: QualityTier,
    /// The tier at or above which no upgrade is attempted.
    pub cutoff: QualityTier,
}

impl QualityProfile {
    /// Every built-in profile name, in a fixed order.
    pub const BUILTIN_NAMES: [&'static str; 2] = ["lossless-preferred", "compact"];

    /// Returns the default profile, `lossless-preferred`.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::profile::QualityProfile;
    /// use hearthsound_core::QualityTier;
    ///
    /// let profile = QualityProfile::lossless_preferred();
    /// assert_eq!(profile.minimum, QualityTier::LossyMid);
    /// assert_eq!(profile.cutoff, QualityTier::Lossless);
    /// assert_eq!(
    ///     profile.order,
    ///     [
    ///         QualityTier::Lossless,
    ///         QualityTier::LosslessHires,
    ///         QualityTier::LossyHigh,
    ///         QualityTier::LossyMid,
    ///     ]
    /// );
    /// ```
    pub fn lossless_preferred() -> QualityProfile {
        QualityProfile {
            name: "lossless-preferred".to_owned(),
            order: vec![
                QualityTier::Lossless,
                QualityTier::LosslessHires,
                QualityTier::LossyHigh,
                QualityTier::LossyMid,
            ],
            minimum: QualityTier::LossyMid,
            cutoff: QualityTier::Lossless,
        }
    }

    /// Returns the `compact` profile, which prefers the best lossy tier.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::profile::QualityProfile;
    /// use hearthsound_core::QualityTier;
    ///
    /// let profile = QualityProfile::compact();
    /// assert_eq!(profile.cutoff, QualityTier::LossyHigh);
    /// assert_eq!(
    ///     profile.order,
    ///     [
    ///         QualityTier::LossyHigh,
    ///         QualityTier::Lossless,
    ///         QualityTier::LosslessHires,
    ///         QualityTier::LossyMid,
    ///     ]
    /// );
    /// ```
    pub fn compact() -> QualityProfile {
        QualityProfile {
            name: "compact".to_owned(),
            order: vec![
                QualityTier::LossyHigh,
                QualityTier::Lossless,
                QualityTier::LosslessHires,
                QualityTier::LossyMid,
            ],
            minimum: QualityTier::LossyMid,
            cutoff: QualityTier::LossyHigh,
        }
    }

    /// Looks up a built-in profile by name.
    ///
    /// # Parameters
    ///
    /// - `name`: the profile name, matched exactly as
    ///   `acquisition.quality_profile` spells it.
    ///
    /// # Returns
    ///
    /// The profile, or `None` when no built-in carries that name.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::profile::QualityProfile;
    ///
    /// assert!(QualityProfile::builtin("lossless-preferred").is_some());
    /// assert!(QualityProfile::builtin("Compact").is_none());
    /// ```
    pub fn builtin(name: &str) -> Option<QualityProfile> {
        match name {
            "lossless-preferred" => Some(QualityProfile::lossless_preferred()),
            "compact" => Some(QualityProfile::compact()),
            _ => None,
        }
    }

    /// Returns whether a tier is acceptable to this profile.
    ///
    /// # Parameters
    ///
    /// - `tier`: the tier computed for a candidate file.
    ///
    /// # Returns
    ///
    /// `true` when the tier is at or above [`QualityProfile::minimum`] in the
    /// absolute quality order, so a tier equal to the minimum passes.
    pub fn meets_minimum(&self, tier: QualityTier) -> bool {
        quality_rank(tier) >= quality_rank(self.minimum)
    }

    /// Returns whether a tier satisfies the profile's cutoff.
    ///
    /// # Parameters
    ///
    /// - `tier`: the tier computed for a file.
    ///
    /// # Returns
    ///
    /// `true` when the tier is at or above [`QualityProfile::cutoff`] in the
    /// absolute quality order, which means an existing file needs no upgrade.
    pub fn meets_cutoff(&self, tier: QualityTier) -> bool {
        quality_rank(tier) >= quality_rank(self.cutoff)
    }

    /// Returns the position of a tier in the profile's preference order.
    ///
    /// # Parameters
    ///
    /// - `tier`: the tier to place.
    ///
    /// # Returns
    ///
    /// `0` for the most preferred tier. A tier the order does not list sorts
    /// after every listed tier, best quality first among the unlisted ones, so
    /// a profile that omits `lossy-low` still ranks it below a listed
    /// `lossy-mid`.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::profile::QualityProfile;
    /// use hearthsound_core::QualityTier;
    ///
    /// let profile = QualityProfile::lossless_preferred();
    /// assert_eq!(profile.preference(QualityTier::Lossless), 0);
    /// assert_eq!(profile.preference(QualityTier::LosslessHires), 1);
    /// assert_eq!(profile.preference(QualityTier::LossyLow), 4);
    /// ```
    pub fn preference(&self, tier: QualityTier) -> usize {
        if let Some(position) = self.order.iter().position(|listed| *listed == tier) {
            return position;
        }
        let better_unlisted = QualityTier::ALL
            .into_iter()
            .filter(|candidate| {
                !self.order.contains(candidate) && quality_rank(*candidate) > quality_rank(tier)
            })
            .count();
        self.order.len() + better_unlisted
    }
}

impl Default for QualityProfile {
    /// Returns [`QualityProfile::lossless_preferred`].
    fn default() -> QualityProfile {
        QualityProfile::lossless_preferred()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a candidate file with the given attributes.
    fn file(
        name: &str,
        format: Option<&str>,
        bitrate_kbps: Option<u32>,
        sample_rate: Option<u32>,
        bit_depth: Option<u8>,
    ) -> CandidateFile {
        CandidateFile {
            name: name.to_owned(),
            size: Some(30_000_000),
            format: format.map(str::to_owned),
            bitrate_kbps,
            sample_rate,
            bit_depth,
            duration_ms: Some(300_000),
            track_index: None,
        }
    }

    #[test]
    fn lossless_formats_get_their_tier() {
        for format in ["flac", "alac", "wav", "aiff", "wv", "FLAC", "audio/x-flac"] {
            let plain = file("track", Some(format), None, Some(44_100), Some(16));
            assert_eq!(
                quality_tier(&plain),
                Some(QualityTier::Lossless),
                "{format} at 16/44.1"
            );
        }
        let hi_res = [
            ("flac", None, Some(44_100), Some(24)),
            ("flac", None, Some(96_000), Some(16)),
            ("wav", None, Some(192_000), Some(24)),
            ("wv", None, Some(48_000), Some(32)),
        ];
        for (format, bitrate, sample_rate, bit_depth) in hi_res {
            let hi_res_file = file("track", Some(format), bitrate, sample_rate, bit_depth);
            assert_eq!(
                quality_tier(&hi_res_file),
                Some(QualityTier::LosslessHires),
                "{format} at {sample_rate:?}/{bit_depth:?}"
            );
        }
        let unknown_attributes = file("track", Some("flac"), None, None, None);
        assert_eq!(
            quality_tier(&unknown_attributes),
            Some(QualityTier::Lossless)
        );
    }

    #[test]
    fn mp3_bitrates_map_to_tiers() {
        let cases = [
            (320, QualityTier::LossyHigh),
            (256, QualityTier::LossyHigh),
            (245, QualityTier::LossyHigh),
            (244, QualityTier::LossyMid),
            (192, QualityTier::LossyMid),
            (191, QualityTier::LossyLow),
            (128, QualityTier::LossyLow),
            (64, QualityTier::LossyLow),
        ];
        for (bitrate, tier) in cases {
            let file = file("track.mp3", None, Some(bitrate), Some(44_100), None);
            assert_eq!(quality_tier(&file), Some(tier), "mp3 at {bitrate} kbps");
        }
    }

    #[test]
    fn lossy_format_thresholds_match_the_design() {
        let cases = [
            ("aac", 256, QualityTier::LossyHigh),
            ("aac", 255, QualityTier::LossyMid),
            ("m4a", 160, QualityTier::LossyMid),
            ("m4a", 159, QualityTier::LossyLow),
            ("opus", 160, QualityTier::LossyHigh),
            ("opus", 112, QualityTier::LossyMid),
            ("opus", 111, QualityTier::LossyLow),
            ("ogg", 256, QualityTier::LossyHigh),
            ("ogg", 160, QualityTier::LossyMid),
            ("ogg", 159, QualityTier::LossyLow),
        ];
        for (format, bitrate, tier) in cases {
            let file = file("track", Some(format), Some(bitrate), None, None);
            assert_eq!(quality_tier(&file), Some(tier), "{format} at {bitrate}");
        }
    }

    #[test]
    fn undecidable_attributes_have_no_tier() {
        assert_eq!(quality_tier(&file("track", None, None, None, None)), None);
        assert_eq!(
            quality_tier(&file("track.mp3", None, None, None, None)),
            None
        );
        assert_eq!(
            quality_tier(&file("track.mp3", None, Some(0), None, None)),
            None
        );
        assert_eq!(
            quality_tier(&file("track.ape", None, None, None, None)),
            None
        );
        assert_eq!(
            quality_tier(&file(
                "track",
                Some("application/octet-stream"),
                None,
                None,
                None
            )),
            None
        );
        assert_eq!(
            quality_tier(&file("track.wma", None, Some(192), None, None)),
            None
        );
    }

    #[test]
    fn the_file_name_extension_is_the_fallback() {
        let file = file(
            "Music/Album/01 - Song.flac",
            None,
            None,
            Some(96_000),
            Some(24),
        );
        assert_eq!(quality_tier(&file), Some(QualityTier::LosslessHires));
        let lossy = file_with_name("01 - Song.MP3", Some(320));
        assert_eq!(quality_tier(&lossy), Some(QualityTier::LossyHigh));
        assert_eq!(file_format(&file_with_name("01 - Song", Some(320))), None);
        assert_eq!(
            file_format(&file_with_name("01 - Song.unknownext", Some(320))),
            None
        );
    }

    /// Builds a candidate file whose only attribute is its name and bitrate.
    fn file_with_name(name: &str, bitrate_kbps: Option<u32>) -> CandidateFile {
        file(name, None, bitrate_kbps, None, None)
    }

    #[test]
    fn format_aliases_are_canonicalised() {
        assert_eq!(
            file_format(&file("x", Some("MPEG-1"), None, None, None)).as_deref(),
            Some("mp3")
        );
        assert_eq!(
            file_format(&file("x", Some("Vorbis"), None, None, None)).as_deref(),
            Some("ogg")
        );
        assert_eq!(
            file_format(&file("x", Some("audio/x-aiff"), None, None, None)).as_deref(),
            Some("aiff")
        );
        assert_eq!(
            file_format(&file("x", Some("WavPack"), None, None, None)).as_deref(),
            Some("wv")
        );
        assert_eq!(
            file_format(&file("x", Some("MP4"), None, None, None)).as_deref(),
            Some("m4a")
        );
    }

    #[test]
    fn profile_bounds_use_the_absolute_order() {
        let profile = QualityProfile::compact();
        assert!(profile.meets_minimum(QualityTier::LossyHigh));
        assert!(profile.meets_minimum(QualityTier::Lossless));
        assert!(profile.meets_minimum(QualityTier::LossyMid));
        assert!(!profile.meets_minimum(QualityTier::LossyLow));
        assert!(profile.meets_cutoff(QualityTier::LossyHigh));
        assert!(profile.meets_cutoff(QualityTier::Lossless));
        assert!(!profile.meets_cutoff(QualityTier::LossyMid));

        let default = QualityProfile::lossless_preferred();
        assert!(default.meets_cutoff(QualityTier::LosslessHires));
        assert!(!default.meets_cutoff(QualityTier::LossyHigh));
    }

    #[test]
    fn profile_order_is_the_preference() {
        let profile = QualityProfile::compact();
        assert_eq!(profile.preference(QualityTier::LossyHigh), 0);
        assert_eq!(profile.preference(QualityTier::Lossless), 1);
        assert_eq!(profile.preference(QualityTier::LosslessHires), 2);
        assert_eq!(profile.preference(QualityTier::LossyMid), 3);
        assert_eq!(profile.preference(QualityTier::LossyLow), 4);

        let default = QualityProfile::lossless_preferred();
        assert!(
            default.preference(QualityTier::Lossless)
                < default.preference(QualityTier::LosslessHires)
        );
        assert!(
            default.preference(QualityTier::LosslessHires)
                < default.preference(QualityTier::LossyHigh)
        );
    }

    #[test]
    fn builtin_profiles_round_trip() {
        assert_eq!(
            QualityProfile::BUILTIN_NAMES,
            ["lossless-preferred", "compact"]
        );
        for name in QualityProfile::BUILTIN_NAMES {
            let profile = QualityProfile::builtin(name).unwrap();
            assert_eq!(profile.name, name);
        }
        assert_eq!(
            QualityProfile::default(),
            QualityProfile::lossless_preferred()
        );
        assert!(QualityProfile::builtin("no-such-profile").is_none());
    }

    #[test]
    fn quality_ranks_are_ordered() {
        assert!(quality_rank(QualityTier::LosslessHires) > quality_rank(QualityTier::Lossless));
        assert!(quality_rank(QualityTier::Lossless) > quality_rank(QualityTier::LossyHigh));
        assert!(quality_rank(QualityTier::LossyHigh) > quality_rank(QualityTier::LossyMid));
        assert!(quality_rank(QualityTier::LossyMid) > quality_rank(QualityTier::LossyLow));
    }
}
