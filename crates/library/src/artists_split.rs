//! Splitting a single-valued artist tag into its credited artists.
//!
//! A file has two ways to credit several artists: a multi-valued tag, whose
//! values are already separate artists, and a single value that lists them, for
//! example `Nova Reyes feat. Ida Frost`. The scanner uses a multi-valued tag
//! as it stands and splits a single value on the configured separators
//! (`library.scan.artist_separators`), which the admin chooses because every
//! library spells a collaboration differently.
//!
//! A separator only splits where it stands as a word: it must have whitespace
//! on both sides, so `AC/DC` stays one artist while `A / B` becomes two. The
//! semicolon is the exception, because `A;B` is a common spelling without
//! spaces. Matching is ASCII case-insensitive; parts are trimmed and empty
//! parts are dropped.

/// Splits a single-valued artist tag into its credited artists.
///
/// # Parameters
///
/// - `value`: the tag value.
/// - `separators`: the separators to split on; an empty list never splits.
///
/// # Returns
///
/// The trimmed, non-empty parts in order. A value with nothing to split on
/// comes back as a single part, and a value that is only whitespace comes back
/// empty.
///
/// # Examples
///
/// ```
/// use hearthsound_library::artists_split::split_artists;
///
/// let separators: Vec<String> = [";", "/", "feat."].iter().map(|s| (*s).to_owned()).collect();
/// assert_eq!(split_artists("A / B", &separators), ["A", "B"]);
/// assert_eq!(split_artists("AC/DC", &separators), ["AC/DC"]);
/// assert_eq!(split_artists("A;B feat. C", &separators), ["A", "B", "C"]);
/// ```
pub fn split_artists(value: &str, separators: &[String]) -> Vec<String> {
    let mut parts = vec![value.to_owned()];
    for separator in separators {
        if separator.is_empty() {
            continue;
        }
        parts = parts
            .into_iter()
            .flat_map(|part| split_on(&part, separator))
            .collect();
    }
    parts
        .into_iter()
        .map(|part| part.trim().to_owned())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Splits one part on a single separator.
///
/// # Parameters
///
/// - `part`: the text to split.
/// - `separator`: the separator, matched ASCII case-insensitively.
///
/// # Returns
///
/// The pieces of the part, which is returned whole when the separator does not
/// stand as a word in it.
fn split_on(part: &str, separator: &str) -> Vec<String> {
    if separator == ";" {
        return part.split(';').map(str::to_owned).collect();
    }
    let mut pieces = Vec::new();
    let mut rest = part;
    while let Some(index) = find_word(rest, separator) {
        pieces.push(rest[..index].to_owned());
        rest = &rest[index + separator.len()..];
    }
    pieces.push(rest.to_owned());
    pieces
}

/// Finds a separator that stands as a word.
///
/// # Parameters
///
/// - `text`: the text to search.
/// - `separator`: the separator.
///
/// # Returns
///
/// The byte offset of a match with whitespace on both sides, or `None`.
fn find_word(text: &str, separator: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(offset) = find_ignore_ascii_case(&text[start..], separator) {
        let index = start + offset;
        let before = text[..index].chars().next_back();
        let after = text[index + separator.len()..].chars().next();
        if before.is_some_and(char::is_whitespace) && after.is_some_and(char::is_whitespace) {
            return Some(index);
        }
        start = index + separator.len();
    }
    None
}

/// Finds a substring, comparing ASCII letters case-insensitively.
///
/// # Parameters
///
/// - `text`: the text to search.
/// - `needle`: the substring to find.
///
/// # Returns
///
/// The byte offset of the first match, or `None`.
fn find_ignore_ascii_case(text: &str, needle: &str) -> Option<usize> {
    let needle_len = needle.len();
    if needle_len == 0 || needle_len > text.len() {
        return None;
    }
    let bytes = text.as_bytes();
    let wanted = needle.as_bytes();
    (0..=bytes.len() - needle_len).find(|&index| {
        bytes[index..index + needle_len].eq_ignore_ascii_case(wanted)
            && text.is_char_boundary(index)
            && text.is_char_boundary(index + needle_len)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The separators the shipped setting carries.
    fn separators() -> Vec<String> {
        [";", "/", "feat.", "ft.", "featuring"]
            .iter()
            .map(|separator| (*separator).to_owned())
            .collect()
    }

    #[test]
    fn a_slash_splits_only_between_whitespace() {
        let separators = separators();
        assert_eq!(split_artists("A / B", &separators), ["A", "B"]);
        assert_eq!(split_artists("AC/DC", &separators), ["AC/DC"]);
        assert_eq!(split_artists("A/B", &separators), ["A/B"]);
    }

    #[test]
    fn a_semicolon_splits_anywhere() {
        assert_eq!(split_artists("A;B", &separators()), ["A", "B"]);
        assert_eq!(split_artists("A ; B", &separators()), ["A", "B"]);
    }

    #[test]
    fn a_credit_word_splits_between_whitespace() {
        let separators = separators();
        assert_eq!(split_artists("A feat. B", &separators), ["A", "B"]);
        assert_eq!(split_artists("A FEAT. B", &separators), ["A", "B"]);
        assert_eq!(split_artists("A featuring B", &separators), ["A", "B"]);
        assert_eq!(split_artists("A ft. B", &separators), ["A", "B"]);
        assert_eq!(split_artists("A feat.B", &separators), ["A feat.B"]);
    }

    #[test]
    fn several_separators_split_in_one_pass() {
        assert_eq!(
            split_artists("A;B feat. C / D", &separators()),
            ["A", "B", "C", "D"]
        );
    }

    #[test]
    fn an_empty_separator_list_never_splits() {
        assert_eq!(split_artists("A;B feat. C", &[]), ["A;B feat. C"]);
        assert_eq!(split_artists("A / B", &[String::new()]), ["A / B"],);
    }

    #[test]
    fn parts_are_trimmed_and_empty_ones_dropped() {
        assert_eq!(split_artists("  A ; ; B  ", &separators()), ["A", "B"]);
        assert_eq!(split_artists("   ", &separators()), Vec::<String>::new());
    }

    #[test]
    fn a_separator_within_a_word_is_left_alone() {
        let separators = vec!["and".to_owned()];
        assert_eq!(
            split_artists("Simon and Garfunkel", &separators),
            ["Simon", "Garfunkel"]
        );
        assert_eq!(split_artists("Sand", &separators), ["Sand"]);
        assert_eq!(
            split_artists("Simon AND Garfunkel", &separators),
            ["Simon", "Garfunkel"]
        );
    }

    #[test]
    fn unicode_text_keeps_its_boundaries() {
        let separators = vec!["/".to_owned()];
        assert_eq!(
            split_artists("Caf\u{e9} / Bar", &separators),
            ["Caf\u{e9}", "Bar"]
        );
        assert_eq!(
            split_artists("Caf\u{e9}/Bar", &separators),
            ["Caf\u{e9}/Bar"]
        );
    }
}
