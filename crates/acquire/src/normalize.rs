//! Text normalization and string similarity for candidate matching.
//!
//! Both sides of every field comparison pass through [`normalize_with`] before
//! [`similarity`] computes a Jaro-Winkler value, so `Beyonce` matches
//! `Beyoncé`, `Rock'n'Roll` matches `Rock n Roll`, and a title carrying a
//! featuring credit matches the same title without one. Bracketed groups such
//! as `(Official Video)` or `(Live)` are dropped from the candidate unless the
//! target keeps the same group, which is what makes a bare Soulseek file name
//! comparable to a fully tagged catalog track.
//!
//! [`qualifiers_in`] reports the alternate-version qualifiers a raw text
//! carries, so the scorer can halve the score of a live take, a remix or a
//! nightcore edit when the target does not ask for one.
//!
//! The pipeline is fixed:
//!
//! 1. lowercase;
//! 2. NFKD compatibility decomposition, dropping combining marks, so full-width
//!    and accented characters fold onto their ASCII forms;
//! 3. drop every bracketed group the filter does not keep;
//! 4. split into alphanumeric tokens, dropping punctuation and symbols;
//! 5. cut the text at a featuring credit (`feat`, `feat.`, `featuring`, `ft`);
//! 6. join the tokens with single spaces.

use std::collections::BTreeSet;
use std::fmt;

use strsim::jaro_winkler;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::brackets;

/// An alternate-version qualifier the scorer penalizes.
///
/// A candidate whose file name or path carries one of these when the target
/// does not is an alternate take of the requested recording, so its match score
/// is halved. The literals are fixed by the acquisition design and are the
/// values compared against the target's own text.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::normalize::{Qualifier, qualifiers_in};
///
/// assert_eq!(Qualifier::SpedUp.as_str(), "sped up");
/// assert!(qualifiers_in("Song (Sped Up)").contains(&Qualifier::SpedUp));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Qualifier {
    /// A live recording.
    Live,
    /// A remix.
    Remix,
    /// A karaoke backing track.
    Karaoke,
    /// A version without the lead vocals.
    Instrumental,
    /// A cover version by another act.
    Cover,
    /// An acoustic rendition.
    Acoustic,
    /// A demo recording.
    Demo,
    /// A radio edit or another edit of the master.
    Edit,
    /// A sped-up edit.
    SpedUp,
    /// A slowed-down edit.
    Slowed,
    /// A nightcore edit.
    Nightcore,
    /// An 8D audio mix.
    EightD,
    /// A mix drowned in reverb.
    Reverb,
}

impl Qualifier {
    /// Every qualifier, in the order the acquisition design lists them.
    ///
    /// The order is stable across releases.
    pub const ALL: [Qualifier; 13] = [
        Qualifier::Live,
        Qualifier::Remix,
        Qualifier::Karaoke,
        Qualifier::Instrumental,
        Qualifier::Cover,
        Qualifier::Acoustic,
        Qualifier::Demo,
        Qualifier::Edit,
        Qualifier::SpedUp,
        Qualifier::Slowed,
        Qualifier::Nightcore,
        Qualifier::EightD,
        Qualifier::Reverb,
    ];

    /// Returns the literal a candidate text has to carry to be penalized.
    ///
    /// A multi-word literal such as `sped up` is matched as a token sequence,
    /// so `sped-up`, `sped up` and `SPED UP` all match it.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::normalize::Qualifier;
    ///
    /// assert_eq!(Qualifier::EightD.as_str(), "8d");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            Qualifier::Live => "live",
            Qualifier::Remix => "remix",
            Qualifier::Karaoke => "karaoke",
            Qualifier::Instrumental => "instrumental",
            Qualifier::Cover => "cover",
            Qualifier::Acoustic => "acoustic",
            Qualifier::Demo => "demo",
            Qualifier::Edit => "edit",
            Qualifier::SpedUp => "sped up",
            Qualifier::Slowed => "slowed",
            Qualifier::Nightcore => "nightcore",
            Qualifier::EightD => "8d",
            Qualifier::Reverb => "reverb",
        }
    }
}

impl fmt::Display for Qualifier {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The bracketed groups a normalized text keeps.
///
/// A comparison builds one filter from the target and applies it to both
/// sides: a bracketed group of the candidate is dropped unless the target
/// carries a group with the same content. The target thus keeps its own
/// `(Live)` or `(Deluxe Edition)` while a candidate that adds an unrelated
/// bracket loses it.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::normalize::{BracketFilter, normalize_with};
///
/// let filter = BracketFilter::of("Song (Live)");
/// assert_eq!(normalize_with("Song (Live at Wembley)", &filter), "song");
/// assert_eq!(normalize_with("Song (Live)", &filter), "song live");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BracketFilter {
    groups: BTreeSet<String>,
}

impl BracketFilter {
    /// Returns a filter that keeps no bracketed group.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::normalize::{BracketFilter, normalize_with};
    ///
    /// let filter = BracketFilter::empty();
    /// assert!(filter.is_empty());
    /// assert_eq!(normalize_with("Song (Live)", &filter), "song");
    /// ```
    pub fn empty() -> BracketFilter {
        BracketFilter {
            groups: BTreeSet::new(),
        }
    }

    /// Collects the bracketed groups of `target`.
    ///
    /// An unclosed bracket is not a group and contributes nothing.
    ///
    /// # Parameters
    ///
    /// - `target`: the text of the field the candidate is compared against.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::normalize::BracketFilter;
    ///
    /// let filter = BracketFilter::of("Song (Live) [Remastered]");
    /// assert!(filter.keeps("Live"));
    /// assert!(filter.keeps("remastered"));
    /// assert!(!filter.keeps("Demo"));
    /// ```
    pub fn of(target: &str) -> BracketFilter {
        let folded = case_fold(target);
        let mut groups = BTreeSet::new();
        for group in brackets::groups(&folded) {
            groups.insert(tokens_of(&group).join(" "));
        }
        BracketFilter { groups }
    }

    /// Returns whether a bracketed group is kept.
    ///
    /// # Parameters
    ///
    /// - `content`: the raw text between the brackets. It is folded before the
    ///   lookup, so case, accents and punctuation do not matter.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_acquire::normalize::BracketFilter;
    ///
    /// let filter = BracketFilter::of("Song (Deluxe Edition)");
    /// assert!(filter.keeps("deluxe  edition"));
    /// assert!(!filter.keeps("Live"));
    /// ```
    pub fn keeps(&self, content: &str) -> bool {
        self.groups
            .contains(&tokens_of(&case_fold(content)).join(" "))
    }

    /// Returns whether the filter keeps no group at all.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// Returns the normalized form of `text`, dropping every bracketed group.
///
/// This is the target-agnostic form of [`normalize_with`]; use [`similarity`]
/// or [`normalize_with`] when a target exists, because they keep the target's
/// own bracketed qualifiers on both sides.
///
/// # Parameters
///
/// - `text`: the raw text to normalize.
///
/// # Returns
///
/// Lowercased, accent-folded, punctuation-free text with single spaces between
/// the remaining tokens; an empty string when nothing is left.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::normalize::normalize;
///
/// assert_eq!(normalize("Beyonce - DONT Start (2008 Remaster)"), "beyonce dont start");
/// ```
pub fn normalize(text: &str) -> String {
    normalize_with(text, &BracketFilter::empty())
}

/// Returns the normalized form of `text`, keeping the groups `filter` allows.
///
/// # Parameters
///
/// - `text`: the raw text to normalize.
/// - `filter`: the bracketed groups to keep; usually
///   [`BracketFilter::of`] of the target.
///
/// # Returns
///
/// The folded, tokenized text, or an empty string when nothing survives.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::normalize::{BracketFilter, normalize_with};
///
/// let filter = BracketFilter::of("Title (Acoustic)");
/// assert_eq!(normalize_with("Title (Acoustic) feat. Someone", &filter), "title acoustic");
/// assert_eq!(normalize_with("Title (Live)", &filter), "title");
/// ```
pub fn normalize_with(text: &str, filter: &BracketFilter) -> String {
    let folded = case_fold(text);
    let without_groups = drop_brackets(&folded, filter);
    let tokens = tokens_of(&without_groups);
    strip_featuring(&tokens).join(" ")
}

/// Returns the Jaro-Winkler similarity of two field values.
///
/// Both values are normalized with `target` as the bracketed-group reference,
/// so the target keeps its own qualifiers and the candidate keeps only the ones
/// the target shares. The comparison is asymmetric on purpose: the target
/// decides which brackets matter.
///
/// # Parameters
///
/// - `value`: the candidate's field, for example its derived title.
/// - `target`: the requested field, for example the catalog track title.
///
/// # Returns
///
/// A value in `0.0..=1.0`, where `1.0` means the normalized texts are equal.
/// Two empty texts compare equal, as do two texts that normalize to the same
/// string.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::normalize::similarity;
///
/// assert_eq!(similarity("Mojo Pin", "mojo pin"), 1.0);
/// assert!(similarity("Mojo Pin (Live)", "Mojo Pin") == 1.0);
/// assert!(similarity("Mojo Pin", "Grace") < 0.6);
/// ```
pub fn similarity(value: &str, target: &str) -> f64 {
    let filter = BracketFilter::of(target);
    jaro_winkler(
        &normalize_with(value, &filter),
        &normalize_with(target, &filter),
    )
}

/// Returns every alternate-version qualifier `text` carries.
///
/// The scan runs on the folded token sequence of the whole text, so brackets,
/// hyphens and case do not hide a qualifier: `(Live)`, `- live` and `LIVE` all
/// report [`Qualifier::Live`]. A token has to match in full, so `olive` and
/// `delivery` do not.
///
/// # Parameters
///
/// - `text`: a raw file name, title or path.
///
/// # Returns
///
/// The qualifiers found, in the [`Qualifier::ALL`] order. Empty when the text
/// carries none.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::normalize::{Qualifier, qualifiers_in};
///
/// let found = qualifiers_in("Artist - Title [Sped Up + Reverb]");
/// assert!(found.contains(&Qualifier::SpedUp));
/// assert!(found.contains(&Qualifier::Reverb));
/// assert!(!found.contains(&Qualifier::Live));
/// ```
pub fn qualifiers_in(text: &str) -> BTreeSet<Qualifier> {
    let tokens = tokens_of(&case_fold(text));
    Qualifier::ALL
        .into_iter()
        .filter(|qualifier| contains_phrase(&tokens, qualifier.as_str()))
        .collect()
}

/// Returns whether the token sequence contains `phrase` as whole tokens.
fn contains_phrase(tokens: &[String], phrase: &str) -> bool {
    let wanted: Vec<&str> = phrase.split(' ').collect();
    if wanted.is_empty() || wanted.len() > tokens.len() {
        return false;
    }
    tokens.windows(wanted.len()).any(|window| {
        window
            .iter()
            .zip(&wanted)
            .all(|(token, part)| token == part)
    })
}

/// Returns `text` lowercased, NFKD decomposed with combining marks dropped.
///
/// Punctuation and brackets survive this step; [`tokens_of`] is what removes
/// them.
fn case_fold(text: &str) -> String {
    text.nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Splits `text` into alphanumeric tokens, dropping every other character.
fn tokens_of(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Returns the tokens a normalized text keeps: everything before a featuring
/// credit.
///
/// `feat`, `feats`, `featuring` and `ft` all start a credit, and the credit
/// runs to the end of the text, which is where backends put it.
fn strip_featuring(tokens: &[String]) -> &[String] {
    let cut = tokens
        .iter()
        .position(|token| matches!(token.as_str(), "feat" | "feats" | "featuring" | "ft"));
    match cut {
        Some(index) => &tokens[..index],
        None => tokens,
    }
}

/// Returns `text` with every bracketed group the filter does not keep replaced
/// by a space.
///
/// An unbalanced bracket makes the text unusable as a group list, so the text
/// is returned unchanged and the brackets are dropped later as punctuation.
fn drop_brackets(text: &str, filter: &BracketFilter) -> String {
    brackets::drop_groups(text, |content| filter.keeps(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_accents_and_width() {
        assert_eq!(normalize("Beyonce"), normalize("BEYONCE"));
        assert_eq!(normalize("Beyoncé"), "beyonce");
        assert_eq!(normalize("BJORK"), normalize("Björk"));
        assert_eq!(normalize("ＭＪ"), "mj");
        assert_eq!(normalize("Sigur Rós - Hoppípolla"), "sigur ros hoppipolla");
    }

    #[test]
    fn punctuation_becomes_separators() {
        assert_eq!(normalize("Rock'n'Roll"), "rock n roll");
        assert_eq!(normalize("Rock n Roll"), "rock n roll");
        assert_eq!(normalize("AC/DC"), normalize("AC DC"));
        assert_eq!(normalize("  spaced   out  "), "spaced out");
        assert_eq!(normalize("!!!"), "");
    }

    #[test]
    fn strips_featuring_credits() {
        assert_eq!(normalize("Title (feat. Someone)"), "title");
        assert_eq!(normalize("Title feat Someone"), "title");
        assert_eq!(normalize("Title FEATURING Someone"), "title");
        assert_eq!(normalize("Title ft. Someone"), "title");
        assert_eq!(normalize("Title"), "title");
    }

    #[test]
    fn drops_brackets_the_target_does_not_carry() {
        let filter = BracketFilter::of("Mojo Pin");
        assert_eq!(
            normalize_with("Mojo Pin (Official Video)", &filter),
            "mojo pin"
        );
        assert_eq!(normalize_with("Mojo Pin [FLAC 24-96]", &filter), "mojo pin");
        assert_eq!(normalize_with("Mojo Pin(Live)", &filter), "mojo pin");
    }

    #[test]
    fn keeps_brackets_the_target_carries() {
        let filter = BracketFilter::of("Mojo Pin (Live)");
        assert!(filter.keeps("Live"));
        assert!(!filter.keeps("Acoustic"));
        assert_eq!(normalize_with("Mojo Pin (Live)", &filter), "mojo pin live");
        assert_eq!(
            normalize_with("Mojo Pin (Live at Wembley)", &filter),
            "mojo pin"
        );
        assert_eq!(
            normalize_with("Mojo Pin (Live) (Deluxe)", &filter),
            "mojo pin live"
        );
    }

    #[test]
    fn unbalanced_brackets_are_text() {
        let filter = BracketFilter::of("Song (Live");
        assert!(filter.is_empty());
        assert_eq!(normalize_with("Song (Live", &filter), "song live");
        assert_eq!(normalize("Song ) Live"), "song live");
    }

    #[test]
    fn similarity_ignores_case_accents_and_brackets() {
        assert_eq!(similarity("mojo pin", "Mojo Pin"), 1.0);
        assert_eq!(similarity("Hoppípolla", "Hoppipolla"), 1.0);
        assert_eq!(similarity("Mojo Pin (Live)", "Mojo Pin"), 1.0);
        assert_eq!(similarity("", ""), 1.0);
        assert!(similarity("Mojo Pin", "Grace") < 0.6);
    }

    #[test]
    fn similarity_keeps_only_the_targets_brackets() {
        // A candidate that adds a qualifier loses it; a target that asks for
        // one keeps it, so a studio take scores below a live take.
        assert_eq!(similarity("Mojo Pin", "Mojo Pin"), 1.0);
        assert_eq!(similarity("Mojo Pin (Live at Wembley)", "Mojo Pin"), 1.0);
        let live_request = similarity("Mojo Pin", "Mojo Pin (Live)");
        assert!(
            live_request > 0.8 && live_request < 1.0,
            "got {live_request}"
        );
        let far = similarity("Last Goodbye", "Mojo Pin");
        assert!(far < 0.7, "expected a low score, got {far}");
    }

    #[test]
    fn qualifiers_match_whole_tokens_only() {
        assert!(qualifiers_in("Song (Live)").contains(&Qualifier::Live));
        assert!(qualifiers_in("Song - LIVE").contains(&Qualifier::Live));
        assert!(qualifiers_in("Song [Live at Wembley]").contains(&Qualifier::Live));
        assert!(qualifiers_in("Song (Deluxe Edition)").is_empty());
        assert!(qualifiers_in("Olive Branch").is_empty());
        assert!(qualifiers_in("Delivery Man").is_empty());
        assert!(qualifiers_in("Song (8D Audio)").contains(&Qualifier::EightD));
        assert!(qualifiers_in("Song (8d)").contains(&Qualifier::EightD));
    }

    #[test]
    fn qualifiers_find_phrases_and_edits() {
        let found = qualifiers_in("Title (Sped Up)");
        assert!(found.contains(&Qualifier::SpedUp));
        let found = qualifiers_in("Title (sped-up version)");
        assert!(found.contains(&Qualifier::SpedUp));
        let found = qualifiers_in("Title (Radio Edit)");
        assert!(found.contains(&Qualifier::Edit));
        let found = qualifiers_in("Title (Slowed + Reverb)");
        assert!(found.contains(&Qualifier::Slowed));
        assert!(found.contains(&Qualifier::Reverb));
        let found = qualifiers_in("Title (Nightcore)");
        assert!(found.contains(&Qualifier::Nightcore));
        let found = qualifiers_in("Title (Instrumental)");
        assert!(found.contains(&Qualifier::Instrumental));
    }

    #[test]
    fn qualifier_literals_are_fixed() {
        assert_eq!(
            Qualifier::ALL.map(Qualifier::as_str),
            [
                "live",
                "remix",
                "karaoke",
                "instrumental",
                "cover",
                "acoustic",
                "demo",
                "edit",
                "sped up",
                "slowed",
                "nightcore",
                "8d",
                "reverb",
            ]
        );
        assert_eq!(Qualifier::Live.to_string(), "live");
    }
}
