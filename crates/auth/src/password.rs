//! Argon2id password hashing.
//!
//! An account password is never stored or logged in the clear. [`hash_password`]
//! derives a self-describing Argon2id hash string (`$argon2id$v=19$m=19456,t=2,p=1$...`)
//! with a fresh 16-byte salt, and [`verify_password`] recomputes it from the
//! parameters the stored string carries. Argon2id is deliberately expensive, so
//! both run on the blocking pool instead of stalling a runtime worker.

use argon2::Argon2;
use argon2::password_hash::{Error as PasswordHashError, PasswordHasher, PasswordVerifier};

use crate::error::AuthError;

/// Hashes a password with Argon2id.
///
/// A fresh random salt is generated for every call, so hashing the same
/// password twice yields two different strings.
///
/// # Parameters
///
/// - `password`: the plaintext password.
///
/// # Returns
///
/// The Argon2id hash string to store in `users.password_hash`.
///
/// # Errors
///
/// Returns [`AuthError::Password`] when Argon2 rejects the password (it is
/// longer than 4 GiB) or the system random number generator fails, and
/// [`AuthError::Join`] when the blocking task cannot be joined.
///
/// # Examples
///
/// ```
/// # async fn example() -> Result<(), hearthsound_auth::AuthError> {
/// let hash = hearthsound_auth::hash_password("hunter2").await?;
/// assert!(hash.starts_with("$argon2id$"));
/// assert!(hearthsound_auth::verify_password("hunter2", &hash).await?);
/// # Ok(())
/// # }
/// ```
pub async fn hash_password(password: &str) -> Result<String, AuthError> {
    let password = password.to_owned();
    let hash = tokio::task::spawn_blocking(move || hash_blocking(&password)).await??;
    Ok(hash)
}

/// Checks a password against a stored Argon2id hash.
///
/// # Parameters
///
/// - `password`: the plaintext password to check.
/// - `hash`: the Argon2id hash string from `users.password_hash`.
///
/// # Returns
///
/// `true` when the password matches.
///
/// # Errors
///
/// Returns [`AuthError::Password`] when `hash` is not a readable Argon2 hash
/// string, and [`AuthError::Join`] when the blocking task cannot be joined. A
/// password that simply does not match is `Ok(false)`, not an error.
pub async fn verify_password(password: &str, hash: &str) -> Result<bool, AuthError> {
    let password = password.to_owned();
    let hash = hash.to_owned();
    let matched = tokio::task::spawn_blocking(move || verify_blocking(&password, &hash)).await??;
    Ok(matched)
}

/// Hashes a password on the current thread.
///
/// # Errors
///
/// Returns what [`hash_password`] documents.
fn hash_blocking(password: &str) -> Result<String, AuthError> {
    let hash = Argon2::default().hash_password(password.as_bytes())?;
    Ok(hash.to_string())
}

/// Verifies a password on the current thread.
///
/// # Errors
///
/// Returns what [`verify_password`] documents.
fn verify_blocking(password: &str, hash: &str) -> Result<bool, AuthError> {
    match Argon2::default().verify_password(password.as_bytes(), hash) {
        Ok(()) => Ok(true),
        Err(PasswordHashError::PasswordInvalid) => Ok(false),
        Err(error) => Err(AuthError::Password(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_and_verifies_a_password() {
        let hash = hash_password("correct horse").await.unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &hash).await.unwrap());
        assert!(!verify_password("wrong horse", &hash).await.unwrap());
    }

    #[tokio::test]
    async fn generates_a_fresh_salt_per_call() {
        let first = hash_password("same").await.unwrap();
        let second = hash_password("same").await.unwrap();
        assert_ne!(first, second);
        assert!(verify_password("same", &first).await.unwrap());
        assert!(verify_password("same", &second).await.unwrap());
    }

    #[tokio::test]
    async fn rejects_a_malformed_stored_hash() {
        let error = verify_password("whatever", "not a phc string").await;
        assert!(matches!(error, Err(AuthError::Password(_))));
    }
}
