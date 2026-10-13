//! Opening the database every data-touching command works against.
//!
//! The command-line commands write the database directly rather than talking to
//! a running server, so each one opens the data directory the same way the
//! server does: create the directory tree, open the database, and apply the
//! pending migrations (after the `VACUUM INTO` backup a database holding data
//! gets). A command therefore works on a fresh installation without a prior
//! `migrate` or `serve`.

use anyhow::Context as _;
use hearthsound_config::bootstrap::BootstrapConfig;
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
