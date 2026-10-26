//! What a command opens before it does its work.
//!
//! Every command that touches the database opens the data directory the same
//! way the server does: create the directory tree, open the database, and apply
//! the pending migrations, after the `VACUUM INTO` backup a database holding
//! data gets. A command therefore works on a fresh installation without a prior
//! `migrate` or `serve`.
//!
//! A command that would migrate the schema first takes the server's lock on the
//! data directory, so a newer binary cannot move the schema under a running
//! server (the server holds `<data>/server.lock` for its lifetime). A database a
//! newer binary migrated is refused outright: migrations are forward-only, so
//! the binary has to be upgraded. A command that needs no migration at all takes
//! no lock, which is why the read-only commands work beside a running server.
//!
//! [`Context`] adds the two things a command needs to write rows that the server
//! can read back: the runtime settings (a new account's permissions come from
//! `users.default_permissions`) and the secret box (`create-app-password` seals
//! what it generates). It deliberately stops there; the plugin host is only
//! built by the commands that need wasmtime.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingsRegistry};
use hearthsound_db::{Database, MIGRATOR, MigrationOutcome};
use hearthsound_library::folders::ServerPaths;
use hearthsound_plugin_host::instances::InstanceService;
use hearthsound_plugin_host::{
    IndexClient, PluginAdmin, PluginHost, PluginInstaller, PluginRepository,
};

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

    let applied = database
        .migration_version()
        .await
        .context("could not read the schema version")?;
    let bundled = bundled_migration_version();
    match schema_state(applied, bundled) {
        // Nothing to do: no lock is taken, so a read-only command keeps working
        // beside a running server.
        SchemaState::Current => Ok((
            database,
            MigrationOutcome {
                from: applied,
                to: applied,
                backup: None,
            },
        )),
        SchemaState::Newer => Err(anyhow::anyhow!(
            "the database was migrated by a newer Hearthsound (schema {applied}, this binary \
             bundles {bundled}); upgrade this binary"
        )),
        SchemaState::Older => {
            let lock_path = hearthsound_server::serve_lock_path(&config.paths.data);
            let Some(lock) = hearthsound_server::ServeLock::try_acquire(&config.paths.data)
                .context("could not take the data directory lock")?
            else {
                return Err(anyhow::anyhow!(
                    "a server is running (it holds {}); stop it before this command migrates \
                     the database",
                    lock_path.display()
                ));
            };
            let outcome = database
                .migrate(&MIGRATOR, &config.paths.backups)
                .await
                .context("could not migrate the database")?;
            if outcome.to != outcome.from {
                tracing::info!(from = outcome.from, to = outcome.to, "applied migrations");
            }
            drop(lock);
            Ok((database, outcome))
        }
    }
}

/// What a database's schema version says about a command's work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaState {
    /// The database is at the bundled version: nothing to migrate.
    Current,
    /// A newer binary migrated the database past what this one bundles.
    Newer,
    /// The database is behind the bundled version, so a command would migrate.
    Older,
}

/// Compares the applied schema version with the bundled one.
///
/// # Parameters
///
/// - `applied`: the version the database is at.
/// - `bundled`: the highest version the binary ships.
///
/// # Returns
///
/// Which of the three states the pair is in.
fn schema_state(applied: i64, bundled: i64) -> SchemaState {
    match applied.cmp(&bundled) {
        std::cmp::Ordering::Equal => SchemaState::Current,
        std::cmp::Ordering::Greater => SchemaState::Newer,
        std::cmp::Ordering::Less => SchemaState::Older,
    }
}

/// The schema version the bundled migrations bring a database to.
///
/// # Returns
///
/// The highest version in [`MIGRATOR`], or zero when the migrator is empty.
fn bundled_migration_version() -> i64 {
    MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}

/// Loads the runtime settings of one command.
///
/// Unlike [`Context::open`] this builds no secret box and no plugin host, so a
/// command that only reads a setting does not depend on the key file.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
/// - `env`: the process environment, keyed by variable name, for the
///   environment layer of the runtime settings.
/// - `database`: the open, migrated database.
///
/// # Returns
///
/// The settings service, whose current snapshot is already resolved.
///
/// # Errors
///
/// Returns an error when a stored setting cannot be read or resolved.
pub async fn load_settings(
    config: &BootstrapConfig,
    env: &BTreeMap<String, String>,
    database: &Database,
) -> anyhow::Result<Settings> {
    Settings::load(
        database.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::new(env.clone(), config.settings.clone()),
    )
    .await
    .context("could not load the runtime settings")
}

/// The database, settings, and secret key of one data-writing command.
pub struct Context {
    /// The resolved bootstrap configuration.
    config: BootstrapConfig,
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
        let settings = load_settings(&config, env, &database).await?;
        let secret_box = SecretKey::load(&config.secrets.key_file, env)
            .context("could not load the secret key")?
            .secret_box();

        Ok(Context {
            config,
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

    /// Returns the server's own files and directories.
    ///
    /// A library folder may not be one of them, contain one, or sit inside one,
    /// so the folder commands check a path against them before storing it.
    pub fn server_paths(&self) -> ServerPaths {
        ServerPaths {
            data: self.config.paths.data.clone(),
            storage: self.config.paths.storage.clone(),
            database: self.config.database.path.clone(),
        }
    }

    /// Returns the directory the installed plugin trees live under.
    ///
    /// Each plugin's files sit in `<plugins_dir>/<id>/<version>`, and removing
    /// a plugin removes `<plugins_dir>/<id>`.
    pub fn plugins_dir(&self) -> &Path {
        &self.config.paths.plugins
    }

    /// Returns the secret box of the secret key file.
    pub fn secret_box(&self) -> &SecretBox {
        &self.secret_box
    }

    /// Builds a plugin host over the data directory's plugin tree.
    ///
    /// The host is built on demand because only the plugin and provider commands
    /// need the wasmtime engine. It is not shared with a running server: a
    /// command-line process has its own engine, and the two only meet in the
    /// database and the installed plugin files.
    ///
    /// # Returns
    ///
    /// The host, rooted at `<data>/plugins`, using the settings snapshot loaded
    /// by [`Context::open`] to validate manifests.
    ///
    /// # Errors
    ///
    /// Returns an error when the wasmtime engine or the host linker cannot be
    /// built.
    pub fn plugin_host(&self) -> anyhow::Result<Arc<PluginHost>> {
        let host = PluginHost::new(
            self.config.paths.plugins.clone(),
            self.settings.current(),
            Arc::clone(&self.database),
        )
        .context("could not build the plugin host")?;
        Ok(Arc::new(host))
    }

    /// Builds the administrative plugin service over the data directory's
    /// plugin tree.
    ///
    /// The service is given no event bus: a command-line process has no
    /// subscriber, and the running server notices the writes through its
    /// data-version watcher.
    ///
    /// # Returns
    ///
    /// The service, whose plugin tables live in this context's database.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Context::plugin_host`].
    pub fn plugin_admin(&self) -> anyhow::Result<Arc<PluginAdmin>> {
        Ok(Arc::new(PluginAdmin::new(
            self.plugin_host()?,
            PluginRepository::new(&self.database),
            None,
        )))
    }

    /// Builds the plugin installer over the data directory's plugin tree.
    ///
    /// The installer reads index documents and downloads archives over its own
    /// HTTP client, so installing a plugin from an index needs no running
    /// server. It is bound to the same compiled-in official index URL and
    /// public key as the server, and staged downloads go to
    /// [`Context::plugins_dir`].
    ///
    /// # Returns
    ///
    /// The installer.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Context::plugin_admin`], a failure to build the
    /// HTTP client, and a failure to read the compiled-in official public key.
    pub fn installer(&self) -> anyhow::Result<PluginInstaller> {
        // The workspace builds reqwest without a bundled TLS provider, so ring
        // is installed as the process default before the first client is built;
        // installing it again is harmless, which is why the server does the
        // same.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(format!("Hearthsound/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not build the HTTP client")?;
        Ok(PluginInstaller::new(
            self.plugin_admin()?,
            PluginRepository::new(&self.database),
            IndexClient::official(http).context("could not read the official plugin index key")?,
            self.plugins_dir().to_path_buf(),
        ))
    }

    /// Builds the provider-instance service over a fresh plugin host.
    ///
    /// # Returns
    ///
    /// The service, whose plugin tables live in this context's database and
    /// whose components load from `<data>/plugins`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Context::plugin_host`].
    pub fn instances(&self) -> anyhow::Result<InstanceService> {
        Ok(InstanceService::new(
            self.plugin_host()?,
            PluginRepository::new(&self.database),
            self.secret_box.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_state_names_which_side_is_ahead() {
        assert_eq!(schema_state(17, 17), SchemaState::Current);
        assert_eq!(schema_state(16, 17), SchemaState::Older);
        assert_eq!(schema_state(18, 17), SchemaState::Newer);
    }
}
