//! What a command opens before it does its work.
//!
//! Every command that touches the database opens the data directory the same
//! way the server does: create the directory tree, open the database, and apply
//! the pending migrations, after the `VACUUM INTO` backup a database holding
//! data gets. A command therefore works on a fresh installation without a prior
//! `migrate` or `serve`.
//!
//! [`Context`] adds the two things a command needs to write rows that the server
//! can read back: the runtime settings (a new account's permissions come from
//! `users.default_permissions`) and the secret box (`create-app-password` seals
//! what it generates). It deliberately stops there; the plugin host is only
//! built by the commands that need wasmtime.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context as _;
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingsRegistry};
use hearthsound_db::{Database, MIGRATOR, MigrationOutcome};

/// Creates the directory tree, opens the database, and migrates it.
///
/// The migration is logged when it applied anything, so a command that does not
/// print the versions still records that the schema moved.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
///
/// # Returns
///
/// The open database and what the migration did, which `migrate` prints and
/// every other command ignores.
///
/// # Errors
///
/// Returns an error when a directory cannot be created, the database cannot be
/// opened, or a migration fails. A failed migration leaves the backup in place
/// as the recovery point.
pub async fn open_database(
    config: &BootstrapConfig,
) -> anyhow::Result<(Database, MigrationOutcome)> {
    hearthsound_server::paths::prepare(config)
        .context("could not prepare the data and storage directories")?;

    let database = Database::open(&config.database.path)
        .await
        .with_context(|| format!("could not open {}", config.database.path.display()))?;
    let outcome = database
        .migrate(&MIGRATOR, &config.paths.backups)
        .await
        .context("could not migrate the database")?;
    if outcome.to != outcome.from {
        tracing::info!(from = outcome.from, to = outcome.to, "applied migrations");
    }

    Ok((database, outcome))
}

/// The database, settings, and secret key of one data-writing command.
pub struct Context {
    /// The open, migrated database.
    database: Arc<Database>,
    /// The runtime settings service.
    settings: Settings,
    /// The box that seals app passwords and write-only plugin configuration.
    secret_box: SecretBox,
}

impl Context {
    /// Opens everything a data-writing command needs.
    ///
    /// # Parameters
    ///
    /// - `config`: the resolved bootstrap configuration.
    /// - `env`: the process environment, keyed by variable name, for the
    ///   environment layer of the runtime settings and for
    ///   `HEARTHSOUND_SECRET_KEY`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`open_database`], plus a failure to resolve a
    /// stored setting and a failure to read or create the secret key.
    pub async fn open(
        config: BootstrapConfig,
        env: &BTreeMap<String, String>,
    ) -> anyhow::Result<Context> {
        let (database, _outcome) = open_database(&config).await?;
        let settings = Settings::load(
            database.writer().clone(),
            SettingsRegistry::builtin(),
            SettingSources::new(env.clone(), config.settings.clone()),
        )
        .await
        .context("could not load the runtime settings")?;
        let secret_box = SecretKey::load(&config.secrets.key_file, env)
            .context("could not load the secret key")?
            .secret_box();

        Ok(Context {
            database: Arc::new(database),
            settings,
            secret_box,
        })
    }

    /// Returns the open database.
    pub fn database(&self) -> &Arc<Database> {
        &self.database
    }

    /// Returns the runtime settings service.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Returns the secret box of the secret key file.
    pub fn secret_box(&self) -> &SecretBox {
        &self.secret_box
    }
}
