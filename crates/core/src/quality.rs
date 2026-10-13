//! The [`QualityTier`] bucket assigned to a media file.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The quality bucket of a media file.
///
/// A tier is a bucket of formats and bit depths, not a specific format. The
/// core computes a candidate's tier from the raw attributes the provider
/// reports (format, bitrate, bit depth, sample rate); the provider never
/// assigns a tier itself. The literals are fixed by the acquisition design and
/// are the values stored in `media_files.quality_tier`, so they must not be
/// renamed.
///
/// # Examples
///
/// ```
/// use hearthsound_core::QualityTier;
///
/// assert_eq!(QualityTier::LosslessHires.as_str(), "lossless-hires");
/// assert_eq!("lossy-low".parse::<QualityTier>().unwrap(), QualityTier::LossyLow);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QualityTier {
    /// Lossless with 24-bit depth, or a sample rate above 48 kHz.
    LosslessHires,
    /// FLAC, ALAC, WAV, AIFF or WavPack, 16-bit, sample rate at or below
    /// 48 kHz.
    Lossless,
    /// MP3 320 CBR or V0, AAC at or above 256, Opus at or above 160, Vorbis at
    /// or above 256.
    LossyHigh,
    /// MP3 at or above 192, AAC at or above 160, Opus at or above 112, Vorbis
    /// at or above 160.
    LossyMid,
    /// Anything below [`QualityTier::LossyMid`].
    LossyLow,
}

impl QualityTier {
    /// Every tier, in a fixed order.
    ///
    /// The order matches the variant declaration order (best first) and is
    /// stable across releases. It is not a sort key: a quality profile carries
    /// its own ordered preference list, in which `lossless` may precede
    /// `lossless-hires`.
    pub const ALL: [QualityTier; 5] = [
        QualityTier::LosslessHires,
        QualityTier::Lossless,
        QualityTier::LossyHigh,
        QualityTier::LossyMid,
        QualityTier::LossyLow,
    ];

    /// Returns the literal written to `media_files.quality_tier`.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::QualityTier;
    ///
    /// assert_eq!(QualityTier::LossyMid.as_str(), "lossy-mid");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            QualityTier::LosslessHires => "lossless-hires",
            QualityTier::Lossless => "lossless",
            QualityTier::LossyHigh => "lossy-high",
            QualityTier::LossyMid => "lossy-mid",
            QualityTier::LossyLow => "lossy-low",
        }
    }
}

impl fmt::Display for QualityTier {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`QualityTier`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown quality tier `{0}`")]
pub struct UnknownQualityTier(pub String);

impl FromStr for QualityTier {
    type Err = UnknownQualityTier;

    /// Parses a quality tier from its literal.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownQualityTier`] when `s` is not one of the literals
    /// produced by [`QualityTier::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|tier| tier.as_str() == s)
            .ok_or_else(|| UnknownQualityTier(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_match_the_specification() {
        assert_eq!(
            QualityTier::ALL.map(QualityTier::as_str),
            [
                "lossless-hires",
                "lossless",
                "lossy-high",
                "lossy-mid",
                "lossy-low",
            ]
        );
    }

    #[test]
    fn literals_round_trip() {
        for tier in QualityTier::ALL {
            assert_eq!(tier.as_str().parse::<QualityTier>().unwrap(), tier);
            assert_eq!(tier.to_string(), tier.as_str());
            let json = serde_json::to_string(&tier).unwrap();
            assert_eq!(json, format!("\"{tier}\""));
            assert_eq!(serde_json::from_str::<QualityTier>(&json).unwrap(), tier);
        }
    }

    #[test]
    fn rejects_unknown_literals() {
        assert_eq!(
            "lossless-hi".parse::<QualityTier>(),
            Err(UnknownQualityTier("lossless-hi".to_owned()))
        );
        assert!("LOSSLESS".parse::<QualityTier>().is_err());
        assert!(serde_json::from_str::<QualityTier>("\"hi-res\"").is_err());
    }
}
