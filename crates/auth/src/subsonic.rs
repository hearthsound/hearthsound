//! Resolving Subsonic credentials into a principal.
//!
//! The `/rest` surface accepts four credential shapes, and this module turns any
//! of them into one [`Principal`] or one [`AuthenticationError`]:
//!
//! - an API key in `apiKey=` (the OpenSubsonic `apiKeyAuthentication`
//!   extension), which must not be combined with `u`;
//! - an app password in `u` + `p`, sent either in the clear or hex-encoded after
//!   an `enc:` prefix;
//! - an app password in `u` + `t` + `s`, where `t = md5(secret + salt)` and the
//!   salt is at least six characters;
//! - the account password in `u` + `p`, but only when the
//!   `subsonic.allow_account_password` setting enables it.
//!
//! A presented secret is compared against each of the account's active app
//! passwords in constant time. The account password is verified as an Argon2id
//! hash, never decrypted, which is why token authentication can only ever match
//! an app password: the server has no plaintext of an account password to hash
//! into `t`.
//!
//! Because the server cannot tell an account password from a mistyped app
//! password, every `u` + `p` request that fails while
//! `subsonic.allow_account_password` is off is answered with error 40 and a help
//! URL pointing at the app-password documentation, which is exactly the advice
//! the client needs.
//!
//! Logins that present a username are rate limited on their failures: a key
//! that has spent its failure budget is refused before any credential is
//! checked, and a successful login costs nothing. The limit's keys and window
//! are described in [`crate::rate_limit`].

use std::net::IpAddr;

use governor::clock::{Clock, DefaultClock};
use hearthsound_config::secret::SecretBox;
use hearthsound_config::{SettingsStore, keys};
use hearthsound_core::{EntityId, IdPrefix};
use hearthsound_db::Database;
use md5::{Digest, Md5};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::api_key::{ApiKeyOwner, ApiKeys};
use crate::app_password::AppPasswords;
use crate::error::AuthError;
use crate::rate_limit::LoginRateLimiter;
use crate::user::{Permission, User, Users};
use crate::verify_password;

/// Where a client is sent when its account password is refused on `/rest`.
pub const APP_PASSWORDS_HELP_URL: &str = "https://hearthsound.app/docs/clients/app-passwords";

/// The shortest salt token authentication accepts.
pub const MIN_SALT_LEN: usize = 6;

/// The credential parameters of a request.
///
/// Every field is the raw query parameter of the same name, `None` when the
/// parameter was absent. An empty string is treated as absent by the checks that
/// need a value and as present by the conflict checks, so a client is never
/// silently half-authenticated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubsonicCredentials {
    /// `u`, the username.
    pub username: Option<String>,
    /// `p`, the password, in the clear or hex-encoded after `enc:`.
    pub password: Option<String>,
    /// `t`, the token `md5(secret + salt)`.
    pub token: Option<String>,
    /// `s`, the salt the token was computed with.
    pub salt: Option<String>,
    /// `apiKey`, an API key.
    pub api_key: Option<String>,
}

/// Which credential authenticated a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// An API key, named by the `api_keys` row it matched.
    ApiKey {
        /// The id of the key that matched.
        id: Uuid,
    },
    /// An app password, named by the `app_passwords` row it matched.
    AppPassword {
        /// The id of the app password that matched.
        id: Uuid,
    },
    /// The account password, stored as an Argon2id hash.
    AccountPassword,
}

/// An authenticated requester.
///
/// The principal is what a handler needs to answer a request: who it is, what it
/// may do, and which credential proved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The user's client id, `us_<...>`; `user_id.uuid()` is the database key.
    pub user_id: EntityId,
    /// The login name.
    pub username: String,
    /// Whether the user is an administrator.
    pub is_admin: bool,
    /// Whether the user may request acquisitions.
    pub can_acquire: bool,
    /// Whether the user may upload files.
    pub can_upload: bool,
    /// Whether the user may download stored files.
    pub can_download: bool,
    /// The credential that authenticated the request.
    pub credential: Credential,
}

impl Principal {
    /// Reports whether the principal may exercise a permission.
    ///
    /// # Parameters
    ///
    /// - `permission`: the permission to check.
    ///
    /// # Returns
    ///
    /// `true` when the corresponding flag is set. Administrators are not
    /// special-cased here; the flags are authoritative.
    pub fn allows(&self, permission: Permission) -> bool {
        match permission {
            Permission::Acquire => self.can_acquire,
            Permission::Upload => self.can_upload,
            Permission::Download => self.can_download,
        }
    }
}

/// A request rejected by the credential check.
///
/// Each variant is one row of the Subsonic error table: [`code`] is the numeric
/// code the client sees, [`message`] the text, and [`help_url`] the optional
/// documentation link of the `apiKeyAuthentication` extension.
///
/// [`code`]: AuthenticationError::code
/// [`message`]: AuthenticationError::message
/// [`help_url`]: AuthenticationError::help_url
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthenticationError {
    /// A required parameter is absent or malformed. Subsonic error 10.
    #[error("required parameter `{parameter}` is missing")]
    MissingParameter {
        /// The name of the parameter, as the client wrote it.
        parameter: &'static str,
    },
    /// The username is unknown, the account is disabled, or the secret is
    /// wrong. Subsonic error 40.
    #[error("wrong username or password")]
    WrongCredentials,
    /// The account password was presented while
    /// `subsonic.allow_account_password` is off. Subsonic error 40 with a help
    /// URL.
    #[error("the account password is not accepted on /rest; use an app password or an API key")]
    AccountPasswordDisabled {
        /// The documentation link to send with the error.
        help_url: String,
    },
    /// `u` + `t` + `s` was presented while `subsonic.token_auth` is off.
    /// Subsonic error 41.
    #[error("token authentication is not supported")]
    TokenAuthDisabled,
    /// More than one authentication mechanism was supplied. Subsonic error 42.
    #[error("more than one authentication mechanism was supplied")]
    MultipleMechanisms,
    /// `apiKey` and `u` were supplied together. Subsonic error 43.
    #[error("an API key and a username were supplied together")]
    ApiKeyWithUsername,
    /// The API key is malformed, unknown, revoked, or belongs to a disabled
    /// account. Subsonic error 44.
    #[error("invalid API key")]
    InvalidApiKey,
    /// Too many login attempts for this address or username in the last minute.
    /// Subsonic error 40.
    #[error("too many login attempts; try again later")]
    TooManyAttempts,
}

impl AuthenticationError {
    /// Returns the Subsonic error code.
    ///
    /// # Returns
    ///
    /// The number the `error.code` field carries: 10, 40, 41, 42, 43 or 44.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_auth::AuthenticationError;
    ///
    /// assert_eq!(AuthenticationError::InvalidApiKey.code(), 44);
    /// assert_eq!(AuthenticationError::WrongCredentials.code(), 40);
    /// ```
    pub fn code(&self) -> i32 {
        match self {
            AuthenticationError::MissingParameter { .. } => 10,
            AuthenticationError::WrongCredentials
            | AuthenticationError::AccountPasswordDisabled { .. }
            | AuthenticationError::TooManyAttempts => 40,
            AuthenticationError::TokenAuthDisabled => 41,
            AuthenticationError::MultipleMechanisms => 42,
            AuthenticationError::ApiKeyWithUsername => 43,
            AuthenticationError::InvalidApiKey => 44,
        }
    }

    /// Returns the message the `error.message` field carries.
    ///
    /// The text is the standard Subsonic message for the code wherever the base
    /// protocol defines one, so a client that displays it shows what it would
    /// show against any other server. [`AuthenticationError::TooManyAttempts`]
    /// shares code 40 with [`AuthenticationError::WrongCredentials`] but not its
    /// text, because "wrong username or password" would misdescribe a limit.
    ///
    /// # Returns
    ///
    /// A short human readable sentence.
    pub fn message(&self) -> &'static str {
        match self {
            AuthenticationError::MissingParameter { .. } => "Required parameter is missing.",
            AuthenticationError::WrongCredentials => "Wrong username or password.",
            AuthenticationError::AccountPasswordDisabled { .. } => "Wrong username or password.",
            AuthenticationError::TokenAuthDisabled => {
                "Token authentication not supported for LDAP users."
            }
            AuthenticationError::MultipleMechanisms => {
                "Provided authentication mechanism not supported."
            }
            AuthenticationError::ApiKeyWithUsername => {
                "Multiple conflicting authentication mechanisms provided."
            }
            AuthenticationError::InvalidApiKey => "Invalid API key.",
            AuthenticationError::TooManyAttempts => "Too many login attempts; try again later.",
        }
    }

    /// Returns the `error.helpUrl` field, when the error has one.
    ///
    /// # Returns
    ///
    /// The documentation link, or `None` when the error has no advice to give.
    pub fn help_url(&self) -> Option<&str> {
        match self {
            AuthenticationError::AccountPasswordDisabled { help_url } => Some(help_url),
            _ => None,
        }
    }
}

/// The outcome of a credential check that may also fail internally.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The request was rejected; render [`AuthenticationError::code`] and its
    /// message.
    #[error(transparent)]
    Rejected(#[from] AuthenticationError),
    /// The check could not be completed: the database, the secret box, or the
    /// settings snapshot failed. Render this as a server-side failure, not as a
    /// credential error.
    #[error(transparent)]
    Internal(#[from] AuthError),
}

/// The credential resolver for the `/rest` surface.
///
/// The clock of the login rate limiter is a type parameter so a test can drive a
/// fake clock; [`Authenticator::new`] builds one over the real clock.
pub struct Authenticator<C: Clock + Clone = DefaultClock> {
    /// The `users` table.
    users: Users,
    /// The `api_keys` table.
    api_keys: ApiKeys,
    /// The `app_passwords` table.
    app_passwords: AppPasswords,
    /// The login rate limiter.
    limiter: LoginRateLimiter<C>,
}

impl<C: Clock + Clone> std::fmt::Debug for Authenticator<C> {
    /// Reports the type without the pools or the key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Authenticator")
    }
}

impl Authenticator {
    /// Builds the resolver over a database, the secret box, and a login rate.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    /// - `secret_box`: the box the app passwords are sealed with, which must be
    ///   the same one `hearthsound-auth` writes them with.
    /// - `login_rate_limit_per_minute`: the value of the
    ///   `auth.login_rate_limit_per_minute` setting the limiter starts with.
    ///
    /// # Returns
    ///
    /// The resolver.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(db: &hearthsound_db::Database, secret_box: hearthsound_config::secret::SecretBox) {
    /// let authenticator = hearthsound_auth::Authenticator::new(db, secret_box, 10);
    /// # let _ = authenticator;
    /// # }
    /// ```
    pub fn new(
        db: &Database,
        secret_box: SecretBox,
        login_rate_limit_per_minute: u32,
    ) -> Authenticator {
        Authenticator::with_clock(
            db,
            secret_box,
            login_rate_limit_per_minute,
            DefaultClock::default(),
        )
    }
}

impl<C: Clock + Clone> Authenticator<C> {
    /// Builds the resolver over a given limiter clock.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database.
    /// - `secret_box`: the box the app passwords are sealed with.
    /// - `login_rate_limit_per_minute`: the rate the limiter starts with.
    /// - `clock`: the clock the limiter reads.
    ///
    /// # Returns
    ///
    /// The resolver.
    pub fn with_clock(
        db: &Database,
        secret_box: SecretBox,
        login_rate_limit_per_minute: u32,
        clock: C,
    ) -> Authenticator<C> {
        Authenticator {
            users: Users::new(db),
            api_keys: ApiKeys::new(db),
            app_passwords: AppPasswords::new(db, secret_box),
            limiter: LoginRateLimiter::with_clock(login_rate_limit_per_minute, clock),
        }
    }

    /// Returns the login rate limiter.
    ///
    /// A caller that follows `auth.login_rate_limit_per_minute` across settings
    /// changes applies the new value with
    /// [`LoginRateLimiter::set_per_minute`].
    pub fn limiter(&self) -> &LoginRateLimiter<C> {
        &self.limiter
    }

    /// Resolves a request's credentials into a principal.
    ///
    /// # Parameters
    ///
    /// - `settings`: the resolved settings snapshot that supplies
    ///   `subsonic.allow_account_password` and `subsonic.token_auth`.
    /// - `client_ip`: the address the request came from, used to key the login
    ///   rate limit.
    /// - `credentials`: the credential parameters of the request.
    ///
    /// # Returns
    ///
    /// The authenticated principal.
    ///
    /// # Errors
    ///
    /// Returns [`ResolveError::Rejected`] with the Subsonic error to report when
    /// the credentials are absent, contradictory, or wrong, and
    /// [`ResolveError::Internal`] when the database, the secret box, or the
    /// settings snapshot fails.
    pub async fn resolve(
        &self,
        settings: &SettingsStore,
        client_ip: IpAddr,
        credentials: &SubsonicCredentials,
    ) -> Result<Principal, ResolveError> {
        let has_key = credentials.api_key.is_some();
        let has_password = credentials.password.is_some();
        let has_token_material = credentials.token.is_some() || credentials.salt.is_some();

        if has_key && credentials.username.is_some() {
            return Err(AuthenticationError::ApiKeyWithUsername.into());
        }
        let mechanisms =
            usize::from(has_key) + usize::from(has_password) + usize::from(has_token_material);
        if mechanisms > 1 {
            return Err(AuthenticationError::MultipleMechanisms.into());
        }

        if has_key {
            let key = credentials.api_key.as_deref().unwrap_or_default();
            return self.resolve_api_key(key).await;
        }

        let Some(username) = credentials
            .username
            .as_deref()
            .filter(|name| !name.is_empty())
        else {
            return Err(AuthenticationError::MissingParameter { parameter: "u" }.into());
        };

        if has_password {
            return self
                .resolve_password(settings, client_ip, username, credentials)
                .await;
        }
        if has_token_material {
            return self
                .resolve_token(settings, client_ip, username, credentials)
                .await;
        }
        Err(AuthenticationError::MissingParameter { parameter: "p" }.into())
    }

    /// Resolves an API key.
    ///
    /// # Errors
    ///
    /// Returns [`AuthenticationError::InvalidApiKey`] for a malformed, unknown,
    /// revoked, or disabled-account key.
    async fn resolve_api_key(&self, key: &str) -> Result<Principal, ResolveError> {
        let Some(owner) = self.api_keys.resolve(key).await? else {
            return Err(AuthenticationError::InvalidApiKey.into());
        };
        let key_id = owner.key_id;
        let api_keys = self.api_keys.clone();
        spawn_touch(async move { api_keys.touch_last_used(key_id).await });

        Ok(principal_from_owner(
            &owner,
            Credential::ApiKey { id: key_id },
        ))
    }

    /// Resolves `u` + `p`.
    ///
    /// Only a rejected attempt is charged to the login rate limit; a correct
    /// secret costs nothing, because a Subsonic client sends its credentials
    /// with every request.
    ///
    /// # Errors
    ///
    /// Returns [`AuthenticationError::AccountPasswordDisabled`] when no app
    /// password matches and the account password is not accepted on `/rest`,
    /// [`AuthenticationError::WrongCredentials`] when the account password is
    /// accepted and does not match, and [`AuthenticationError::TooManyAttempts`]
    /// when the address or the username has spent its failure budget.
    async fn resolve_password(
        &self,
        settings: &SettingsStore,
        client_ip: IpAddr,
        username: &str,
        credentials: &SubsonicCredentials,
    ) -> Result<Principal, ResolveError> {
        self.limiter
            .check(client_ip, username)
            .map_err(|_| AuthenticationError::TooManyAttempts)?;
        let outcome = self
            .verify_password_login(settings, username, credentials)
            .await;
        self.charge_rejection(client_ip, username, &outcome);
        outcome
    }

    /// Verifies `u` + `p` against the account's app passwords and, when
    /// allowed, its account password.
    ///
    /// # Errors
    ///
    /// As [`Authenticator::resolve_password`], without the rate limit.
    async fn verify_password_login(
        &self,
        settings: &SettingsStore,
        username: &str,
        credentials: &SubsonicCredentials,
    ) -> Result<Principal, ResolveError> {
        let allow_account_password = settings
            .get(keys::SUBSONIC_ALLOW_ACCOUNT_PASSWORD)
            .map_err(AuthError::from)?;

        let presented = credentials.password.as_deref().unwrap_or_default();
        let secret = match presented.strip_prefix("enc:") {
            Some(hex) => match data_encoding::HEXLOWER_PERMISSIVE.decode(hex.as_bytes()) {
                Ok(bytes) => bytes,
                Err(_) => return Err(self.reject_password(allow_account_password).into()),
            },
            None => presented.as_bytes().to_vec(),
        };

        let Some(user) = self.users.find_by_username(username).await? else {
            return Err(self.reject_password(allow_account_password).into());
        };
        if user.is_disabled() {
            return Err(self.reject_password(allow_account_password).into());
        }

        for candidate in self.app_passwords.active_secrets(user.id).await? {
            if secrets_match(&candidate.secret, &secret) {
                let id = candidate.id;
                let app_passwords = self.app_passwords.clone();
                spawn_touch(async move { app_passwords.touch_last_used(id).await });
                return Ok(principal_from_user(&user, Credential::AppPassword { id }));
            }
        }

        if allow_account_password
            && let Ok(text) = std::str::from_utf8(&secret)
            && verify_password(text, user.password_hash()).await?
        {
            return Ok(principal_from_user(&user, Credential::AccountPassword));
        }

        Err(self.reject_password(allow_account_password).into())
    }

    /// Resolves `u` + `t` + `s`.
    ///
    /// Only a rejected attempt is charged to the login rate limit, as for
    /// [`Authenticator::resolve_password`].
    ///
    /// # Errors
    ///
    /// Returns [`AuthenticationError::MissingParameter`] for an absent token or
    /// a salt below [`MIN_SALT_LEN`], [`AuthenticationError::TokenAuthDisabled`]
    /// when `subsonic.token_auth` is off,
    /// [`AuthenticationError::TooManyAttempts`] when the address or the
    /// username has spent its failure budget, and
    /// [`AuthenticationError::WrongCredentials`] when no active app password
    /// hashes to the token.
    async fn resolve_token(
        &self,
        settings: &SettingsStore,
        client_ip: IpAddr,
        username: &str,
        credentials: &SubsonicCredentials,
    ) -> Result<Principal, ResolveError> {
        let token = credentials
            .token
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or(AuthenticationError::MissingParameter { parameter: "t" })?;
        let salt = credentials
            .salt
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or(AuthenticationError::MissingParameter { parameter: "s" })?;
        if salt.len() < MIN_SALT_LEN {
            return Err(AuthenticationError::MissingParameter { parameter: "s" }.into());
        }

        if !settings
            .get(keys::SUBSONIC_TOKEN_AUTH)
            .map_err(AuthError::from)?
        {
            return Err(AuthenticationError::TokenAuthDisabled.into());
        }

        self.limiter
            .check(client_ip, username)
            .map_err(|_| AuthenticationError::TooManyAttempts)?;
        let outcome = self.verify_token(username, token, salt).await;
        self.charge_rejection(client_ip, username, &outcome);
        outcome
    }

    /// Verifies a token against `md5(secret + salt)` for each of the account's
    /// active app passwords.
    ///
    /// # Errors
    ///
    /// Returns [`AuthenticationError::WrongCredentials`] when the token is not
    /// hex, the account is unknown or disabled, or no app password matches.
    async fn verify_token(
        &self,
        username: &str,
        token: &str,
        salt: &str,
    ) -> Result<Principal, ResolveError> {
        let expected = match data_encoding::HEXLOWER_PERMISSIVE.decode(token.as_bytes()) {
            Ok(bytes) => bytes,
            Err(_) => return Err(AuthenticationError::WrongCredentials.into()),
        };

        let Some(user) = self.users.find_by_username(username).await? else {
            return Err(AuthenticationError::WrongCredentials.into());
        };
        if user.is_disabled() {
            return Err(AuthenticationError::WrongCredentials.into());
        }

        for candidate in self.app_passwords.active_secrets(user.id).await? {
            let mut hasher = Md5::new();
            hasher.update(&candidate.secret);
            hasher.update(salt.as_bytes());
            let computed = hasher.finalize();
            if secrets_match(&computed, &expected) {
                let id = candidate.id;
                let app_passwords = self.app_passwords.clone();
                spawn_touch(async move { app_passwords.touch_last_used(id).await });
                return Ok(principal_from_user(&user, Credential::AppPassword { id }));
            }
        }

        Err(AuthenticationError::WrongCredentials.into())
    }

    /// Charges a rejected login attempt to the rate limit.
    ///
    /// An internal failure is not charged, because it says nothing about the
    /// presented secret.
    ///
    /// # Parameters
    ///
    /// - `client_ip`: the address the request came from.
    /// - `username`: the submitted username.
    /// - `outcome`: the result of the credential check.
    fn charge_rejection(
        &self,
        client_ip: IpAddr,
        username: &str,
        outcome: &Result<Principal, ResolveError>,
    ) {
        if let Err(ResolveError::Rejected(_)) = outcome {
            self.limiter.record_failure(client_ip, username);
        }
    }

    /// Builds the rejection for a failed `u` + `p` check.
    ///
    /// # Parameters
    ///
    /// - `allow_account_password`: whether `subsonic.allow_account_password` is
    ///   on.
    ///
    /// # Returns
    ///
    /// Error 40 with a help URL when the account password is not accepted, which
    /// is the advice a client that sent one needs, and plain error 40 otherwise.
    fn reject_password(&self, allow_account_password: bool) -> AuthenticationError {
        if allow_account_password {
            AuthenticationError::WrongCredentials
        } else {
            AuthenticationError::AccountPasswordDisabled {
                help_url: APP_PASSWORDS_HELP_URL.to_owned(),
            }
        }
    }
}

/// Compares two secrets in constant time.
///
/// # Parameters
///
/// - `expected`: the secret the server holds.
/// - `presented`: the secret the client sent.
///
/// # Returns
///
/// `true` when they are equal. Values of different lengths are unequal, and the
/// comparison time does not depend on where they first differ.
fn secrets_match(expected: &[u8], presented: &[u8]) -> bool {
    bool::from(expected.ct_eq(presented))
}

/// Builds a principal from a stored account.
///
/// # Parameters
///
/// - `user`: the account that matched.
/// - `credential`: the credential that matched it.
///
/// # Returns
///
/// The principal.
fn principal_from_user(user: &User, credential: Credential) -> Principal {
    Principal {
        user_id: EntityId::from_uuid(IdPrefix::User, user.id),
        username: user.username.clone(),
        is_admin: user.is_admin,
        can_acquire: user.can_acquire,
        can_upload: user.can_upload,
        can_download: user.can_download,
        credential,
    }
}

/// Builds a principal from the owner of an API key.
///
/// # Parameters
///
/// - `owner`: the key's owner, as `api_keys.resolve` reported it.
/// - `credential`: the credential that matched.
///
/// # Returns
///
/// The principal.
fn principal_from_owner(owner: &ApiKeyOwner, credential: Credential) -> Principal {
    Principal {
        user_id: EntityId::from_uuid(IdPrefix::User, owner.user_id),
        username: owner.username.clone(),
        is_admin: owner.is_admin,
        can_acquire: owner.can_acquire,
        can_upload: owner.can_upload,
        can_download: owner.can_download,
        credential,
    }
}

/// Records a credential's last use without holding up the request.
///
/// A failure to write the timestamp is logged and otherwise ignored: a client
/// that authenticated must not be failed because a bookkeeping write lost a
/// race.
///
/// # Parameters
///
/// - `write`: the write to run.
fn spawn_touch(write: impl Future<Output = Result<(), AuthError>> + Send + 'static) {
    tokio::spawn(async move {
        if let Err(error) = write.await {
            tracing::debug!(error = %error, "could not record a credential's last use");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use governor::clock::FakeRelativeClock;
    use hearthsound_config::secret::SecretKey;
    use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
    use hearthsound_core::new_uuid_v7;
    use hearthsound_db::{Database, MIGRATOR};
    use uuid::Uuid;

    use super::*;
    use crate::{ApiKeys, AppPasswords, NewUser, Users};

    /// The account password of the test account.
    const ACCOUNT_PASSWORD: &str = "hunter2";

    /// A migrated temporary database with one account and a resolver.
    struct Fixture {
        /// Keeps the temporary directory alive.
        _dir: tempfile::TempDir,
        /// The open database.
        db: Database,
        /// The account every credential belongs to.
        user_id: Uuid,
        /// The box the app passwords are sealed with.
        secret_box: SecretBox,
        /// The clock the rate limiter reads.
        clock: FakeRelativeClock,
        /// The resolver under test.
        authenticator: Authenticator<FakeRelativeClock>,
    }

    impl Fixture {
        /// Builds the fixture.
        ///
        /// # Parameters
        ///
        /// - `login_rate_limit_per_minute`: the limiter's starting rate.
        async fn new(login_rate_limit_per_minute: u32) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(&dir.path().join("hearthsound.db"))
                .await
                .unwrap();
            MIGRATOR.run(db.writer()).await.unwrap();
            let snapshot =
                SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default())
                    .unwrap();
            let user = Users::new(&db)
                .create(
                    &snapshot,
                    &NewUser::new("echo", "Echo", ACCOUNT_PASSWORD, false),
                )
                .await
                .unwrap();

            let secret_box = SecretKey::generate().secret_box();
            let clock = FakeRelativeClock::default();
            let authenticator = Authenticator::with_clock(
                &db,
                secret_box.clone(),
                login_rate_limit_per_minute,
                clock.clone(),
            );
            Fixture {
                _dir: dir,
                db,
                user_id: user.id,
                secret_box,
                clock,
                authenticator,
            }
        }

        /// Generates an app password through the service and returns its secret.
        async fn app_password(&self, name: &str) -> String {
            AppPasswords::new(&self.db, self.secret_box.clone())
                .generate(self.user_id, name)
                .await
                .unwrap()
                .plaintext
        }

        /// Inserts an app password with a chosen secret.
        ///
        /// Used by the token-authentication test, which checks a published MD5
        /// vector and therefore cannot use a randomly generated secret.
        async fn seed_app_password(&self, name: &str, plaintext: &str) -> Uuid {
            let (ciphertext, nonce) = self.secret_box.seal(plaintext.as_bytes()).unwrap();
            let id = new_uuid_v7();
            sqlx::query!(
                "INSERT INTO app_passwords (id, user_id, name, ciphertext, nonce, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
                id.as_bytes().as_slice(),
                self.user_id.as_bytes().as_slice(),
                name,
                ciphertext.as_slice(),
                nonce.as_slice(),
                crate::now_millis(),
            )
            .execute(self.db.writer())
            .await
            .unwrap();
            id
        }

        /// Returns a loopback address for the rate limiter.
        fn ip() -> IpAddr {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }

        /// Generates an API key and returns its plaintext.
        async fn api_key(&self) -> String {
            ApiKeys::new(&self.db)
                .generate(self.user_id, "Symfonium")
                .await
                .unwrap()
                .plaintext
        }
    }

    /// Resolves a settings snapshot with the two Subsonic switches.
    fn settings(allow_account_password: bool, token_auth: bool) -> SettingsStore {
        let mut sources = SettingSources::default();
        sources.env.insert(
            "HEARTHSOUND__SUBSONIC__ALLOW_ACCOUNT_PASSWORD".to_owned(),
            allow_account_password.to_string(),
        );
        sources.env.insert(
            "HEARTHSOUND__SUBSONIC__TOKEN_AUTH".to_owned(),
            token_auth.to_string(),
        );
        SettingsStore::resolve(&SettingsRegistry::builtin(), &sources).unwrap()
    }

    /// Builds credentials without an API key.
    fn credentials(
        username: Option<&str>,
        password: Option<&str>,
        token: Option<&str>,
        salt: Option<&str>,
    ) -> SubsonicCredentials {
        SubsonicCredentials {
            username: username.map(str::to_owned),
            password: password.map(str::to_owned),
            token: token.map(str::to_owned),
            salt: salt.map(str::to_owned),
            api_key: None,
        }
    }

    /// Unwraps the rejection of a resolve call.
    fn rejection(result: Result<Principal, ResolveError>) -> AuthenticationError {
        match result {
            Err(ResolveError::Rejected(error)) => error,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    /// Resolves a request that presents only an API key.
    async fn resolve_key(
        fixture: &Fixture,
        settings: &SettingsStore,
        key: &str,
    ) -> Result<Principal, ResolveError> {
        let credentials = SubsonicCredentials {
            api_key: Some(key.to_owned()),
            ..SubsonicCredentials::default()
        };
        fixture
            .authenticator
            .resolve(settings, Fixture::ip(), &credentials)
            .await
    }

    /// Resolves a request that presents the account password wrongly.
    async fn resolve_wrong_password(
        fixture: &Fixture,
        settings: &SettingsStore,
    ) -> Result<Principal, ResolveError> {
        let credentials = credentials(Some("echo"), Some("wrong"), None, None);
        fixture
            .authenticator
            .resolve(settings, Fixture::ip(), &credentials)
            .await
    }

    /// Inserts an app password with a chosen secret.

    #[tokio::test]
    async fn an_api_key_authenticates_its_owner() {
        let fixture = Fixture::new(10).await;
        let key = fixture.api_key().await;
        let credentials = SubsonicCredentials {
            api_key: Some(key),
            ..SubsonicCredentials::default()
        };

        let principal = fixture
            .authenticator
            .resolve(&settings(false, true), Fixture::ip(), &credentials)
            .await
            .unwrap();

        assert_eq!(principal.username, "echo");
        assert_eq!(principal.user_id.uuid(), fixture.user_id);
        assert!(matches!(principal.credential, Credential::ApiKey { .. }));
        assert!(principal.allows(Permission::Download));
        assert!(!principal.is_admin);
    }

    #[tokio::test]
    async fn an_api_key_with_a_username_is_a_conflict() {
        let fixture = Fixture::new(10).await;
        let credentials = SubsonicCredentials {
            api_key: Some(fixture.api_key().await),
            username: Some("echo".to_owned()),
            ..SubsonicCredentials::default()
        };

        let error = rejection(
            fixture
                .authenticator
                .resolve(&settings(false, true), Fixture::ip(), &credentials)
                .await,
        );
        assert_eq!(error, AuthenticationError::ApiKeyWithUsername);
        assert_eq!(error.code(), 43);
        assert_eq!(
            error.message(),
            "Multiple conflicting authentication mechanisms provided."
        );
        assert!(error.help_url().is_none());
    }

    #[tokio::test]
    async fn unknown_revoked_and_disabled_api_keys_are_invalid() {
        let fixture = Fixture::new(10).await;
        let settings = settings(false, true);
        for key in [
            "hs_0000000000000000000000000000000000000000000000000000",
            "nonsense",
        ] {
            let error = rejection(resolve_key(&fixture, &settings, key).await);
            assert_eq!(error, AuthenticationError::InvalidApiKey);
            assert_eq!(error.code(), 44);
            assert_eq!(error.message(), "Invalid API key.");
        }

        let revoked = fixture.api_key().await;
        let key_id = ApiKeys::new(&fixture.db)
            .list_for_user(fixture.user_id)
            .await
            .unwrap()
            .last()
            .unwrap()
            .id;
        assert!(ApiKeys::new(&fixture.db).revoke(key_id).await.unwrap());
        assert_eq!(
            rejection(resolve_key(&fixture, &settings, &revoked).await),
            AuthenticationError::InvalidApiKey
        );

        let disabled_user_key = fixture.api_key().await;
        assert!(
            Users::new(&fixture.db)
                .disable(fixture.user_id)
                .await
                .unwrap()
        );
        assert_eq!(
            rejection(resolve_key(&fixture, &settings, &disabled_user_key).await),
            AuthenticationError::InvalidApiKey
        );
    }

    #[tokio::test]
    async fn a_plain_app_password_authenticates() {
        let fixture = Fixture::new(10).await;
        let secret = fixture.app_password("Amperfy").await;

        let principal = fixture
            .authenticator
            .resolve(
                &settings(false, true),
                Fixture::ip(),
                &credentials(Some("echo"), Some(&secret), None, None),
            )
            .await
            .unwrap();

        assert!(matches!(
            principal.credential,
            Credential::AppPassword { .. }
        ));
        assert_eq!(principal.username, "echo");
    }

    #[tokio::test]
    async fn a_hex_encoded_app_password_authenticates() {
        let fixture = Fixture::new(10).await;
        let secret = fixture.app_password("Amperfy").await;
        let encoded = format!("enc:{}", data_encoding::HEXLOWER.encode(secret.as_bytes()));

        let principal = fixture
            .authenticator
            .resolve(
                &settings(false, true),
                Fixture::ip(),
                &credentials(Some("echo"), Some(&encoded), None, None),
            )
            .await
            .unwrap();

        assert!(matches!(
            principal.credential,
            Credential::AppPassword { .. }
        ));

        let uppercase = format!(
            "enc:{}",
            data_encoding::HEXLOWER
                .encode(secret.as_bytes())
                .to_uppercase()
        );
        assert!(
            fixture
                .authenticator
                .resolve(
                    &settings(false, true),
                    Fixture::ip(),
                    &credentials(Some("echo"), Some(&uppercase), None, None),
                )
                .await
                .is_ok(),
            "hex decoding accepts either case"
        );
    }

    #[tokio::test]
    async fn the_account_password_is_refused_with_a_help_url_by_default() {
        let fixture = Fixture::new(10).await;

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings(false, true),
                    Fixture::ip(),
                    &credentials(Some("echo"), Some(ACCOUNT_PASSWORD), None, None),
                )
                .await,
        );

        assert_eq!(error.code(), 40);
        assert_eq!(error.message(), "Wrong username or password.");
        assert_eq!(error.help_url(), Some(APP_PASSWORDS_HELP_URL));
    }

    #[tokio::test]
    async fn a_malformed_hex_password_follows_the_same_rule() {
        let fixture = Fixture::new(10).await;

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings(false, true),
                    Fixture::ip(),
                    &credentials(Some("echo"), Some("enc:zz"), None, None),
                )
                .await,
        );

        assert_eq!(error.code(), 40);
        assert_eq!(error.help_url(), Some(APP_PASSWORDS_HELP_URL));
    }

    #[tokio::test]
    async fn the_account_password_is_accepted_when_enabled() {
        let fixture = Fixture::new(10).await;

        let principal = fixture
            .authenticator
            .resolve(
                &settings(true, true),
                Fixture::ip(),
                &credentials(Some("echo"), Some(ACCOUNT_PASSWORD), None, None),
            )
            .await
            .unwrap();
        assert_eq!(principal.credential, Credential::AccountPassword);

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings(true, true),
                    Fixture::ip(),
                    &credentials(Some("echo"), Some("wrong"), None, None),
                )
                .await,
        );
        assert_eq!(error, AuthenticationError::WrongCredentials);
        assert!(error.help_url().is_none());
    }

    #[tokio::test]
    async fn token_auth_matches_a_published_md5_vector() {
        let fixture = Fixture::new(10).await;
        fixture.seed_app_password("Amperfy", "sesame").await;

        let principal = fixture
            .authenticator
            .resolve(
                &settings(false, true),
                Fixture::ip(),
                &credentials(
                    Some("echo"),
                    None,
                    Some("26719a1196d2a940705a59634eb18eab"),
                    Some("c19b2d"),
                ),
            )
            .await
            .unwrap();
        assert!(matches!(
            principal.credential,
            Credential::AppPassword { .. }
        ));

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings(false, true),
                    Fixture::ip(),
                    &credentials(
                        Some("echo"),
                        None,
                        Some("26719a1196d2a940705a59634eb18eac"),
                        Some("c19b2d"),
                    ),
                )
                .await,
        );
        assert_eq!(error, AuthenticationError::WrongCredentials);
    }

    #[tokio::test]
    async fn token_auth_requires_the_setting() {
        let fixture = Fixture::new(10).await;
        fixture.seed_app_password("Amperfy", "sesame").await;

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings(false, false),
                    Fixture::ip(),
                    &credentials(
                        Some("echo"),
                        None,
                        Some("26719a1196d2a940705a59634eb18eab"),
                        Some("c19b2d"),
                    ),
                )
                .await,
        );
        assert_eq!(error, AuthenticationError::TokenAuthDisabled);
        assert_eq!(error.code(), 41);
        assert_eq!(
            error.message(),
            "Token authentication not supported for LDAP users."
        );
    }

    #[tokio::test]
    async fn a_short_salt_is_rejected() {
        let fixture = Fixture::new(10).await;
        fixture.seed_app_password("Amperfy", "sesame").await;

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings(false, true),
                    Fixture::ip(),
                    &credentials(Some("echo"), None, Some("deadbeef"), Some("c19b2")),
                )
                .await,
        );
        assert_eq!(error.code(), 10);
        assert_eq!(error.message(), "Required parameter is missing.");
    }

    #[tokio::test]
    async fn missing_parameters_are_reported() {
        let fixture = Fixture::new(10).await;
        let settings = settings(false, true);

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings,
                    Fixture::ip(),
                    &credentials(None, None, None, None),
                )
                .await,
        );
        assert_eq!(
            error,
            AuthenticationError::MissingParameter { parameter: "u" }
        );

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings,
                    Fixture::ip(),
                    &credentials(Some("echo"), Some("secret"), None, Some("c19b2d")),
                )
                .await,
        );
        assert_eq!(
            error,
            AuthenticationError::MultipleMechanisms,
            "a password and a salt are two mechanisms"
        );

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings,
                    Fixture::ip(),
                    &credentials(Some("echo"), None, None, Some("c19b2d")),
                )
                .await,
        );
        assert_eq!(
            error,
            AuthenticationError::MissingParameter { parameter: "t" }
        );
    }

    #[tokio::test]
    async fn two_mechanisms_are_rejected() {
        let fixture = Fixture::new(10).await;
        let settings = settings(false, true);

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings,
                    Fixture::ip(),
                    &credentials(
                        Some("echo"),
                        Some("secret"),
                        Some("26719a1196d2a940705a59634eb18eab"),
                        Some("c19b2d"),
                    ),
                )
                .await,
        );
        assert_eq!(error, AuthenticationError::MultipleMechanisms);
        assert_eq!(error.code(), 42);

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings,
                    Fixture::ip(),
                    &SubsonicCredentials {
                        api_key: Some("hs_x".to_owned()),
                        password: Some("secret".to_owned()),
                        ..SubsonicCredentials::default()
                    },
                )
                .await,
        );
        assert_eq!(error, AuthenticationError::MultipleMechanisms);
    }

    #[tokio::test]
    async fn disabled_users_fail_like_unknown_ones() {
        let fixture = Fixture::new(10).await;
        let secret = fixture.app_password("Amperfy").await;
        assert!(
            Users::new(&fixture.db)
                .disable(fixture.user_id)
                .await
                .unwrap()
        );

        let error = rejection(
            fixture
                .authenticator
                .resolve(
                    &settings(true, true),
                    Fixture::ip(),
                    &credentials(Some("echo"), Some(&secret), None, None),
                )
                .await,
        );
        assert_eq!(error, AuthenticationError::WrongCredentials);
    }

    #[tokio::test]
    async fn login_attempts_are_rate_limited_and_recover() {
        let fixture = Fixture::new(2).await;
        let settings = settings(true, true);
        assert_eq!(
            rejection(resolve_wrong_password(&fixture, &settings).await),
            AuthenticationError::WrongCredentials
        );
        assert_eq!(
            rejection(resolve_wrong_password(&fixture, &settings).await),
            AuthenticationError::WrongCredentials
        );
        let limited = rejection(resolve_wrong_password(&fixture, &settings).await);
        assert_eq!(limited, AuthenticationError::TooManyAttempts);
        assert_eq!(
            limited.code(),
            40,
            "the base protocol has no rate limit code"
        );
        assert_eq!(
            limited.message(),
            "Too many login attempts; try again later."
        );

        fixture.clock.advance(Duration::from_secs(60));

        assert_eq!(
            rejection(resolve_wrong_password(&fixture, &settings).await),
            AuthenticationError::WrongCredentials,
            "the budget refills after a minute"
        );
    }

    #[tokio::test]
    async fn successful_logins_do_not_spend_the_budget() {
        let fixture = Fixture::new(2).await;
        let settings = settings(false, true);
        let secret = fixture.app_password("Gelly").await;

        for attempt in 0..20 {
            let salt = format!("salt{attempt:04}");
            let token = data_encoding::HEXLOWER.encode(&Md5::digest(format!("{secret}{salt}")));
            let credentials = SubsonicCredentials {
                username: Some("echo".to_owned()),
                token: Some(token),
                salt: Some(salt),
                ..SubsonicCredentials::default()
            };
            assert!(
                fixture
                    .authenticator
                    .resolve(&settings, Fixture::ip(), &credentials)
                    .await
                    .is_ok(),
                "token login {attempt} is accepted"
            );
        }
    }

    #[tokio::test]
    async fn a_spent_budget_refuses_even_the_right_secret() {
        let fixture = Fixture::new(2).await;
        let settings = settings(false, true);
        let secret = fixture.app_password("Feishin").await;
        let password = |value: &str| SubsonicCredentials {
            username: Some("echo".to_owned()),
            password: Some(value.to_owned()),
            ..SubsonicCredentials::default()
        };

        for _ in 0..2 {
            assert!(
                fixture
                    .authenticator
                    .resolve(&settings, Fixture::ip(), &password("guess"))
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            rejection(
                fixture
                    .authenticator
                    .resolve(&settings, Fixture::ip(), &password(&secret))
                    .await
            ),
            AuthenticationError::TooManyAttempts,
            "a guess after the budget is spent learns nothing"
        );

        fixture.clock.advance(Duration::from_secs(30));
        assert!(
            fixture
                .authenticator
                .resolve(&settings, Fixture::ip(), &password(&secret))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn the_api_key_path_is_not_rate_limited() {
        let fixture = Fixture::new(1).await;
        let settings = settings(false, true);
        let key = fixture.api_key().await;
        let credentials = SubsonicCredentials {
            api_key: Some(key),
            ..SubsonicCredentials::default()
        };

        for _ in 0..3 {
            assert!(
                fixture
                    .authenticator
                    .resolve(&settings, Fixture::ip(), &credentials)
                    .await
                    .is_ok()
            );
        }
    }
}
