//! `hearthsound scan`: index the tags of the library folders' files.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_core::events::EventBus;
use hearthsound_library::scan::{LibraryScanner, ScanReport, ScanRequest};
use hearthsound_library::{LibraryFolder, LibraryRepository};

use crate::cli::ScanArgs;
use crate::context;

/// Scans one folder or every folder and prints what the scan did.
///
/// The scanner runs directly rather than through the job queue: an operator who
/// typed `scan` is waiting for the result, and a job would only add a queue row
/// and a worker wake-up. The scan is idempotent, so running it again after an
/// interruption is safe.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
/// - `env`: the process environment, keyed by variable name, for the
///   environment layer of the runtime settings.
/// - `args`: the folder to scan, or `None` for every folder.
///
/// # Errors
///
/// Returns an error when the database cannot be opened, `--folder` names
/// neither a folder id nor a folder path, or a scan fails.
pub async fn run(
    config: BootstrapConfig,
    env: &BTreeMap<String, String>,
    args: ScanArgs,
) -> anyhow::Result<()> {
    let (database, _outcome) = context::open_database(&config).await?;
    let settings = context::load_settings(&config, env, &database).await?;
    let scanner = LibraryScanner::new(&database, Arc::new(EventBus::new()), settings);
    let mut folders = LibraryRepository::new(&database)
        .list_library_folders()
        .await
        .context("could not list the library folders")?;

    let request = match &args.folder {
        None => ScanRequest::all(),
        Some(selector) => {
            let folder_id = resolve_folder(&folders, selector)?.id;
            folders.retain(|candidate| candidate.id == folder_id);
            ScanRequest::folder(folder_id)
        }
    };
    let request = if args.full { request.full() } else { request };

    let reports = scanner
        .scan(&request)
        .await
        .context("could not scan the library folders")?;

    if reports.is_empty() {
        println!("no library folders are configured");
        return Ok(());
    }
    for report in &reports {
        print_report(report, folder_name(&folders, report.folder_id));
    }
    let indexed: u64 = reports.iter().map(|report| report.files_indexed).sum();
    println!("indexed {indexed} files");
    Ok(())
}

/// Resolves the `--folder` value into a library folder.
///
/// # Parameters
///
/// - `folders`: the configured folders.
/// - `selector`: a folder id or the path of a folder. A path is canonicalized
///   first, so a relative or symlinked spelling of a configured folder resolves
///   to it.
///
/// # Returns
///
/// The folder.
///
/// # Errors
///
/// Returns an error when the value is neither a known folder id nor a path that
/// canonicalizes to a configured folder.
pub(crate) fn resolve_folder<'a>(
    folders: &'a [LibraryFolder],
    selector: &str,
) -> anyhow::Result<&'a LibraryFolder> {
    if let Ok(id) = selector.parse::<uuid::Uuid>() {
        return folders
            .iter()
            .find(|folder| folder.id == id)
            .with_context(|| format!("no library folder has the id {selector}"));
    }
    let path = Path::new(selector);
    let canonical = path
        .canonicalize()
        .with_context(|| format!("could not resolve {}", path.display()))?;
    folders
        .iter()
        .find(|folder| folder.path == canonical)
        .with_context(|| {
            format!(
                "{} is not a library folder; add it with `admin add-folder` first",
                canonical.display()
            )
        })
}

/// Returns a folder's display name and path, or its id when it is unknown.
///
/// # Parameters
///
/// - `folders`: the configured folders.
/// - `id`: the folder the report belongs to.
fn folder_name(folders: &[LibraryFolder], id: uuid::Uuid) -> String {
    folders
        .iter()
        .find(|folder| folder.id == id)
        .map(|folder| format!("{} ({})", folder.name, folder.path.display()))
        .unwrap_or_else(|| id.to_string())
}

/// Prints one folder's report.
///
/// # Parameters
///
/// - `report`: the counts of one folder scan.
/// - `folder`: the folder's name and path as configured.
fn print_report(report: &ScanReport, folder: String) {
    println!(
        "{folder}: {} seen, {} indexed, {} skipped, {} failed, {} unreadable, {} removed, \
         {} tracks removed",
        report.files_seen,
        report.files_indexed,
        report.files_skipped,
        report.files_failed,
        report.unreadable,
        report.files_removed,
        report.tracks_removed,
    );
    if let Some(problem) = report.problem() {
        println!("{folder}: {problem}");
    }
}
