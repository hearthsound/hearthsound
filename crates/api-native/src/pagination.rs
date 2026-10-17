//! Cursor pagination.
//!
//! A paginated list takes `?limit=<n>&cursor=<opaque>` and answers with
//! `nextCursor` while more rows follow. The cursor is opaque to clients: it is
//! the base64url (no padding) encoding of the last row's sort key and its
//! client id, joined by a newline, so the next page starts strictly after that
//! row without an `OFFSET` scan and stays stable while rows are added.

use serde::Deserialize;
use utoipa::IntoParams;

use crate::problem::Problem;

/// The page size when a request names none.
pub const DEFAULT_LIMIT: usize = 50;

/// The largest page a request may ask for.
pub const MAX_LIMIT: usize = 200;

/// The pagination parameters of a list request.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageParams {
    /// How many rows to answer, 1 to 200; 50 when absent.
    pub limit: Option<usize>,
    /// The `nextCursor` of the previous page.
    pub cursor: Option<String>,
}

impl PageParams {
    /// Returns the page size, clamped to 1..=[`MAX_LIMIT`].
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }

    /// Decodes the cursor, when there is one.
    ///
    /// # Returns
    ///
    /// The position the page starts after.
    ///
    /// # Errors
    ///
    /// Answers an `invalid-request` problem for a cursor this server did not
    /// issue.
    pub fn after(&self) -> Result<Option<Cursor>, Problem> {
        self.cursor
            .as_deref()
            .filter(|cursor| !cursor.is_empty())
            .map(Cursor::decode)
            .transpose()
    }
}

/// The position of the last row of a page.
///
/// # Examples
///
/// ```
/// use hearthsound_api_native::pagination::Cursor;
///
/// let cursor = Cursor::new("alice", "us_01j9z6q2v4r7w8x1y2z3a4b5c6");
/// let decoded = Cursor::decode(&cursor.encode()).unwrap();
/// assert_eq!(decoded, cursor);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// The sort key of the last row.
    pub key: String,
    /// The client id of the last row, which breaks ties between equal keys.
    pub id: String,
}

impl Cursor {
    /// Builds a cursor.
    ///
    /// # Parameters
    ///
    /// - `key`: the sort key of the last row.
    /// - `id`: the client id of the last row.
    pub fn new(key: impl Into<String>, id: impl Into<String>) -> Cursor {
        Cursor {
            key: key.into(),
            id: id.into(),
        }
    }

    /// Encodes the cursor for `nextCursor`.
    pub fn encode(&self) -> String {
        data_encoding::BASE64URL_NOPAD.encode(format!("{}\n{}", self.key, self.id).as_bytes())
    }

    /// Decodes a cursor a client sent back.
    ///
    /// # Errors
    ///
    /// Answers an `invalid-request` problem when the text is not a cursor.
    pub fn decode(text: &str) -> Result<Cursor, Problem> {
        let invalid = || Problem::invalid_request("The cursor is not one this server issued.");
        let bytes = data_encoding::BASE64URL_NOPAD
            .decode(text.as_bytes())
            .map_err(|_| invalid())?;
        let decoded = String::from_utf8(bytes).map_err(|_| invalid())?;
        let (key, id) = decoded.rsplit_once('\n').ok_or_else(invalid)?;
        Ok(Cursor::new(key, id))
    }
}

/// Cuts one page out of rows that are already in order.
///
/// # Parameters
///
/// - `rows`: every row, in the list's order.
/// - `params`: the request's pagination parameters.
/// - `position`: the sort key and client id of a row.
///
/// # Returns
///
/// The page and the cursor of the next one, which is `None` on the last page.
///
/// # Errors
///
/// Answers an `invalid-request` problem for a malformed cursor.
pub fn page<T>(
    rows: Vec<T>,
    params: &PageParams,
    position: impl Fn(&T) -> Cursor,
) -> Result<(Vec<T>, Option<String>), Problem> {
    let after = params.after()?;
    let limit = params.limit();
    let start = match &after {
        Some(after) => rows
            .iter()
            .position(|row| {
                let here = position(row);
                (here.key.as_str(), here.id.as_str()) > (after.key.as_str(), after.id.as_str())
            })
            .unwrap_or(rows.len()),
        None => 0,
    };
    let mut rest: Vec<T> = rows.into_iter().skip(start).collect();
    let next = if rest.len() > limit {
        rest.truncate(limit);
        rest.last().map(|row| position(row).encode())
    } else {
        None
    };
    Ok((rest, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(limit: usize, cursor: Option<String>) -> PageParams {
        PageParams {
            limit: Some(limit),
            cursor,
        }
    }

    #[test]
    fn pages_follow_each_other_without_gaps_or_repeats() {
        let rows: Vec<(String, String)> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|key| ((*key).to_owned(), format!("us_{key}")))
            .collect();
        let position = |row: &(String, String)| Cursor::new(row.0.clone(), row.1.clone());

        let (first, next) = page(rows.clone(), &params(2, None), position).unwrap();
        assert_eq!(first.len(), 2);
        let (second, next) = page(rows.clone(), &params(2, next), position).unwrap();
        assert_eq!(second[0].0, "c");
        let (third, next) = page(rows, &params(2, next), position).unwrap();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].0, "e");
        assert!(next.is_none(), "the last page has no next cursor");
    }

    #[test]
    fn a_foreign_cursor_is_an_invalid_request() {
        let problem = Cursor::decode("not a cursor!").unwrap_err();
        assert_eq!(problem.code, "invalid-request");
    }

    #[test]
    fn the_limit_is_clamped() {
        assert_eq!(params(0, None).limit(), 1);
        assert_eq!(params(10_000, None).limit(), MAX_LIMIT);
        assert_eq!(PageParams::default().limit(), DEFAULT_LIMIT);
    }
}
