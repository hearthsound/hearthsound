//! The bracket scanner shared by text normalization and path heuristics.
//!
//! Release folders and Soulseek file names carry qualifiers in brackets:
//! `(Live)`, `[FLAC 24-96]`, `(2011 Deluxe Edition)`. Both the normalizer and
//! the path parser need to walk those groups, keep or drop them, and know when
//! a bracket never closes. The scan lives here once so the two modules agree on
//! what a group is.
//!
//! An unbalanced bracket is not a group: the text is returned unchanged, so a
//! half-written file name degrades to its literal characters instead of losing
//! everything after the stray bracket.

/// Characters that open a bracketed group, paired with their closer.
const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// Returns the canonical opener matching `c`, or `None`.
pub(crate) fn opener(c: char) -> Option<char> {
    PAIRS
        .iter()
        .find(|(open, _)| *open == c)
        .map(|(open, _)| *open)
}

/// Returns the closer for an opener, or `None` when `opener` is not one.
pub(crate) fn closer_for(opener: char) -> Option<char> {
    PAIRS
        .iter()
        .find(|(open, _)| *open == opener)
        .map(|(_, close)| *close)
}

/// Returns the content of every completed top-level bracketed group.
///
/// # Parameters
///
/// - `text`: the text to scan.
///
/// # Returns
///
/// The raw text between each pair of brackets, in order. A group nested inside
/// another is part of the outer group's content, not a group of its own, and an
/// unclosed bracket contributes nothing.
pub(crate) fn groups(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut found = Vec::new();
    let mut stack: Vec<(usize, char)> = Vec::new();
    for (index, c) in chars.iter().copied().enumerate() {
        if let Some(inner) = opener(c) {
            stack.push((index, inner));
            continue;
        }
        let Some(&(start, inner)) = stack.last() else {
            continue;
        };
        if closer_for(inner) == Some(c) {
            stack.pop();
            if stack.is_empty() {
                found.push(chars[start + 1..index].iter().collect());
            }
        }
    }
    found
}

/// Returns `text` with the bracketed groups `keep` rejects replaced by a space.
///
/// # Parameters
///
/// - `text`: the text to rewrite.
/// - `keep`: decides whether a group survives; it receives the raw content
///   between the brackets.
///
/// # Returns
///
/// The rewritten text. When the brackets do not balance, `text` is returned
/// unchanged, because there is no reliable group to remove.
pub(crate) fn drop_groups(text: &str, keep: impl Fn(&str) -> bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut stack: Vec<(usize, char)> = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if let Some(inner) = opener(c) {
            stack.push((index, inner));
            index += 1;
            continue;
        }
        match stack.last().copied() {
            Some((start, inner)) if closer_for(inner) == Some(c) => {
                stack.pop();
                if stack.is_empty() {
                    let content: String = chars[start + 1..index].iter().collect();
                    if keep(&content) {
                        out.extend(chars[start..=index].iter());
                    } else {
                        out.push(' ');
                    }
                }
                index += 1;
            }
            Some(_) => index += 1,
            None => {
                out.push(c);
                index += 1;
            }
        }
    }
    if stack.is_empty() {
        out
    } else {
        text.to_owned()
    }
}

/// Returns `text` with every bracketed group replaced by a space.
pub(crate) fn remove_groups(text: &str) -> String {
    drop_groups(text, |_| false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_returns_top_level_content_in_order() {
        assert_eq!(groups("Album (2011) [FLAC]"), ["2011", "FLAC"]);
        assert_eq!(groups("Album (a (b))"), ["a (b)"]);
        assert!(groups("Album (2011").is_empty());
    }

    #[test]
    fn removes_groups_and_keeps_text() {
        assert_eq!(
            remove_groups("Album (2011) [FLAC]")
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["Album"]
        );
        assert_eq!(remove_groups("Album"), "Album");
        assert_eq!(remove_groups("Album (2011"), "Album (2011");
        assert_eq!(remove_groups("a(1)b(2)c"), "a b c");
    }

    #[test]
    fn keeps_selected_groups() {
        let kept = drop_groups("Title (Live) (Deluxe)", |content| content == "Live");
        assert_eq!(kept, "Title (Live)  ");
        assert_eq!(remove_groups("Title (Live)").trim(), "Title");
    }

    #[test]
    fn pairs_are_known() {
        assert_eq!(opener('['), Some('['));
        assert_eq!(opener('x'), None);
        assert_eq!(closer_for('{'), Some('}'));
        assert_eq!(closer_for('x'), None);
    }
}
