//! The kind of a job.
//!
//! A job's kind is the name a handler is registered under. Kinds are strings
//! because the set of jobs is open: a crate that owns background work picks its
//! own names (the library scanner enqueues `library.scan`, the acquisition
//! orchestrator `acquisition.run`, and so on) and the composition root is the
//! only place that knows which kinds have a handler. A job whose kind has no
//! registered handler fails without being retried, so a typo surfaces in the
//! `failed` rows instead of silently disappearing.

use std::borrow::Borrow;
use std::fmt;

/// The kind of a job, matched against a registered handler.
///
/// The literal is stored in `jobs.kind` and `schedules.kind` and is the key a
/// [`JobHandler`](crate::JobHandler) is registered under, so it must not be
/// changed after jobs of that kind exist in the database.
///
/// # Examples
///
/// ```
/// use hearthsound_jobs::JobKind;
///
/// let kind = JobKind::new("library.scan");
/// assert_eq!(kind.as_str(), "library.scan");
/// assert_eq!(kind.to_string(), "library.scan");
/// assert_eq!(JobKind::from(String::from("acquisition.run")).as_str(), "acquisition.run");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobKind(String);

impl JobKind {
    /// Wraps `kind` as a job kind.
    ///
    /// The kind is not validated here: a job whose kind has no registered
    /// handler is failed by the runner without a retry, which is where an
    /// unknown kind is rejected.
    ///
    /// # Parameters
    ///
    /// - `kind`: the kind literal, for example `library.scan`.
    ///
    /// # Returns
    ///
    /// The job kind.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_jobs::JobKind;
    ///
    /// assert_eq!(JobKind::new("scrobble.deliver").as_str(), "scrobble.deliver");
    /// ```
    pub fn new(kind: impl Into<String>) -> JobKind {
        JobKind(kind.into())
    }

    /// Returns the kind literal.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_jobs::JobKind;
    ///
    /// assert_eq!(JobKind::new("plugins.sync").as_str(), "plugins.sync");
    /// ```
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for JobKind {
    /// Wraps a string literal.
    fn from(kind: &str) -> JobKind {
        JobKind(kind.to_owned())
    }
}

impl From<String> for JobKind {
    /// Wraps an owned string.
    fn from(kind: String) -> JobKind {
        JobKind(kind)
    }
}

impl AsRef<str> for JobKind {
    /// Returns the kind literal.
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for JobKind {
    /// Borrows the kind literal so a map keyed by [`JobKind`] can be looked up
    /// by `&str`.
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobKind {
    /// Writes the kind literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
