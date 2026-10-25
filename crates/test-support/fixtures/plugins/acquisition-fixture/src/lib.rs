//! Acquisition fixture: a downloader simulated entirely through the host.
//!
//! The plugin keeps every piece of state in the key-value store, so a search or
//! a download survives its instance being recycled, and it discovers the files a
//! download produced through the file import: when a download completes, it
//! lists the configured output root and reports every file it finds there.
//!
//! The server configuration decides what happens:
//!
//! - `candidates` are the results a search returns;
//! - `searchPolls` polls answer `running` before the search completes, and
//!   `downloadQueuedPolls` plus `downloadRunningPolls` polls answer `queued`
//!   and `running` before the download completes;
//! - `outputRoot` is the root the completed download lists, `staging` by
//!   default and an import-root id when a backend downloads into one;
//! - `scenario` is `normal`, `no-candidates` (an empty search),
//!   `download-failure` (a download the backend reports as failed),
//!   `stalled` (a download that stops making progress and never finishes),
//!   `trickle` (a download that advances one byte per poll and never
//!   completes) or `unavailable` (every call answers that the backend is not
//!   answering);
//! - `pollFailures` is how many polls of a download answer an error before the
//!   backend answers normally;
//! - `lostToken` makes the first poll of a download report that the token is
//!   unknown, which is what a lost transfer looks like.
//!
//! Polling a token the plugin never issued reports `not-found`.

use hearthsound_plugin::acquisition_provider::exports::hearthsound::plugin::acquisition::{
    self, Candidate, CandidateFile, Capabilities, DownloadState, Granularity, OutputFile, Progress,
    Query, SearchState, SourceInfo,
};
use hearthsound_plugin::acquisition_provider::exports::hearthsound::plugin::lifecycle;
use hearthsound_plugin::host::files;
use hearthsound_plugin::host::kv as host_kv;
use hearthsound_plugin::kv;
use hearthsound_plugin::types::ProviderError;
use serde::{Deserialize, Serialize};

/// The plugin implementation.
struct AcquisitionFixture;

/// Server-scoped configuration of the plugin.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Config {
    /// The candidates a track search returns.
    candidates: Vec<CandidateRecord>,
    /// The folders an album search returns.
    album_candidates: Vec<AlbumCandidateRecord>,
    /// How many polls answer `running` before a search completes.
    search_polls: u32,
    /// How many polls answer `queued` before a download starts running.
    download_queued_polls: u32,
    /// How many polls answer `running` before a download completes.
    download_running_polls: u32,
    /// How many bytes each `running` poll reports.
    download_bytes_per_poll: u64,
    /// The root a completed download lists for its output files.
    output_root: String,
    /// The suffix an output file must have to be reported.
    match_suffix: Option<String>,
    /// What the simulated backend does.
    scenario: Scenario,
    /// How many polls of a download answer an error before the backend
    /// answers normally.
    poll_failures: u32,
    /// Whether the first poll of a download reports an unknown token.
    lost_token: bool,
    /// Whether the provider can deliver bytes while downloading.
    progressive: bool,
    /// The granularities the provider claims.
    granularities: Vec<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            candidates: Vec::new(),
            album_candidates: Vec::new(),
            search_polls: 1,
            download_queued_polls: 1,
            download_running_polls: 2,
            download_bytes_per_poll: 5000,
            output_root: "staging".to_string(),
            match_suffix: None,
            scenario: Scenario::Normal,
            poll_failures: 0,
            lost_token: false,
            progressive: false,
            granularities: vec!["track".to_string()],
        }
    }
}

/// What the simulated backend does.
#[derive(Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Scenario {
    /// Searches find candidates and downloads complete with files.
    #[default]
    Normal,
    /// Searches complete with no candidates.
    NoCandidates,
    /// Downloads report a failure once they would complete.
    DownloadFailure,
    /// Downloads stop making progress and never finish.
    Stalled,
    /// Downloads advance one byte per poll and never finish.
    Trickle,
    /// Every call answers that the backend is not answering.
    Unavailable,
}

/// One configured candidate, and its persisted form.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandidateRecord {
    /// The token that addresses the candidate in a download.
    token: String,
    /// The candidate's title.
    title: String,
    /// The backend's path for the candidate.
    path: String,
    /// The files the candidate offers.
    #[serde(default)]
    files: Vec<FileRecord>,
    /// What the backend knows about the source.
    source: SourceRecord,
}

/// One configured album folder, and its persisted form.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumCandidateRecord {
    /// The folder the download writes its files under.
    folder: String,
    /// The files the folder offers.
    #[serde(default)]
    files: Vec<FileRecord>,
    /// What the backend knows about the source.
    source: SourceRecord,
}

/// One configured candidate file, and its persisted form.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileRecord {
    /// The file name.
    name: String,
    /// The file size in bytes.
    #[serde(default)]
    size: Option<u64>,
    /// The container format.
    #[serde(default)]
    format: Option<String>,
    /// The bitrate in kbps.
    #[serde(default)]
    bitrate_kbps: Option<u32>,
    /// The sample rate in Hz.
    #[serde(default)]
    sample_rate: Option<u32>,
    /// The bit depth.
    #[serde(default)]
    bit_depth: Option<u8>,
    /// The duration in milliseconds.
    #[serde(default)]
    duration_ms: Option<u32>,
    /// The track the file carries.
    #[serde(default)]
    track_index: Option<u32>,
}

/// What the backend knows about a source, and its persisted form.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceRecord {
    /// The backend's identifier for the source.
    key: String,
    /// The source's upload speed in bytes per second.
    #[serde(default)]
    speed_bps: Option<u64>,
    /// The source's queue length.
    #[serde(default)]
    queue_length: Option<u32>,
    /// Whether the source has a free upload slot.
    #[serde(default)]
    free_slot: Option<bool>,
}

/// The persisted state of a running search.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchRecord {
    /// The candidates the search found.
    candidates: Vec<CandidateRecord>,
    /// The polls still to answer `running` before the search completes.
    polls_left: u32,
}

/// The persisted state of a running download.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRecord {
    /// The polls still to answer `queued`.
    queued_left: u32,
    /// The polls still to answer `running`.
    running_left: u32,
    /// The total number of running polls, used to report progress.
    running_polls: u32,
    /// The bytes each running poll reports.
    bytes_per_poll: u64,
    /// The total size the progress reports.
    total_bytes: u64,
    /// Whether the download reports a failure instead of completing.
    failed: bool,
    /// Whether the download stalls instead of progressing.
    stalled: bool,
    /// Whether the download advances one byte per poll and never finishes.
    trickle: bool,
    /// How many polls still answer an error.
    poll_errors: u32,
    /// Whether the first poll reports an unknown token.
    lost_first: bool,
    /// Whether that first poll was already answered.
    lost_reported: bool,
    /// The root the completed download lists.
    output_root: String,
    /// The suffix a reported output file must have.
    match_suffix: Option<String>,
    /// The album folder the download writes into, when it is an album folder.
    folder: Option<String>,
    /// The positions of the candidate's files the download was asked for.
    #[serde(default)]
    requested_files: Vec<u32>,
}

/// Returns the key a search's state is stored under.
fn search_key(token: &str) -> String {
    format!("search:{token}")
}

/// Returns the key a download's state is stored under.
fn download_key(token: &str) -> String {
    format!("download:{token}")
}

/// Loads the plugin's server-scoped configuration.
fn config() -> Result<Config, ProviderError> {
    hearthsound_plugin::config::server_config()
}

/// Returns the next token of `kind`, counting through the key-value store.
fn next_token(kind: &str) -> Result<String, ProviderError> {
    let key = format!("counter:{kind}");
    let current: u64 = kv::get_json(&key)?.unwrap_or(0);
    let next = current + 1;
    kv::set_json(&key, &next, None)?;
    Ok(format!("{kind}-{next}"))
}

/// Converts a configured candidate into the interface record.
fn to_candidate(record: &CandidateRecord) -> Candidate {
    Candidate {
        token: record.token.clone(),
        title: record.title.clone(),
        path: record.path.clone(),
        files: record
            .files
            .iter()
            .map(|file| CandidateFile {
                name: file.name.clone(),
                size: file.size,
                format: file.format.clone(),
                bitrate_kbps: file.bitrate_kbps,
                sample_rate: file.sample_rate,
                bit_depth: file.bit_depth,
                duration_ms: file.duration_ms,
                track_index: file.track_index,
            })
            .collect(),
        source: SourceInfo {
            key: record.source.key.clone(),
            speed_bps: record.source.speed_bps,
            queue_length: record.source.queue_length,
            free_slot: record.source.free_slot,
        },
    }
}

/// Converts a configured album folder into the search record it answers with.
///
/// The folder's name is the candidate's token, so a download addressed by that
/// token can find the folder again, and each file carries its position as its
/// track index, which is what the core maps against the album's tracks.
fn album_record(record: &AlbumCandidateRecord) -> CandidateRecord {
    CandidateRecord {
        token: record.folder.clone(),
        title: record.folder.clone(),
        path: record.folder.clone(),
        files: record
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| FileRecord {
                track_index: Some(index as u32),
                ..file.clone()
            })
            .collect(),
        source: record.source.clone(),
    }
}

/// Reports the files a completed download produced.
///
/// A track download lists the configured output root; an album download lists
/// the files of its configured folder that were requested, in request order, so
/// the core can line each one up with the track it was downloaded for.
///
/// # Parameters
///
/// - `record`: the download's persisted state.
///
/// # Errors
///
/// Returns [`ProviderError::Unavailable`] when the output root cannot be
/// listed, which a misconfigured instance can cause.
fn output_files(record: &DownloadRecord) -> Result<Vec<OutputFile>, ProviderError> {
    if let Some(folder) = &record.folder {
        let config = config()?;
        let Some(candidate) = config
            .album_candidates
            .iter()
            .find(|candidate| &candidate.folder == folder)
        else {
            return Err(ProviderError::Unavailable(format!(
                "the album folder {folder} is not configured"
            )));
        };
        let mut produced = Vec::new();
        for index in &record.requested_files {
            let Some(file) = candidate.files.get(*index as usize) else {
                continue;
            };
            let path = format!("{folder}/{}", file.name);
            let Some(stat) = files::stat(&record.output_root, &path) else {
                continue;
            };
            if stat.is_dir {
                continue;
            }
            produced.push(OutputFile {
                root: record.output_root.clone(),
                path,
                track_index: Some(*index),
            });
        }
        return Ok(produced);
    }
    let entries = files::list_dir(&record.output_root, "").map_err(|error| {
        ProviderError::Unavailable(format!("listing {}: {error}", record.output_root))
    })?;

    let mut produced = Vec::new();
    for name in entries {
        if let Some(suffix) = &record.match_suffix
            && !name.ends_with(suffix.as_str())
        {
            continue;
        }
        let Some(stat) = files::stat(&record.output_root, &name) else {
            continue;
        };
        if stat.is_dir {
            continue;
        }
        produced.push(OutputFile {
            root: record.output_root.clone(),
            path: name,
            track_index: None,
        });
    }
    Ok(produced)
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for AcquisitionFixture {
    fn validate_config(server_config: String) -> Result<(), Vec<String>> {
        match serde_json::from_str::<Config>(&server_config) {
            Ok(_) => Ok(()),
            Err(error) => Err(vec![error.to_string()]),
        }
    }

    fn health() -> Result<String, ProviderError> {
        Ok("acquisition-fixture ready".to_string())
    }
}

/// Implements the acquisition interface.
impl acquisition::Guest for AcquisitionFixture {
    fn get_capabilities() -> Capabilities {
        let config = config().unwrap_or_default();
        Capabilities {
            granularities: config
                .granularities
                .iter()
                .map(|name| match name.as_str() {
                    "album" => Granularity::Album,
                    _ => Granularity::Track,
                })
                .collect(),
            progressive: config.progressive,
        }
    }

    fn start_search(query: Query) -> Result<String, ProviderError> {
        let config = config()?;
        if config.scenario == Scenario::Unavailable {
            return Err(ProviderError::Unavailable(
                "the backend is not answering".to_string(),
            ));
        }
        hearthsound_plugin::info!(
            "searching {} track(s) for `{}`",
            query.tracks.len(),
            query.album.as_deref().unwrap_or("")
        );

        let candidates = match config.scenario {
            Scenario::NoCandidates => Vec::new(),
            _ => match query.granularity {
                Granularity::Album => config.album_candidates.iter().map(album_record).collect(),
                Granularity::Track => config.candidates.clone(),
            },
        };
        let record = SearchRecord {
            candidates,
            polls_left: config.search_polls,
        };
        let token = next_token("search")?;
        kv::set_json(&search_key(&token), &record, None)?;
        Ok(token)
    }

    fn poll_search(search_token: String) -> Result<SearchState, ProviderError> {
        let Some(mut record) = kv::get_json::<SearchRecord>(&search_key(&search_token))? else {
            return Err(ProviderError::NotFound);
        };

        if record.polls_left == 0 {
            return Ok(SearchState::Complete(
                record.candidates.iter().map(to_candidate).collect(),
            ));
        }
        record.polls_left -= 1;
        kv::set_json(&search_key(&search_token), &record, None)?;
        Ok(SearchState::Running(Vec::new()))
    }

    fn cancel_search(search_token: String) {
        host_kv::delete(&search_key(&search_token));
        hearthsound_plugin::debug!("cancelled {search_token}");
    }

    fn start_download(candidate_token: String, files: Vec<u32>) -> Result<String, ProviderError> {
        let config = config()?;
        if config.scenario == Scenario::Unavailable {
            return Err(ProviderError::Unavailable(
                "the backend is not answering".to_string(),
            ));
        }
        hearthsound_plugin::info!("downloading {candidate_token} file(s) {files:?}");

        let running_polls = config.download_running_polls;
        let folder = config
            .album_candidates
            .iter()
            .find(|candidate| candidate.folder == candidate_token)
            .map(|candidate| candidate.folder.clone());
        let record = DownloadRecord {
            queued_left: config.download_queued_polls,
            running_left: running_polls,
            running_polls,
            bytes_per_poll: config.download_bytes_per_poll,
            total_bytes: config.download_bytes_per_poll * u64::from(running_polls),
            failed: config.scenario == Scenario::DownloadFailure,
            stalled: config.scenario == Scenario::Stalled,
            trickle: config.scenario == Scenario::Trickle,
            poll_errors: config.poll_failures,
            // Only the first download loses its token; the retried one is what
            // proves the search restarted.
            lost_first: config.lost_token && next_token("lost")? == "lost-1",
            lost_reported: false,
            output_root: config.output_root,
            match_suffix: config.match_suffix,
            folder,
            requested_files: files,
        };
        let token = next_token("download")?;
        kv::set_json(&download_key(&token), &record, None)?;
        Ok(token)
    }

    fn poll_download(download_token: String) -> Result<DownloadState, ProviderError> {
        let Some(mut record) = kv::get_json::<DownloadRecord>(&download_key(&download_token))?
        else {
            return Err(ProviderError::NotFound);
        };

        if record.lost_first && !record.lost_reported {
            record.lost_reported = true;
            kv::set_json(&download_key(&download_token), &record, None)?;
            return Err(ProviderError::NotFound);
        }

        if record.poll_errors > 0 {
            record.poll_errors -= 1;
            kv::set_json(&download_key(&download_token), &record, None)?;
            return Err(ProviderError::Unavailable(
                "the backend is momentarily unreachable".to_string(),
            ));
        }

        if record.trickle {
            record.running_left = record.running_left.saturating_sub(1);
            let bytes = (record.running_polls - record.running_left) as u64;
            kv::set_json(&download_key(&download_token), &record, None)?;
            return Ok(DownloadState::Running(Progress {
                bytes,
                total_bytes: Some(record.total_bytes),
                speed_bps: Some(1),
            }));
        }

        if record.stalled {
            return Ok(DownloadState::Running(Progress {
                bytes: record.bytes_per_poll,
                total_bytes: Some(record.total_bytes),
                speed_bps: Some(record.bytes_per_poll),
            }));
        }

        if record.queued_left > 0 {
            record.queued_left -= 1;
            let position = record.queued_left + 1;
            kv::set_json(&download_key(&download_token), &record, None)?;
            return Ok(DownloadState::Queued(Some(position)));
        }

        if record.running_left > 0 {
            record.running_left -= 1;
            let bytes = (record.running_polls - record.running_left) as u64 * record.bytes_per_poll;
            kv::set_json(&download_key(&download_token), &record, None)?;
            return Ok(DownloadState::Running(Progress {
                bytes,
                total_bytes: Some(record.total_bytes),
                speed_bps: Some(record.bytes_per_poll),
            }));
        }

        if record.failed {
            return Ok(DownloadState::Failed("the peer disconnected".to_string()));
        }

        let produced = output_files(&record)?;
        if produced.is_empty() {
            return Ok(DownloadState::Failed(
                "the download produced no files".to_string(),
            ));
        }
        Ok(DownloadState::Complete(produced))
    }

    fn cancel_download(download_token: String) {
        host_kv::delete(&download_key(&download_token));
        // The count is what a test asserts a cancellation on: the deleted
        // record cannot say how often it was cancelled.
        let _ = next_token("cancel");
        hearthsound_plugin::debug!("cancelled {download_token}");
    }
}

hearthsound_plugin::export_acquisition_provider!(AcquisitionFixture);
