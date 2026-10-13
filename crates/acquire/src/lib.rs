//! Candidate matching for on-demand acquisition.
//!
//! This crate holds the decision logic of the acquisition pipeline and the
//! pipeline's own service. On one side sit the pure rules, which take the wanted
//! track and the candidates a provider reported and say which candidate to
//! download and in what order; they touch no database, no plugin and no file
//! system, so every rule is testable with plain values:
//!
//! - [`normalize`] folds case, accents and punctuation and drops the bracketed
//!   qualifiers and featuring credits that stand between a Soulseek file name
//!   and a tagged catalog track;
//! - [`path`] recovers the title, artist, album and track number a file name
//!   and its parent directories imply;
//! - [`score`] weighs the five field similarities into the match score `m`,
//!   rejects candidates whose duration is far off, and halves the score of a
//!   live take, a remix or another alternate version the target did not ask
//!   for;
//! - [`profile`] assigns a quality tier to a file from its raw attributes and
//!   holds the named quality profiles that say which tiers are acceptable;
//! - [`select`] estimates each candidate's ETA and produces the ordered list of
//!   candidates one acquisition should try.
//!
//! On the other side sits [`orchestrator`], the service the rest of the server
//! calls: it owns the `acquisitions` and `acquisition_log` tables (through
//! [`repo`]), decides which requests become jobs, and drives a job through its
//! stages.
//!
//! [`prefetch`] sits on top of the orchestrator: it infers the queue a client is
//! about to play from the listing it last read, and requests the items the
//! client is about to reach before it asks for them.
//!
//! Every threshold and limit is a parameter the composition root reads from the
//! settings registry; the pure rules never read settings, and the orchestrator
//! reads them through [`hearthsound_config::settings::Settings`].
#![forbid(unsafe_code)]

mod brackets;
mod budget;
mod error;
mod identity;
mod import;

pub mod download;
pub mod normalize;
pub mod orchestrator;
pub mod path;
pub mod prefetch;
pub mod profile;
pub mod repo;
pub mod score;
pub mod search;
pub mod select;

pub use error::AcquireError;

/// Returns the current time in unix milliseconds.
///
/// Every timestamp this crate and the ledger write shares one clock and one
/// unit.
///
/// # Returns
///
/// The number of milliseconds since the Unix epoch.
pub(crate) fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}
