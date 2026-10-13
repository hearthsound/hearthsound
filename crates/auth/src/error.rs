//! Errors of the auth layer.
//!
//! [`AuthError`] covers everything that can go wrong while storing or checking
//! a credential: the database rejecting a statement, password hashing, the
//! secret box, and the validation of a new account or a per-user setting
//! override. The Subsonic credential check has its own error type,
//! [`crate::subsonic::AuthenticationError`], because it has to carry a Subsonic
//! error code and an optional help URL rather than a cause.

use uuid::Uuid;

/// An error from storing or checking a credential.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The database rejected a read or a write.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A blocking task such as password hashing could not be joined.
    #[error("background task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// Argon2 rejected the password, the salt, or the stored hash.
    #[error("password hashing error: {0}")]
    Password(#[from] argon2::password_hash::Error),
    /// The secret box could not seal or open a secret.
    #[error(transparent)]
    Secret(#[from] hearthsound_config::secret::SecretError),
    /// A value stored as a setting was rejected by that setting's kind.
    #[error(transparent)]
    Setting(#[from] hearthsound_config::ConfigError),
    /// A new account was submitted with an empty field.
    #[error("`{field}` must not be empty")]
    EmptyField {
        /// The name of the field that was empty.
        field: &'static str,
    },
    /// A username was submitted that an account already uses.
    ///
    /// Usernames are compared case-insensitively, so `Echo` and `echo` name the
    /// same account.
    #[error("username `{username}` is already taken")]
    UsernameTaken {
        /// The username that clashed.
        username: String,
    },
    /// A 16-byte identifier stored as a BLOB did not decode as a UUID.
    #[error("stored identifier is not a UUID: {0}")]
    CorruptId(#[from] uuid::Error),
    /// A per-user setting was written under a key the registry does not know.
    #[error("setting `{key}` is not registered")]
    UnknownUserSetting {
        /// The unregistered key.
        key: String,
    },
    /// A per-user setting was written for a setting users may not override.
    #[error("setting `{key}` cannot be overridden per user")]
    NotUserOverridable {
        /// The key that is not user overridable.
        key: String,
    },
    /// A user id was referenced that has no account.
    #[error("user `{user_id}` does not exist")]
    UnknownUser {
        /// The id that has no account.
        user_id: Uuid,
    },
}
