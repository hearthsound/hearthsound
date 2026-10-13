//! Accounts and their credentials.
//!
//! This crate owns the tables that say who a request is: `users`,
//! `api_keys`, `app_passwords`, and the per-user overrides in `user_settings`.
//! It is the only crate that hashes a password, compares a secret in constant
//! time, or decrypts an app password.
//!
//! The credentials follow the design's storage rules:
//!
//! - an account password is stored as an Argon2id hash and never reversibly
//!   ([`hash_password`], [`verify_password`], [`Users`]);
//! - a setting a user may override shadows the server value for that user only
//!   ([`UserSettings`]).
//!
//! Library code takes plain values and reads [`hearthsound_config::SettingsStore`]
//! snapshots handed to it by the composition root; it never reads the process
//! environment or a config file itself.
#![forbid(unsafe_code)]

mod error;
mod password;
mod user;
mod user_settings;

pub use error::AuthError;
pub use password::{hash_password, verify_password};
pub use user::{NewUser, Permission, UnknownPermission, User, UserPermissions, Users};
pub use user_settings::{UserSettingEntry, UserSettings};

/// Returns the current time in unix milliseconds.
///
/// Every timestamp column the auth tables hold (`created_at`, `updated_at`,
/// `last_used_at`, `revoked_at`, `disabled_at`) is written from here so all of
/// them share one clock and one unit.
///
/// # Returns
///
/// Milliseconds since the unix epoch.
pub(crate) fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}
