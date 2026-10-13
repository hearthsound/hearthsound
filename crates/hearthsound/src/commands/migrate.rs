//! `hearthsound migrate`: apply pending migrations and report the versions.

use hearthsound_config::bootstrap::BootstrapConfig;

use crate::context;

/// Applies every pending migration.
///
/// The database is opened and migrated exactly as `serve` would migrate it,
/// including the `VACUUM INTO` backup into `<data>/backups` that a database
/// holding data gets before anything is applied, and the command prints the
/// version it started at, the version it ended at, and the backup path.
///
/// Nothing else is opened: `migrate` has to work on a database whose stored
/// settings no longer resolve, because that is one of the states it is used to
/// recover from.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
///
/// # Errors
///
/// Returns an error when the directory tree cannot be created or the database
/// cannot be opened or migrated. A failed migration leaves the backup in place
/// as the recovery point.
pub async fn run(config: BootstrapConfig) -> anyhow::Result<()> {
    let (database, outcome) = context::open_database(&config).await?;

    match &outcome.backup {
        Some(backup) => println!("backed up the database to {}", backup.display()),
        None => println!("no backup was needed"),
    }
    if outcome.to == outcome.from {
        println!("the database is already at version {}", outcome.to);
    } else {
        println!(
            "migrated the database from version {} to {}",
            outcome.from, outcome.to
        );
    }
    database.close().await;
    Ok(())
}
