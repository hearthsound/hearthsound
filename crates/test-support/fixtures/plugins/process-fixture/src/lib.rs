//! Process fixture: an acquisition plugin whose download runs yt-dlp.
//!
//! The plugin offers one candidate per search and downloads it by starting the
//! `yt-dlp` tool the administrator configured, rather than by transferring bytes
//! itself. Every piece of state lives in the key-value store, so a poll that
//! lands on a recycled instance still finds the process handle.
//!
//! The candidate file is reported as `opus` at 160 kbps. The output path is
//! `<download token>/out.opus` inside the staging directory, which is where the
//! tool writes it; the completed download reports exactly that path.

use hearthsound_plugin::host::{files, process};
use hearthsound_plugin::kv;
use hearthsound_plugin::process_acquisition_provider::exports::hearthsound::plugin::acquisition::{
    self, Candidate, CandidateFile, Capabilities, DownloadState, Granularity, OutputFile, Progress,
    Query, SearchState, SourceInfo,
};
use hearthsound_plugin::process_acquisition_provider::exports::hearthsound::plugin::lifecycle;
use hearthsound_plugin::types::ProviderError;
use serde::{Deserialize, Serialize};

/// The tool name the manifest grants.
const TOOL: &str = "yt-dlp";

/// The plugin implementation.
struct ProcessFixture;

/// The wanted track a search remembered.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wanted {
    /// The track's title.
    title: String,
    /// The primary artist's name.
    artist: String,
    /// The track's duration in milliseconds.
    duration_ms: Option<u32>,
}

/// The persisted state of a search.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchRecord {
    /// The track the search looked for.
    wanted: Wanted,
    /// The candidate token the search returned.
    candidate: String,
}

/// The persisted state of a download.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRecord {
    /// The handle of the running process, when one started.
    handle: Option<u64>,
    /// The output path relative to the staging directory.
    path: String,
    /// Why the process could not be started, when it could not.
    error: Option<String>,
}

/// Returns the key a search's state is stored under.
fn search_key(token: &str) -> String {
    format!("search:{token}")
}

/// Returns the key a download's state is stored under.
fn download_key(token: &str) -> String {
    format!("download:{token}")
}

/// Returns the next token of `kind`, counting through the key-value store.
fn next_token(kind: &str) -> Result<String, ProviderError> {
    let key = format!("counter:{kind}");
    let current: u64 = kv::get_json(&key)?.unwrap_or(0);
    let next = current + 1;
    kv::set_json(&key, &next, None)?;
    Ok(format!("{kind}-{next}"))
}

/// Maps a process error to the provider's vocabulary.
fn map_process_error(error: process::ProcessError) -> ProviderError {
    match error {
        process::ProcessError::Denied(_) | process::ProcessError::NotConfigured(_) => {
            ProviderError::InvalidConfig("the tool could not be used".to_string())
        }
        process::ProcessError::SpawnFailed(reason) => ProviderError::Unavailable(reason),
        process::ProcessError::UnknownHandle => {
            ProviderError::Unavailable("the process handle is gone".to_string())
        }
    }
}

/// Describes a spawn failure for the download state.
fn error_text(error: process::ProcessError) -> String {
    match error {
        process::ProcessError::Denied(reason) => format!("denied: {reason}"),
        process::ProcessError::NotConfigured(reason) => format!("not-configured: {reason}"),
        process::ProcessError::SpawnFailed(reason) => format!("spawn-failed: {reason}"),
        process::ProcessError::UnknownHandle => "unknown-handle".to_string(),
    }
}

/// Returns the wanted track of a search, when the query carries one.
fn wanted_track(query: &Query) -> Option<Wanted> {
    let track = query.tracks.first()?;
    Some(Wanted {
        title: track.title.clone(),
        artist: track
            .artists
            .first()
            .cloned()
            .unwrap_or_else(|| "unknown artist".to_string()),
        duration_ms: track.duration_ms,
    })
}

/// Builds the one candidate a search offers for `wanted`.
fn candidate(wanted: &Wanted, token: &str) -> Candidate {
    Candidate {
        token: token.to_string(),
        title: wanted.title.clone(),
        path: format!("{} - {}", wanted.artist, wanted.title),
        files: vec![CandidateFile {
            name: format!("{}.opus", wanted.title),
            size: None,
            format: Some("opus".to_string()),
            bitrate_kbps: Some(160),
            sample_rate: Some(48_000),
            bit_depth: None,
            duration_ms: wanted.duration_ms,
            track_index: None,
        }],
        source: SourceInfo {
            key: "fixture".to_string(),
            speed_bps: None,
            queue_length: None,
            free_slot: Some(true),
        },
    }
}

/// Returns the last line of a failure message.
fn last_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for ProcessFixture {
    fn validate_config(_server_config: String) -> Result<(), Vec<String>> {
        Ok(())
    }

    fn health() -> Result<String, ProviderError> {
        Ok("process-fixture ready".to_string())
    }
}

/// Implements the acquisition interface.
impl acquisition::Guest for ProcessFixture {
    fn get_capabilities() -> Capabilities {
        Capabilities {
            granularities: vec![Granularity::Track],
            progressive: false,
        }
    }

    fn start_search(query: Query) -> Result<String, ProviderError> {
        let search_token = next_token("search")?;
        let wanted = wanted_track(&query);
        let candidate_token = format!("candidate-{search_token}");
        if let Some(wanted) = &wanted {
            hearthsound_plugin::info!("searching for `{}`", wanted.title);
        }
        kv::set_json(
            &search_key(&search_token),
            &SearchRecord {
                wanted: wanted.clone().unwrap_or(Wanted {
                    title: String::new(),
                    artist: String::new(),
                    duration_ms: None,
                }),
                candidate: candidate_token,
            },
            None,
        )?;
        Ok(search_token)
    }

    fn poll_search(search_token: String) -> Result<SearchState, ProviderError> {
        let Some(record) = kv::get_json::<SearchRecord>(&search_key(&search_token))? else {
            return Err(ProviderError::NotFound);
        };
        if record.wanted.title.is_empty() {
            return Ok(SearchState::Complete(Vec::new()));
        }
        Ok(SearchState::Complete(vec![candidate(
            &record.wanted,
            &record.candidate,
        )]))
    }

    fn cancel_search(search_token: String) {
        hearthsound_plugin::host::kv::delete(&search_key(&search_token));
    }

    fn start_download(
        candidate_token: String,
        _files: Vec<u32>,
    ) -> Result<String, ProviderError> {
        let download_token = next_token("download")?;
        let path = format!("{download_token}/out.opus");
        let search_token = candidate_token
            .strip_prefix("candidate-")
            .unwrap_or(candidate_token.as_str());
        let duration_ms = kv::get_json::<SearchRecord>(&search_key(search_token))?
            .and_then(|record| record.wanted.duration_ms)
            .unwrap_or(3000);
        let seconds = duration_ms.div_ceil(1000).clamp(1, 600).to_string();
        let options = process::SpawnOptions {
            cwd: None,
            env: Vec::new(),
            stdout: process::StdoutMode::Capture,
            timeout_ms: 600_000,
        };
        let (handle, error) = match process::spawn(TOOL, &[path.clone(), seconds.clone()], &options)
        {
            Ok(handle) => (Some(handle), None),
            Err(error) => (None, Some(error_text(error))),
        };
        kv::set_json(
            &download_key(&download_token),
            &DownloadRecord {
                handle,
                path,
                error,
            },
            None,
        )?;
        hearthsound_plugin::info!("downloading {candidate_token} as {download_token}");
        Ok(download_token)
    }

    fn poll_download(download_token: String) -> Result<DownloadState, ProviderError> {
        let Some(record) = kv::get_json::<DownloadRecord>(&download_key(&download_token))? else {
            return Err(ProviderError::NotFound);
        };
        if let Some(error) = &record.error {
            return Ok(DownloadState::Failed(error.clone()));
        }
        let Some(handle) = record.handle else {
            return Ok(DownloadState::Failed("the download has no process".to_string()));
        };
        let state = process::poll(handle).map_err(map_process_error)?;
        match state {
            process::ProcessState::Running => Ok(DownloadState::Running(Progress {
                bytes: 0,
                total_bytes: None,
                speed_bps: None,
            })),
            process::ProcessState::Exited(0) => match files::stat("staging", &record.path) {
                Some(stat) if !stat.is_dir => Ok(DownloadState::Complete(vec![OutputFile {
                    root: "staging".to_string(),
                    path: record.path,
                    track_index: None,
                }])),
                _ => Ok(DownloadState::Failed(
                    "the download produced no file".to_string(),
                )),
            },
            process::ProcessState::Exited(code) => {
                let stderr = process::read_stderr(handle).unwrap_or_default();
                let reason = last_line(&stderr)
                    .unwrap_or_else(|| format!("yt-dlp exited with status {code}"));
                Ok(DownloadState::Failed(reason))
            }
            process::ProcessState::Killed => {
                Ok(DownloadState::Failed("the download was cancelled".to_string()))
            }
            process::ProcessState::TimedOut => {
                Ok(DownloadState::Failed("the download timed out".to_string()))
            }
        }
    }

    fn cancel_download(download_token: String) {
        if let Ok(Some(record)) = kv::get_json::<DownloadRecord>(&download_key(&download_token))
            && let Some(handle) = record.handle
        {
            process::kill(handle);
        }
        hearthsound_plugin::host::kv::delete(&download_key(&download_token));
    }
}

hearthsound_plugin::export_process_acquisition_provider!(ProcessFixture);
