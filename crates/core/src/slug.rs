//! The [`Slug`] provider instance identifier.

use std::fmt;

/// A provider instance identifier.
///
/// A slug is a short, URL-safe name an administrator assigns to one configured
/// provider instance, for example `slskd` or `deezer`. It is restricted to
/// 2 to 24 lowercase ASCII letters and digits so it can appear in URLs, log
/// fields and configuration keys without escaping.
///
/// # Examples
///
/// ```
/// use hearthsound_core::Slug;
///
/// let slug = Slug::new("deezer").unwrap();
/// assert_eq!(slug.as_str(), "deezer");
/// assert!(Slug::new("Deezer").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Slug(String);

impl Slug {
    /// The minimum accepted slug length in bytes.
    pub const MIN_LEN: usize = 2;
    /// The maximum accepted slug length in bytes.
    pub const MAX_LEN: usize = 24;

    /// Creates a slug from `value`.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSlug`] when `value` is shorter than [`Slug::MIN_LEN`] or
    /// longer than [`Slug::MAX_LEN`] bytes, or contains anything other than
    /// lowercase ASCII letters and digits.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::Slug;
    ///
    /// assert!(Slug::new("ab").is_ok());
    /// assert!(Slug::new("a").is_err());
    /// assert!(Slug::new("de-ezer").is_err());
    /// ```
    pub fn new(value: &str) -> Result<Slug, InvalidSlug> {
        let length_ok = (Self::MIN_LEN..=Self::MAX_LEN).contains(&value.len());
        let chars_ok = value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
        if length_ok && chars_ok {
            Ok(Slug(value.to_owned()))
        } else {
            Err(InvalidSlug(value.to_owned()))
        }
    }

    /// Returns the slug as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The error returned when a string is not a valid [`Slug`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid slug `{0}`: expected 2 to 24 lowercase ASCII letters or digits")]
pub struct InvalidSlug(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_minimum_and_maximum_lengths() {
        assert_eq!(Slug::new("ab").unwrap().as_str(), "ab");
        let long = "a".repeat(Slug::MAX_LEN);
        assert_eq!(Slug::new(&long).unwrap().as_str(), long);
    }

    #[test]
    fn rejects_bad_slugs() {
        assert!(Slug::new("a").is_err());
        assert!(Slug::new(&"a".repeat(Slug::MAX_LEN + 1)).is_err());
        assert!(Slug::new("Deezer").is_err());
        assert!(Slug::new("de-ezer").is_err());
    }

    #[test]
    fn displays_itself() {
        assert_eq!(Slug::new("hello").unwrap().to_string(), "hello");
    }
}
