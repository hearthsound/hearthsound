//! `hearthsound acquisitions`: list acquisitions and cancel them.
//!
//! The command reads and writes the ledger directly, through the acquisition
//! crate's repository, exactly like the other administrative commands: a
//! cancellation is a row the server notices through `PRAGMA data_version`, and
//! it is the running server's `reconcile` that stops the worker, removes the
//! acquisition's staging directory and returns its track to `remote`. A
//! cancelled row therefore takes effect at once, and the side effects follow
//! within the server's data-version poll.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};
use hearthsound_acquire::repo::{AcquisitionRepository, AcquisitionSummary, ListFilter};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_core::{AcquisitionState, ClientId, IdPrefix};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::cli::{
    AcquisitionsArgs, AcquisitionsCancelArgs, AcquisitionsCommand, AcquisitionsListArgs,
};
use crate::context::Context;

/// Runs one `acquisitions` command.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
/// - `env`: the process environment, keyed by variable name.
/// - `args`: the subcommand to run.
///
/// # Errors
///
/// Returns an error when the context cannot be opened or the subcommand fails.
pub async fn run(
    config: BootstrapConfig,
    env: &BTreeMap<String, String>,
    args: AcquisitionsArgs,
) -> anyhow::Result<()> {
    let context = Context::open(config, env).await?;
    match args.command {
        AcquisitionsCommand::List(args) => list(&context, args).await,
        AcquisitionsCommand::Cancel(args) => cancel(&context, args).await,
    }
}

/// Lists acquisitions, one per line.
///
/// Prints the client id, the state, the priority, the track, the instance
/// serving the acquisition, the download percentage, the failure reason and the
/// creation time, separated by two spaces; a field the ledger does not hold is
/// printed as `-`.
///
/// # Parameters
///
/// - `context`: the open database.
/// - `args`: the state filter and whether ended acquisitions are included.
///
/// # Errors
///
/// Returns an error when the state is not one of the acquisition states, or when
/// the ledger cannot be read.
async fn list(context: &Context, args: AcquisitionsListArgs) -> anyhow::Result<()> {
    let state = match args.state.as_deref() {
        Some(text) => Some(
            text.parse::<AcquisitionState>()
                .with_context(|| format!("`{text}` is not an acquisition state"))?,
        ),
        None => None,
    };
    let filter = ListFilter {
        state,
        active: !args.all,
        limit: None,
    };
    let rows = AcquisitionRepository::new(context.database())
        .list(&filter)
        .await
        .context("could not list the acquisitions")?;
    if rows.is_empty() {
        println!("no acquisitions");
        return Ok(());
    }
    for row in &rows {
        println!(
            "{}  {}  {}  {}  {}  {}  {}  {}",
            row.client_id,
            row.state.as_str(),
            row.priority.as_str(),
            track_of(row),
            row.instance.as_deref().unwrap_or("-"),
            percent_of(row),
            row.failure_reason
                .map(|reason| reason.as_str())
                .unwrap_or("-"),
            created_at(row.created_at),
        );
    }
    Ok(())
}

/// Cancels one acquisition.
///
/// Only the ledger row is written here: the server that is running the
/// acquisition notices the write and stops it. A row that has already ended is
/// left alone, so cancelling twice is not an error.
///
/// # Parameters
///
/// - `context`: the open database.
/// - `args`: the acquisition's client id.
///
/// # Errors
///
/// Returns an error when the value is not an acquisition id, no acquisition has
/// that id, or the write fails.
async fn cancel(context: &Context, args: AcquisitionsCancelArgs) -> anyhow::Result<()> {
    let id = acquisition_id(&args.acquisition)?;
    let cancelled = AcquisitionRepository::new(context.database())
        .cancel(id, Some("cancelled from the command line"))
        .await
        .with_context(|| format!("could not cancel `{}`", args.acquisition))?;
    if cancelled {
        println!("cancelled {}", args.acquisition);
    } else {
        println!("{} had already ended", args.acquisition);
    }
    Ok(())
}

/// Parses an acquisition's client id into its primary key.
///
/// # Parameters
///
/// - `text`: the id a client would use, `<prefix>_<payload>`.
///
/// # Returns
///
/// The acquisition's primary key.
///
/// # Errors
///
/// Returns an error when the value is not a well-formed client id, or is a
/// client id of another kind.
fn acquisition_id(text: &str) -> anyhow::Result<Uuid> {
    let client = ClientId::parse(text).with_context(|| format!("`{text}` is not a client id"))?;
    match client {
        ClientId::Entity(id) if id.kind() == IdPrefix::Acquisition => Ok(id.uuid()),
        _ => bail!("`{text}` is not an acquisition id"),
    }
}

/// Renders the entity an acquisition targets.
///
/// # Parameters
///
/// - `row`: the listed acquisition.
///
/// # Returns
///
/// `<artist> - <title>` for the target the row names - its track or its album -
/// the one field that is known, or `-`.
fn track_of(row: &AcquisitionSummary) -> String {
    let artist = row.track_artist.as_deref().or(row.album_artist.as_deref());
    let title = row.track_title.as_deref().or(row.album_title.as_deref());
    match (artist, title) {
        (Some(artist), Some(title)) => format!("{artist} - {title}"),
        (None, Some(title)) => title.to_owned(),
        (Some(artist), None) => artist.to_owned(),
        (None, None) => "-".to_owned(),
    }
}

/// Renders the download progress.
///
/// # Parameters
///
/// - `row`: the listed acquisition.
///
/// # Returns
///
/// The percentage, or `-` when the last progress snapshot does not allow one.
fn percent_of(row: &AcquisitionSummary) -> String {
    row.percent
        .map(|percent| format!("{percent}%"))
        .unwrap_or_else(|| "-".to_owned())
}

/// Renders a timestamp.
///
/// # Parameters
///
/// - `millis`: the instant in unix milliseconds.
///
/// # Returns
///
/// The instant in RFC 3339 UTC, or the raw value when it is out of range.
fn created_at(millis: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .ok()
        .and_then(|instant| instant.format(&Rfc3339).ok())
        .unwrap_or_else(|| millis.to_string())
}
