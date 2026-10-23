//! The relevance score a search result carries.
//!
//! The local full-text index answers a search in its own order, and a catalog
//! provider answers in the provider's order. A client that wants to highlight
//! the entry closest to what the listener typed - the Top Result card, for
//! example - needs one number that compares an artist, an album and a song. The
//! score here is that number: it is derived from the normalized query, the
//! entity's name and, for an album or a song, its artist.
//!
//! The score is ordered so the branches never overlap:
//!
//! 1. an empty query scores 0, because there is nothing to be relevant to;
//! 2. an exact normalized name scores 1;
//! 3. the query spelling out the artist and the name in either order scores
//!    0.98, which is what "Radiohead Creep" means when the result is named
//!    "Creep" by "Radiohead";
//! 4. a name that starts with the whole query followed by a word boundary
//!    scores 0.9;
//! 5. everything else is `0.6 * coverage + 0.3 * jaro-winkler`, capped at 0.89,
//!    where `coverage` is the share of the query's words that appear among the
//!    name's and artist's words (the last query word also counts when it is a
//!    prefix of one of them).
//!
//! The branch constants leave room above a partial match, so a client can treat
//! "at least 0.9" as a confident answer without knowing the formula.
//!
//! # Examples
//!
//! ```
//! use hearthsound_catalog::relevance::relevance;
//!
//! assert_eq!(relevance("Cafe del Mar", "Café del Mar", None), 1.0);
//! assert_eq!(relevance("Radiohead Creep", "Creep", Some("Radiohead")), 0.98);
//! assert_eq!(relevance("cardinals", "Cardinals Live", None), 0.9);
//! assert!(relevance("rolling stones", "Rocks Off", Some("The Rolling Stones")) < 0.89);
//! ```

use crate::dedupe::normalize;

/// Scores how well a result answers a query.
///
/// # Parameters
///
/// - `query`: the text the listener typed.
/// - `name`: the entity's name: a song title, an album title or an artist
///   name.
/// - `artist`: the primary artist of a song or the album artist of an album;
///   `None` for an artist result.
///
/// # Returns
///
/// A score from 0 to 1, rounded to three decimal places, where 1 is an exact
/// match and 0 is no match at all. The value only orders results against one
/// query: it is not comparable across queries.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::relevance::relevance;
///
/// assert_eq!(relevance("", "Anything", None), 0.0);
/// assert_eq!(relevance("Creep", "Creep", Some("Radiohead")), 1.0);
/// let partial = relevance("cree", "Creep", Some("Radiohead"));
/// assert!(partial < 0.89 && partial > 0.0);
/// ```
pub fn relevance(query: &str, name: &str, artist: Option<&str>) -> f64 {
    let query = normalize(query);
    if query.is_empty() {
        return 0.0;
    }
    let name = normalize(name);
    if name == query {
        return 1.0;
    }
    let artist = artist.map(normalize).unwrap_or_default();
    if !artist.is_empty() {
        let artist_first = format!("{artist} {name}");
        let name_first = format!("{name} {artist}");
        if query == artist_first || query == name_first {
            return 0.98;
        }
    }
    if name.starts_with(&format!("{query} ")) {
        return 0.9;
    }
    let mut words: Vec<&str> = name.split(' ').filter(|word| !word.is_empty()).collect();
    words.extend(artist.split(' ').filter(|word| !word.is_empty()));
    let query_words: Vec<&str> = query.split(' ').filter(|word| !word.is_empty()).collect();
    let last = query_words.len() - 1;
    let matched = query_words
        .iter()
        .enumerate()
        .filter(|(index, word)| {
            words.contains(word)
                || (*index == last && words.iter().any(|candidate| candidate.starts_with(*word)))
        })
        .count();
    let coverage = matched as f64 / query_words.len() as f64;
    let similarity = strsim::jaro_winkler(&name, &query);
    round_three((0.6 * coverage + 0.3 * similarity).min(0.89))
}

/// Rounds a score to the three decimals a client is promised.
///
/// # Parameters
///
/// - `value`: the score to round.
///
/// # Returns
///
/// The nearest number with at most three decimal places.
fn round_three(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A normalized exact name is the highest score and folds case and accents.
    #[test]
    fn an_exact_name_scores_one() {
        assert_eq!(relevance("Cafe del Mar", "Café del Mar", None), 1.0);
        assert_eq!(relevance("  CAFE   DEL mar ", "cafe del mar", None), 1.0);
        assert_eq!(
            relevance("Rock'n'Roll", "Rock n Roll", Some("Someone")),
            1.0,
            "the name decides before the artist does"
        );
    }

    /// A query that spells out the artist and the name in either order scores
    /// just below an exact name match.
    #[test]
    fn artist_and_name_in_either_order_scores_098() {
        assert_eq!(
            relevance("Radiohead Creep", "Creep", Some("Radiohead")),
            0.98
        );
        assert_eq!(
            relevance("Creep Radiohead", "Creep", Some("Radiohead")),
            0.98
        );
        assert_eq!(
            relevance("radiohead creep", "Creep", Some("Radiohead")),
            0.98,
            "case and spacing fold before the artist form is compared"
        );
    }

    /// A name that continues the query after a word boundary scores 0.9.
    #[test]
    fn a_name_that_starts_with_the_query_scores_09() {
        assert_eq!(relevance("cardinals", "Cardinals Live", None), 0.9);
        assert_eq!(relevance("the", "The Cardinals", None), 0.9);
        assert!(
            relevance("the cardinals band", "The Cardinals", None) < 0.89,
            "the query names more words than the result: {}",
            relevance("the cardinals band", "The Cardinals", None)
        );
    }

    /// A partial match stays below the confident branch and above zero.
    #[test]
    fn a_partial_match_stays_below_089() {
        let score = relevance("rolling stones", "Rocks Off", Some("The Rolling Stones"));
        assert!(score < 0.89, "{score}");
        assert!(score > 0.0, "{score}");
        assert_eq!(
            score,
            (score * 1000.0).round() / 1000.0,
            "the score is rounded to three decimals"
        );
    }

    /// The last query word counts when it is the beginning of a result word.
    #[test]
    fn a_prefix_of_the_last_word_counts_towards_coverage() {
        let completed = relevance("rolling ston", "Rocks Off", Some("The Rolling Stones"));
        let unmatched = relevance("rolling xyz", "Rocks Off", Some("The Rolling Stones"));
        assert!(completed > unmatched, "{completed} should beat {unmatched}");
    }

    /// An empty or punctuation-only query has nothing to be relevant to.
    #[test]
    fn an_empty_query_scores_zero() {
        assert_eq!(relevance("", "Anything", Some("Anyone")), 0.0);
        assert_eq!(relevance("   ", "Anything", None), 0.0);
        assert_eq!(relevance("!!!", "Anything", None), 0.0);
    }

    /// Every score stays inside the promised range.
    #[test]
    fn every_score_is_between_zero_and_one() {
        let queries = ["a", "ab", "the", "rolling stones", "creep radiohead", ""];
        let names = ["a", "Creep", "The Rolling Stones", "Rocks Off", "zzz"];
        for query in queries {
            for name in names {
                let score = relevance(query, name, Some("Radiohead"));
                assert!((0.0..=1.0).contains(&score), "{query} vs {name}: {score}");
            }
        }
    }
}
