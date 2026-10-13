//! Candidate matching for on-demand acquisition.
//!
//! This crate holds the pure decision logic of the acquisition pipeline: it
//! takes the wanted track and the candidates a provider reported, and says
//! which candidate to download and in what order. It touches no database, no
//! plugin and no file system, so every rule here is testable with plain values:
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
//! Every threshold and limit is a parameter the composition root reads from the
//! settings registry; this crate never reads settings itself.
#![forbid(unsafe_code)]

mod brackets;

pub mod normalize;
pub mod path;
pub mod score;
