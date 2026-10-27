//! Text folding for identity comparisons.
//!
//! Two spellings of one title, one artist name or one album must compare equal
//! when the differences are punctuation, case, accents or an edition suffix. The
//! functions here are the only place that folding happens, so the resolver, the
//! discography grouping and the search deduplication all agree on when two texts
//! name the same thing.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// How far a catalog duration may differ from a local one and still count as the
/// same recording.
///
/// Three seconds, in milliseconds.
pub const DURATION_TOLERANCE_MS: i64 = 3_000;

/// The words that name an edition of a release rather than a title.
///
/// A scanned album and the provider's album of it often differ by the edition
/// in a bracket or after a dash: "Abbey Road" against "Abbey Road (Remastered
/// 2009)". A bracket or a trailing segment that contains one of these words is
/// edition information, and [`base_title`] drops it to compare the titles.
///
/// The list is deliberately short: a word that appears in real titles would
/// fold two different releases together. "Live", "Acoustic" and "Version"
/// therefore stay out, because "Live at Leeds" and "Live at Wembley" are two
/// albums.
pub const EDITION_WORDS: [&str; 8] = [
    "remaster",
    "deluxe",
    "edition",
    "anniversary",
    "mono",
    "stereo",
    "expanded",
    "bonus",
];

/// Folds a title down to the part that names the work rather than its edition.
///
/// The edition suffixes a provider and a scanner disagree about are dropped
/// before the text is folded: a trailing bracket group whose content contains
/// one of [`EDITION_WORDS`], and a trailing ` - ` segment whose remainder does
/// so. The rest of the folded text is returned, normalized by [`normalize`].
///
/// # Parameters
///
/// - `text`: the display title.
///
/// # Returns
///
/// The normalized title without its edition suffixes, which is what
/// [`normalize`] returns when the title carries none.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::normalize::base_title;
///
/// assert_eq!(base_title("Abbey Road (Remastered 2009)"), "abbey road");
/// assert_eq!(base_title("Come Together - Remastered 2009"), "come together");
/// assert_eq!(base_title("Song (Live)"), "song live");
/// ```
pub fn base_title(text: &str) -> String {
    let mut current = text.to_owned();
    loop {
        let trimmed = current.trim_end().to_owned();
        let mut shortened: Option<String> = None;
        if let Some(last) = trimmed.chars().last() {
            let opener = match last {
                ')' => Some('('),
                ']' => Some('['),
                _ => None,
            };
            if let Some(open) = opener
                && let Some(position) = trimmed.rfind(open)
            {
                let content = &trimmed[position + open.len_utf8()..trimmed.len() - last.len_utf8()];
                if carries_edition_word(content) {
                    shortened = Some(trimmed[..position].to_owned());
                }
            }
        }
        if shortened.is_none()
            && let Some(position) = trimmed.rfind(" - ")
            && carries_edition_word(&trimmed[position + 3..])
        {
            shortened = Some(trimmed[..position].to_owned());
        }
        match shortened {
            Some(candidate) => current = candidate,
            None => return normalize(&trimmed),
        }
    }
}

/// Reports whether a piece of text names an edition.
///
/// # Parameters
///
/// - `text`: the bracket content or the trailing segment of a title.
///
/// # Returns
///
/// `true` when the lowercased text contains one of [`EDITION_WORDS`].
fn carries_edition_word(text: &str) -> bool {
    let folded = text.to_lowercase();
    EDITION_WORDS.iter().any(|word| folded.contains(word))
}

/// Folds a title or an artist name into the form deduplication compares.
///
/// The pipeline is: lowercase, NFKD compatibility decomposition with combining
/// marks dropped, every character that is not alphanumeric turned into a space,
/// and runs of spaces collapsed. `Beyonce` therefore matches an accented
/// `Beyonce`, and `Rock'n'Roll` matches `Rock n Roll`.
///
/// # Parameters
///
/// - `text`: the display text.
///
/// # Returns
///
/// The folded text, which is empty when the text holds no alphanumeric
/// character.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::normalize::normalize;
///
/// assert_eq!(normalize("Rock'n'Roll"), "rock n roll");
/// assert_eq!(normalize("  Caf\u{e9}   del Mar "), "cafe del mar");
/// ```
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for folded in text.nfkd().flat_map(char::to_lowercase) {
        if is_combining_mark(folded) {
            continue;
        }
        if folded.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(folded);
        } else {
            pending_space = true;
        }
    }
    out
}

/// Folds an ISRC into the form deduplication compares.
///
/// # Parameters
///
/// - `isrc`: the ISRC a provider or a row carries, or `None`.
///
/// # Returns
///
/// The trimmed, uppercased ISRC, or `None` when there was none or it was blank.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::normalize::normalize_isrc;
///
/// assert_eq!(normalize_isrc(Some(" usrc17607839 ")).as_deref(), Some("USRC17607839"));
/// assert_eq!(normalize_isrc(Some("   ")), None);
/// ```
pub fn normalize_isrc(isrc: Option<&str>) -> Option<String> {
    let isrc = isrc?.trim();
    if isrc.is_empty() {
        return None;
    }
    Some(isrc.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_folds_case_accents_and_punctuation() {
        assert_eq!(normalize("Cafe del Mar"), "cafe del mar");
        assert_eq!(normalize("  Caf\u{e9}   Del   MAR  "), "cafe del mar");
        assert_eq!(normalize("Rock'n'Roll"), "rock n roll");
        assert_eq!(normalize("AC/DC"), "ac dc");
        assert_eq!(normalize("!!!"), "");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn isrc_normalization_trims_case_and_blanks() {
        assert_eq!(
            normalize_isrc(Some(" usrc17607839 ")).as_deref(),
            Some("USRC17607839")
        );
        assert_eq!(normalize_isrc(Some("   ")), None);
        assert_eq!(normalize_isrc(None), None);
    }

    #[test]
    fn base_titles_drop_edition_suffixes() {
        assert_eq!(base_title("Abbey Road (Remastered 2009)"), "abbey road");
        assert_eq!(
            base_title("Come Together - Remastered 2009"),
            "come together"
        );
        assert_eq!(base_title("Song (Live)"), "song live");
        assert_eq!(base_title("Song (Deluxe Edition)"), "song");
        assert_eq!(base_title("Song (Live) (Deluxe)"), "song live");
    }
}
