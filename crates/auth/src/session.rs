//! Web sessions and the login failures they share with the rest of the server.
//!
//! A web client authenticates with a cookie instead of a credential parameter.
//! [`Sessions::create`] mints the pair the client keeps: an opaque session
//! token, of which only the SHA-256 hash is stored, and a CSRF token, which the
//! client echoes in `X-Hearthsound-CSRF` on every unsafe request. Both are 32
//! random bytes encoded as lowercase RFC 4648 base32 without padding, so neither
//! is guessable and both are safe to put in a cookie or a header.
//!
//! A session has a fixed lifetime decided when it is created; resolving it does
//! not extend that. [`Sessions::resolve`] answers the account the session
//! belongs to, and refuses an expired session and one whose account has been
//! disabled since. The `last_seen_at` column is bookkeeping for the admin view:
//! it is written opportunistically, at most once a minute, and a failure to
//! write it never fails the request that triggered it.
//!
//! [`LoginError`] is what [`crate::Authenticator::login`] answers with, and
//! [`verify_login`] is the password check it performs, so the login path and the
//! session path live together with one definition of "wrong credentials".

use std::time::Duration;

use hearthsound_core::new_uuid_v7;
use hearthsound_db::Database;
use rand::RngExt;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::AuthError;
use crate::user::{User, Users};

/// How many random bytes back a session token and a CSRF token.
const TOKEN_BYTES: usize = 32;

/// How many base32 characters encode [`TOKEN_BYTES`], without padding.
const TOKEN_LEN: usize = 52;

/// How long a `last_seen_at` write is left alone.
///
/// A client that makes many requests in a row would otherwise write the same
/// column on every one of them; once a minute is enough for the admin view that
/// reads it.
const LAST_SEEN_INTERVAL_MS: i64 = 60_000;

/// A freshly minted session.
///
/// The token and the CSRF token are handed to the client exactly once: only the
/// token's hash is stored, so it cannot be shown again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    /// The session's UUIDv7 primary key.
    pub id: Uuid,
    /// The opaque token to put in the client's cookie.
    pub token: String,
    /// The token the client echoes in `X-Hearthsound-CSRF` on unsafe methods.
    pub csrf_token: String,
    /// When the session stops being valid, in unix milliseconds.
    pub expires_at: i64,
}

/// A resolved session.
///
/// The account is read at resolution time, so a permission change made after
/// the session was created is in force immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// The session's id, which names the credential a principal used.
    pub id: Uuid,
    /// The account the session belongs to.
    pub user: User,
    /// The CSRF token the client must echo on unsafe methods.
    pub csrf_token: String,
    /// When the session stops being valid, in unix milliseconds.
    pub expires_at: i64,
}

/// The `sessions` table.
///
/// Cloning is cheap: every clone shares the same two pools.
#[derive(Debug, Clone)]
pub struct Sessions {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

impl Sessions {
    /// Builds the repository over a database.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    ///
    /// # Returns
    ///
    /// The repository.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(db: &hearthsound_db::Database) {
    /// let sessions = hearthsound_auth::Sessions::new(db);
    /// # let _ = sessions;
    /// # }
    /// ```
    pub fn new(db: &Database) -> Sessions {
        Sessions {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Creates a session for an account.
    ///
    /// The lifetime is fixed here and never extended by use.
    ///
    /// # Parameters
    ///
    /// - `user`: the account the session belongs to. The column is a foreign
    ///   key, so an unknown user is a database error.
    /// - `user_agent`: the client's `User-Agent` header, kept for the admin
    ///   view; `None` when the header was absent.
    /// - `ttl`: how long the session stays valid.
    ///
    /// # Returns
    ///
    /// The session, with the token the client keeps and the CSRF token it must
    /// echo on unsafe methods.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the insert fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(sessions: &hearthsound_auth::Sessions, user: uuid::Uuid) -> Result<(), hearthsound_auth::AuthError> {
    /// let session = sessions
    ///     .create(user, Some("a browser"), std::time::Duration::from_secs(3600))
    ///     .await?;
    /// assert_eq!(session.token.len(), 52);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn create(
        &self,
        user: Uuid,
        user_agent: Option<&str>,
        ttl: Duration,
    ) -> Result<NewSession, AuthError> {
        let token = random_token();
        let csrf_token = random_token();
        let token_hash = Sha256::digest(token.as_bytes());
        let id = new_uuid_v7();
        let now = crate::now_millis();
        let expires_at = now.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX));

        sqlx::query!(
            "INSERT INTO sessions (id, user_id, token_hash, csrf_token, created_at, expires_at, \
             last_seen_at, user_agent) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            id.as_bytes().as_slice(),
            user.as_bytes().as_slice(),
            token_hash.as_slice(),
            csrf_token,
            now,
            expires_at,
            now,
            user_agent,
        )
        .execute(&self.writer)
        .await?;

        Ok(NewSession {
            id,
            token,
            csrf_token,
            expires_at,
        })
    }

    /// Resolves a presented session token.
    ///
    /// The token is hashed and looked up, so a token that matches no stored
    /// hash is `None` whatever its shape. An expired session, a session whose
    /// account was disabled, and an unknown token all resolve to `None`, and
    /// are deliberately indistinguishable.
    ///
    /// The session's `last_seen_at` is refreshed at most once a minute, in a
    /// task of its own: a failure to write it is logged and does not fail the
    /// resolution.
    ///
    /// # Parameters
    ///
    /// - `token`: the token from the client's cookie, exactly as it was sent.
    ///
    /// # Returns
    ///
    /// The session and its account, or `None` when the token does not
    /// authenticate.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when a query fails and
    /// [`AuthError::CorruptId`] when a stored identifier is not a UUID.
    pub async fn resolve(&self, token: &str) -> Result<Option<SessionRecord>, AuthError> {
        let token_hash = Sha256::digest(token.as_bytes());
        let mut conn = self.readers.acquire().await?;
        let stored = sqlx::query!(
            "SELECT id, user_id, csrf_token, expires_at, last_seen_at FROM sessions \
             WHERE token_hash = ?",
            token_hash.as_slice(),
        )
        .fetch_optional(&mut *conn)
        .await?;

        let Some(stored) = stored else {
            return Ok(None);
        };
        let now = crate::now_millis();
        if stored.expires_at <= now {
            return Ok(None);
        }

        let user_id = Uuid::from_slice(&stored.user_id)?;
        let Some(user) = Users::find_by_id_on(&mut conn, user_id).await? else {
            return Ok(None);
        };
        if user.is_disabled() {
            return Ok(None);
        }

        let id = Uuid::from_slice(&stored.id)?;
        if now.saturating_sub(stored.last_seen_at) >= LAST_SEEN_INTERVAL_MS {
            let sessions = self.clone();
            spawn_touch(async move { sessions.touch_last_seen(id, now).await });
        }

        Ok(Some(SessionRecord {
            id,
            user,
            csrf_token: stored.csrf_token,
            expires_at: stored.expires_at,
        }))
    }

    /// Revokes one session by its token.
    ///
    /// # Parameters
    ///
    /// - `token`: the token from the client's cookie.
    ///
    /// # Returns
    ///
    /// `true` when a session was deleted, `false` when no session had that
    /// token, which is also what revoking a session twice answers.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the delete fails.
    pub async fn revoke(&self, token: &str) -> Result<bool, AuthError> {
        let token_hash = Sha256::digest(token.as_bytes());
        let affected = sqlx::query!(
            "DELETE FROM sessions WHERE token_hash = ?",
            token_hash.as_slice(),
        )
        .execute(&self.writer)
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// Revokes every session of an account.
    ///
    /// # Parameters
    ///
    /// - `user`: the account whose sessions to delete.
    ///
    /// # Returns
    ///
    /// How many sessions were deleted.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the delete fails.
    pub async fn revoke_all(&self, user: Uuid) -> Result<u64, AuthError> {
        let affected = sqlx::query!(
            "DELETE FROM sessions WHERE user_id = ?",
            user.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?
        .rows_affected();
        Ok(affected)
    }

    /// Deletes the sessions that have expired.
    ///
    /// # Parameters
    ///
    /// - `now`: the instant to compare against, in unix milliseconds. A session
    ///   that expires exactly then is already gone.
    ///
    /// # Returns
    ///
    /// How many rows were deleted.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the delete fails.
    pub async fn purge_expired(&self, now: i64) -> Result<u64, AuthError> {
        let affected = sqlx::query!("DELETE FROM sessions WHERE expires_at <= ?", now)
            .execute(&self.writer)
            .await?
            .rows_affected();
        Ok(affected)
    }

    /// Writes a session's `last_seen_at`.
    ///
    /// # Parameters
    ///
    /// - `id`: the session that was just used.
    /// - `now`: the instant to record, in unix milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the update fails.
    async fn touch_last_seen(&self, id: Uuid, now: i64) -> Result<(), AuthError> {
        sqlx::query!(
            "UPDATE sessions SET last_seen_at = ? WHERE id = ?",
            now,
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }
}

/// Why a login was refused.
///
/// The variants are the answers a login can get: it worked, it was refused
/// because the credentials were wrong or the account cannot sign in, or it was
/// refused because the client has spent its failure budget. A server-side
/// failure is [`LoginError::Internal`] and carries its cause, so it can be
/// logged and answered as a server failure rather than as a bad password.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The username is unknown, the account is disabled, or the password is
    /// wrong. All three are one answer, so a client cannot learn which of them
    /// it presented.
    #[error("wrong username or password")]
    WrongCredentials,
    /// Too many failed attempts for this address or username in the last
    /// minute.
    #[error("too many login attempts; try again later")]
    RateLimited,
    /// The check could not be completed.
    #[error(transparent)]
    Internal(#[from] AuthError),
}

/// Checks a password against a stored account.
///
/// This is the login path's shared password check: it reads the account,
/// refuses a disabled one exactly like an unknown one, and verifies the stored
/// Argon2id hash.
///
/// # Parameters
///
/// - `users`: the accounts table.
/// - `username`: the submitted login name, compared case-insensitively.
/// - `password`: the submitted plaintext password.
///
/// # Returns
///
/// The account when the password matches it.
///
/// # Errors
///
/// Returns [`LoginError::WrongCredentials`] for an unknown username, a disabled
/// account, or a wrong password, and [`LoginError::Internal`] when the read or
/// the verification fails.
pub(crate) async fn verify_login(
    users: &Users,
    username: &str,
    password: &str,
) -> Result<User, LoginError> {
    let Some(user) = users.find_by_username(username).await? else {
        return Err(LoginError::WrongCredentials);
    };
    if user.is_disabled() {
        return Err(LoginError::WrongCredentials);
    }
    if !crate::verify_password(password, user.password_hash()).await? {
        return Err(LoginError::WrongCredentials);
    }
    Ok(user)
}

/// Returns a fresh random token.
///
/// # Returns
///
/// [`TOKEN_BYTES`] random bytes as lowercase RFC 4648 base32 without padding,
/// [`TOKEN_LEN`] characters long.
fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill(&mut bytes[..]);
    let token = data_encoding::BASE32_NOPAD
        .encode(&bytes)
        .to_ascii_lowercase();
    debug_assert_eq!(token.len(), TOKEN_LEN);
    token
}

/// Writes a session's `last_seen_at` without holding up the request.
///
/// # Parameters
///
/// - `write`: the write to run.
fn spawn_touch(write: impl Future<Output = Result<(), AuthError>> + Send + 'static) {
    tokio::spawn(async move {
        if let Err(error) = write.await {
            tracing::debug!(error = %error, "could not record a session's last use");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use governor::clock::FakeRelativeClock;
    use hearthsound_config::secret::SecretKey;
    use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
    use hearthsound_db::{Database, MIGRATOR};

    use super::*;
    use crate::subsonic::{Authenticator, Credential};
    use crate::{AuthenticationError, NewUser, ResolveError, UserPermissions};

    /// A migrated temporary database with one account.
    struct Fixture {
        /// Keeps the temporary directory alive.
        _dir: tempfile::TempDir,
        /// The open database.
        db: Database,
        /// The account sessions are created for.
        user: User,
        /// The resolver the login tests drive.
        authenticator: Authenticator<FakeRelativeClock>,
    }

    impl Fixture {
        /// Builds the fixture with a login rate of one per minute.
        async fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(&dir.path().join("hearthsound.db"))
                .await
                .unwrap();
            MIGRATOR.run(db.writer()).await.unwrap();
            let user = Users::new(&db)
                .create(
                    &settings(),
                    &NewUser::new("echo", "Echo", "hunter2", false).with_permissions(
                        UserPermissions {
                            acquire: true,
                            upload: true,
                            download: true,
                        },
                    ),
                )
                .await
                .unwrap();
            let authenticator = Authenticator::with_clock(
                &db,
                SecretKey::generate().secret_box(),
                1,
                FakeRelativeClock::default(),
            );
            Fixture {
                _dir: dir,
                db,
                user,
                authenticator,
            }
        }

        /// Returns the session store over the fixture's database.
        fn sessions(&self) -> Sessions {
            Sessions::new(&self.db)
        }
    }

    /// Resolves a settings snapshot.
    fn settings() -> SettingsStore {
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap()
    }

    #[tokio::test]
    async fn a_session_resolves_to_its_account_until_it_expires() {
        let fixture = Fixture::new().await;
        let sessions = fixture.sessions();

        let created = sessions
            .create(
                fixture.user.id,
                Some("a browser"),
                Duration::from_secs(3600),
            )
            .await
            .unwrap();
        assert_eq!(created.token.len(), TOKEN_LEN);
        assert_eq!(created.csrf_token.len(), TOKEN_LEN);
        assert_ne!(created.token, created.csrf_token);
        assert!(created.token.chars().all(|c| !c.is_ascii_uppercase()));

        let resolved = sessions.resolve(&created.token).await.unwrap().unwrap();
        assert_eq!(resolved.id, created.id);
        assert_eq!(resolved.user.id, fixture.user.id);
        assert_eq!(resolved.user.username, "echo");
        assert_eq!(resolved.csrf_token, created.csrf_token);
        assert_eq!(resolved.expires_at, created.expires_at);

        assert!(sessions.resolve("nope").await.unwrap().is_none());
        let already_expired = sessions
            .create(fixture.user.id, None, Duration::ZERO)
            .await
            .unwrap();
        assert!(
            sessions
                .resolve(&already_expired.token)
                .await
                .unwrap()
                .is_none(),
            "a zero lifetime is already over"
        );

        let stored_hash: Vec<u8> =
            sqlx::query_scalar("SELECT token_hash FROM sessions WHERE id = ?")
                .bind(created.id.as_bytes().as_slice())
                .fetch_one(fixture.db.writer())
                .await
                .unwrap();
        assert_eq!(
            stored_hash,
            Sha256::digest(created.token.as_bytes()).to_vec(),
            "only the token's hash is stored"
        );
    }

    #[tokio::test]
    async fn a_disabled_accounts_session_does_not_resolve() {
        let fixture = Fixture::new().await;
        let sessions = fixture.sessions();
        let created = sessions
            .create(fixture.user.id, None, Duration::from_secs(3600))
            .await
            .unwrap();

        Users::new(&fixture.db)
            .disable(fixture.user.id)
            .await
            .unwrap();

        assert!(sessions.resolve(&created.token).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn revoking_removes_the_session() {
        let fixture = Fixture::new().await;
        let sessions = fixture.sessions();
        let first = sessions
            .create(fixture.user.id, None, Duration::from_secs(3600))
            .await
            .unwrap();
        let second = sessions
            .create(fixture.user.id, None, Duration::from_secs(3600))
            .await
            .unwrap();

        assert!(sessions.revoke(&first.token).await.unwrap());
        assert!(!sessions.revoke(&first.token).await.unwrap());
        assert!(sessions.resolve(&first.token).await.unwrap().is_none());
        assert!(sessions.resolve(&second.token).await.unwrap().is_some());

        assert_eq!(sessions.revoke_all(fixture.user.id).await.unwrap(), 1);
        assert!(sessions.resolve(&second.token).await.unwrap().is_none());
        assert_eq!(sessions.revoke_all(fixture.user.id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn purging_removes_only_the_expired_sessions() {
        let fixture = Fixture::new().await;
        let sessions = fixture.sessions();
        let live = sessions
            .create(fixture.user.id, None, Duration::from_secs(3600))
            .await
            .unwrap();
        let dead = sessions
            .create(fixture.user.id, None, Duration::from_secs(60))
            .await
            .unwrap();

        let purged = sessions.purge_expired(dead.expires_at).await.unwrap();

        assert_eq!(purged, 1);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
            .fetch_one(fixture.db.writer())
            .await
            .unwrap();
        assert_eq!(rows, 1, "the live session is kept");
        assert!(sessions.resolve(&live.token).await.unwrap().is_some());
        assert!(sessions.resolve(&dead.token).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resolving_refreshes_last_seen_at_at_most_once_a_minute() {
        let fixture = Fixture::new().await;
        let sessions = fixture.sessions();
        let created = sessions
            .create(fixture.user.id, None, Duration::from_secs(3600))
            .await
            .unwrap();

        let stale = crate::now_millis() - LAST_SEEN_INTERVAL_MS;
        sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
            .bind(stale)
            .bind(created.id.as_bytes().as_slice())
            .execute(fixture.db.writer())
            .await
            .unwrap();

        sessions.resolve(&created.token).await.unwrap();
        let seen = async {
            for _ in 0..100 {
                let last_seen_at: i64 =
                    sqlx::query_scalar("SELECT last_seen_at FROM sessions WHERE id = ?")
                        .bind(created.id.as_bytes().as_slice())
                        .fetch_one(fixture.db.writer())
                        .await
                        .unwrap();
                if last_seen_at > stale {
                    return last_seen_at;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the session's last use was never recorded");
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(5), seen)
                .await
                .unwrap()
                > stale
        );
    }

    #[tokio::test]
    async fn login_verifies_the_password_against_the_stored_hash() {
        let fixture = Fixture::new().await;
        let users = Users::new(&fixture.db);

        let signed_in = verify_login(&users, "ECHO", "hunter2").await.unwrap();
        assert_eq!(signed_in.id, fixture.user.id);

        for (username, password) in [("echo", "wrong"), ("nobody", "hunter2")] {
            let error = verify_login(&users, username, password).await.unwrap_err();
            assert!(matches!(error, LoginError::WrongCredentials));
        }

        Users::new(&fixture.db)
            .disable(fixture.user.id)
            .await
            .unwrap();
        let error = verify_login(&users, "echo", "hunter2").await.unwrap_err();
        assert!(
            matches!(error, LoginError::WrongCredentials),
            "a disabled account answers like an unknown one"
        );
    }

    /// The login path and the bearer path share one limiter, keyed per address.
    #[tokio::test]
    async fn login_and_bearer_share_the_limiter() {
        let fixture = Fixture::new().await;
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        let error = fixture
            .authenticator
            .login(ip, "echo", "wrong")
            .await
            .unwrap_err();
        assert!(matches!(error, LoginError::WrongCredentials));

        let error = fixture
            .authenticator
            .login(ip, "echo", "hunter2")
            .await
            .unwrap_err();
        assert!(
            matches!(error, LoginError::RateLimited),
            "a correct login is refused once the address has spent its budget"
        );

        let error = fixture
            .authenticator
            .resolve_bearer(ip, "hs_nope")
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ResolveError::Rejected(AuthenticationError::TooManyAttempts)
        ));
    }

    #[tokio::test]
    async fn a_bearer_resolves_an_api_key_and_charges_failures() {
        let fixture = Fixture::new().await;
        let key = crate::ApiKeys::new(&fixture.db)
            .generate(fixture.user.id, "a client")
            .await
            .unwrap()
            .plaintext;
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        let principal = fixture
            .authenticator
            .resolve_bearer(ip, &key)
            .await
            .unwrap();
        assert_eq!(principal.username, "echo");
        assert!(principal.can_acquire && principal.can_upload && principal.can_download);
        assert!(matches!(principal.credential, Credential::ApiKey { .. }));

        let error = fixture
            .authenticator
            .resolve_bearer(ip, "hs_bogus")
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ResolveError::Rejected(AuthenticationError::InvalidApiKey)
        ));

        // The failure above spent this address's single budget.
        let error = fixture
            .authenticator
            .resolve_bearer(ip, &key)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ResolveError::Rejected(AuthenticationError::TooManyAttempts)
        ));
    }
}
