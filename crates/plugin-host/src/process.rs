//! The `host-process` import: external tools an acquisition plugin may run.
//!
//! A plugin never names an executable path. It names a *tool*, and the host
//! resolves that name through the `tools.<name>.path` setting the administrator
//! filled in, exactly as an import root resolves a root id. The manifest has to
//! list the tool under `permissions.process.tools`, so installing or upgrading a
//! plugin cannot silently add a program the server may run.
//!
//! One [`ProcessTable`] belongs to one plugin instance and is shared by every
//! store its pool hands out, keyed by an opaque handle. `spawn` only runs inside
//! an acquisition call, because that is the only place with a staging directory
//! the process may work in; `cwd` and a captured-output file are resolved inside
//! that directory and refused when they leave it.
//!
//! Every process runs with a cleared environment: the only variables it sees are
//! the ones the tool's allowlist admits from the server and the entries the
//! plugin passed, each of which must be allowlisted too. Standard output and
//! standard error are captured when the plugin asked for them, each keeping at
//! most one mebibyte of unread data.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingValue, SettingsStore};
use hearthsound_core::Slug;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;

/// An opaque handle to a process one instance started.
///
/// Each plugin API version generates its own `process-handle` type from its
/// `type process-handle = u64` alias; both map onto this one, so the table, its
/// tests and the policies it enforces are written once.
pub(crate) type ProcessHandle = u64;

/// Where a process's standard output goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StdoutMode {
    /// Kept in memory, at most [`MAX_UNREAD_OUTPUT`] unread bytes.
    Capture,
    /// Written to this path, relative to the current staging directory.
    File(String),
}

/// The options of one `spawn` call.
#[derive(Debug, Clone)]
pub(crate) struct SpawnOptions {
    /// Working directory relative to the staging directory, when the plugin
    /// named one.
    pub cwd: Option<String>,
    /// Extra environment entries; each name must be allowlisted by the tool.
    pub env: Vec<(String, String)>,
    /// Where standard output goes.
    pub stdout: StdoutMode,
    /// Wall-clock limit in milliseconds, at most [`MAX_TIMEOUT_MS`].
    pub timeout_ms: u32,
}

/// The state of a process as `poll` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessState {
    /// Still running.
    Running,
    /// Exited with this status code.
    Exited(i32),
    /// Killed by `kill` or by the instance being torn down.
    Killed,
    /// Killed because the wall-clock limit expired.
    TimedOut,
}

/// A refused process operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProcessError {
    /// The tool is not in the manifest's tools, or an option broke a rule.
    Denied(String),
    /// The administrator has no usable path for the tool.
    NotConfigured(String),
    /// The operating system refused to start the process.
    SpawnFailed(String),
    /// The handle is unknown to this instance or already released.
    UnknownHandle,
}

/// The most captured bytes one stream keeps before it starts dropping.
const MAX_UNREAD_OUTPUT: usize = 1024 * 1024;

/// How many processes one instance may have running at once.
const MAX_LIVE_PROCESSES: usize = 4;

/// The longest wall-clock limit a plugin may ask for.
const MAX_TIMEOUT_MS: u32 = 3_600_000;

/// How long an exited process's handle stays readable.
const EXITED_RETENTION: Duration = Duration::from_secs(600);

/// The resolved configuration of one tool.
#[derive(Debug, Clone)]
struct ToolConfig {
    /// The executable path the administrator configured, when it is usable.
    path: Option<PathBuf>,
    /// The server environment variables the process may inherit.
    env_allowlist: Vec<String>,
}

/// The process table of one plugin instance.
pub(crate) struct ProcessTable {
    plugin: String,
    slug: Slug,
    tools: Vec<String>,
    settings: SettingsSource,
    inner: Arc<TableInner>,
}

/// Where the table reads the tool settings from.
enum SettingsSource {
    /// The snapshot the instance was built with.
    Snapshot(Arc<SettingsStore>),
    /// The server's live settings, read again at every spawn.
    Live(Settings),
}

/// The handles, processes, and live-process budget of one table.
///
/// The budget is a semaphore with [`MAX_LIVE_PROCESSES`] permits. `spawn`
/// reserves one permit before it awaits anything, and the running entry holds
/// it until the process stops running or its handle is released.
struct TableInner {
    next_handle: AtomicU64,
    processes: AsyncMutex<HashMap<ProcessHandle, Entry>>,
    slots: Arc<tokio::sync::Semaphore>,
}

impl TableInner {
    /// Creates an empty table with every live-process slot free.
    fn new() -> TableInner {
        TableInner {
            next_handle: AtomicU64::new(1),
            processes: AsyncMutex::new(HashMap::new()),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_LIVE_PROCESSES)),
        }
    }
}

/// One process the instance started and the slot it holds while running.
struct Entry {
    child: Child,
    state: ProcessState,
    stdout: Arc<OutputBuffer>,
    stderr: Arc<OutputBuffer>,
    /// The staging directory the process was started under.
    staging: PathBuf,
    /// When the process stopped, used for the retention window.
    exited_at: Option<Instant>,
    /// The live-process permit, held only while the process runs.
    slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// A bounded, drainable capture of one stream.
struct OutputBuffer {
    data: Mutex<Vec<u8>>,
}

impl OutputBuffer {
    /// Creates an empty buffer.
    fn new() -> OutputBuffer {
        OutputBuffer {
            data: Mutex::new(Vec::new()),
        }
    }

    /// Appends as much of `chunk` as the cap still allows.
    ///
    /// # Parameters
    ///
    /// - `chunk`: the bytes just read from the pipe.
    fn push(&self, chunk: &[u8]) {
        let mut data = lock(&self.data);
        let room = MAX_UNREAD_OUTPUT.saturating_sub(data.len());
        if room == 0 {
            return;
        }
        let take = room.min(chunk.len());
        data.extend_from_slice(&chunk[..take]);
    }

    /// Returns the captured bytes and empties the buffer.
    fn drain(&self) -> Vec<u8> {
        let mut data = lock(&self.data);
        std::mem::take(&mut *data)
    }
}

impl ProcessTable {
    /// Builds the table of one instance.
    ///
    /// Only the tools the manifest lists are resolved; a listed tool without a
    /// registered or non-empty `tools.<name>.path` setting resolves to no
    /// executable, which every `spawn` then answers with
    /// [`ProcessError::NotConfigured`].
    ///
    /// # Parameters
    ///
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug, used in log fields.
    /// - `tools`: the tool names the manifest lists.
    /// - `settings`: the resolved settings, read once for this instance.
    pub(crate) fn new(
        plugin: &str,
        slug: &Slug,
        tools: &[String],
        settings: Arc<SettingsStore>,
    ) -> ProcessTable {
        ProcessTable {
            plugin: plugin.to_owned(),
            slug: slug.clone(),
            tools: tools.to_vec(),
            settings: SettingsSource::Snapshot(settings),
            inner: Arc::new(TableInner::new()),
        }
    }

    /// Builds a table that reads the server's live settings at every spawn.
    ///
    /// The administrator can change a tool's path while the server runs, so the
    /// table resolves the path and the allowlist from the newest settings
    /// snapshot instead of the one the instance was built with.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug, used in log fields.
    /// - `tools`: the tool names the manifest lists.
    /// - `settings`: the server's runtime settings.
    pub(crate) fn live(
        plugin: &str,
        slug: &Slug,
        tools: &[String],
        settings: Settings,
    ) -> ProcessTable {
        ProcessTable {
            plugin: plugin.to_owned(),
            slug: slug.clone(),
            tools: tools.to_vec(),
            settings: SettingsSource::Live(settings),
            inner: Arc::new(TableInner::new()),
        }
    }

    /// Returns the settings snapshot the next spawn resolves against.
    fn settings(&self) -> Arc<SettingsStore> {
        match &self.settings {
            SettingsSource::Snapshot(settings) => Arc::clone(settings),
            SettingsSource::Live(settings) => settings.current(),
        }
    }

    /// Resolves the configuration of `tool` from the current settings.
    ///
    /// # Parameters
    ///
    /// - `tool`: the tool name the manifest lists.
    fn tool_config(&self, tool: &str) -> ToolConfig {
        let settings = self.settings();
        let path = match settings.value(&format!("tools.{tool}.path")) {
            Some(SettingValue::String(path)) if !path.trim().is_empty() => {
                Some(PathBuf::from(path.trim()))
            }
            _ => None,
        };
        let env_allowlist = match settings.value(&format!("tools.{tool}.env_allowlist")) {
            Some(SettingValue::StringList(names)) => names.clone(),
            _ => Vec::new(),
        };
        ToolConfig {
            path,
            env_allowlist,
        }
    }

    /// Builds a table that runs no tools, for tests of other host imports.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug, used in log fields.
    #[cfg(test)]
    pub(crate) fn without_tools(plugin: &str, slug: &Slug) -> ProcessTable {
        ProcessTable {
            plugin: plugin.to_owned(),
            slug: slug.clone(),
            tools: Vec::new(),
            settings: SettingsSource::Snapshot(Arc::new(
                SettingsStore::resolve(
                    &hearthsound_config::SettingsRegistry::builtin(),
                    &hearthsound_config::SettingSources::default(),
                )
                .expect("the built-in settings resolve"),
            )),
            inner: Arc::new(TableInner::new()),
        }
    }

    /// Starts one tool process inside the current staging directory.
    ///
    /// One of the instance's [`MAX_LIVE_PROCESSES`] slots is reserved before
    /// the staging directory or the output file is touched, so concurrent
    /// spawns cannot all pass the cap. The slot is released when the process
    /// stops running, times out, or its handle is released.
    ///
    /// # Parameters
    ///
    /// - `staging`: the staging directory of the acquisition call, which the
    ///   process may work in. `None` refuses every spawn, because a process
    ///   without a staging directory would have nowhere to write.
    /// - `tool`: the tool name the manifest lists.
    /// - `args`: the argument vector passed to the executable; no shell is
    ///   involved.
    /// - `options`: the working directory, environment entries, where standard
    ///   output goes, and the wall-clock limit.
    ///
    /// # Returns
    ///
    /// The handle that addresses the process in [`ProcessTable::poll`],
    /// [`ProcessTable::read_stdout`], [`ProcessTable::read_stderr`] and
    /// [`ProcessTable::kill`].
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError::Denied`] when the tool is not listed, an option
    /// leaves the staging directory, an environment name is not allowlisted, or
    /// the instance already runs the maximum number of processes;
    /// [`ProcessError::NotConfigured`] when the administrator has no usable path
    /// for the tool; and [`ProcessError::SpawnFailed`] when the operating system
    /// refuses to start it.
    pub(crate) async fn spawn(
        &self,
        staging: Option<&Path>,
        tool: &str,
        args: Vec<String>,
        options: SpawnOptions,
    ) -> Result<ProcessHandle, ProcessError> {
        let Some(staging) = staging else {
            return Err(ProcessError::Denied(
                "no staging directory is set for this call".to_owned(),
            ));
        };
        if !self.tools.iter().any(|listed| listed == tool) {
            return Err(ProcessError::Denied(format!(
                "tool `{tool}` is not listed in this plugin's manifest"
            )));
        }
        let config = self.tool_config(tool);
        let Some(program) = config.path.as_deref().filter(|path| is_executable(path)) else {
            return Err(ProcessError::NotConfigured(format!(
                "tools.{tool}.path does not name an executable file"
            )));
        };

        self.reap().await;

        let Ok(slot) = Arc::clone(&self.inner.slots).try_acquire_owned() else {
            return Err(ProcessError::Denied("too many processes".to_owned()));
        };

        let base = tokio::fs::canonicalize(staging).await.map_err(|error| {
            ProcessError::Denied(format!("the staging directory is unusable: {error}"))
        })?;

        let cwd = match options.cwd {
            Some(cwd) => Some(resolve_inside(&base, &cwd).await?),
            None => None,
        };

        let mut command = Command::new(program);
        command.args(&args);
        command.stdin(std::process::Stdio::null());
        command.stderr(std::process::Stdio::piped());
        command.kill_on_drop(true);
        command.current_dir(cwd.unwrap_or_else(|| base.clone()));

        let stdout = match options.stdout {
            StdoutMode::Capture => {
                command.stdout(std::process::Stdio::piped());
                Some(Arc::new(OutputBuffer::new()))
            }
            StdoutMode::File(path) => {
                let relative = checked_relative(&path)?;
                let parent = create_parent_inside(&base, relative).await?;
                let resolved = tokio::fs::canonicalize(&parent).await.map_err(|error| {
                    ProcessError::Denied(format!("the output directory is unusable: {error}"))
                })?;
                if !resolved.starts_with(&base) {
                    return Err(ProcessError::Denied(
                        "the output path leaves the staging directory".to_owned(),
                    ));
                }
                let path = base.join(relative);
                if let Ok(metadata) = tokio::fs::symlink_metadata(&path).await
                    && metadata.file_type().is_symlink()
                {
                    return Err(ProcessError::Denied(
                        "the output path leaves the staging directory".to_owned(),
                    ));
                }
                let mut open = tokio::fs::OpenOptions::new();
                open.write(true).create(true).truncate(true);
                #[cfg(unix)]
                open.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
                let file = open.open(&path).await.map_err(|error| {
                    ProcessError::Denied(format!("the output file cannot be created: {error}"))
                })?;
                command.stdout(std::process::Stdio::from(file.into_std().await));
                None
            }
        };

        command.env_clear();
        for name in &config.env_allowlist {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        for (name, value) in options.env {
            if !config.env_allowlist.iter().any(|allowed| allowed == &name) {
                return Err(ProcessError::Denied(format!(
                    "environment name `{name}` is not allowlisted for {tool}"
                )));
            }
            command.env(name, value);
        }

        let mut child = command
            .spawn()
            .map_err(|error| ProcessError::SpawnFailed(error.to_string()))?;

        let stderr = {
            let buffer = Arc::new(OutputBuffer::new());
            if let Some(pipe) = child.stderr.take() {
                read_pipe(pipe, Arc::clone(&buffer));
            }
            buffer
        };
        let stdout = match stdout {
            Some(buffer) => {
                if let Some(pipe) = child.stdout.take() {
                    read_pipe(pipe, Arc::clone(&buffer));
                }
                buffer
            }
            None => Arc::new(OutputBuffer::new()),
        };

        let handle = self.inner.next_handle.fetch_add(1, Ordering::Relaxed);
        let timeout = Duration::from_millis(u64::from(options.timeout_ms.min(MAX_TIMEOUT_MS)));
        self.inner.processes.lock().await.insert(
            handle,
            Entry {
                child,
                state: ProcessState::Running,
                stdout,
                stderr,
                staging: staging.to_path_buf(),
                exited_at: None,
                slot: Some(slot),
            },
        );

        {
            let weak = Arc::downgrade(&self.inner);
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                if let Some(inner) = weak.upgrade() {
                    time_out(&inner, handle).await;
                }
            });
        }

        tracing::debug!(
            target: "hearthsound::plugin",
            plugin = %self.plugin,
            instance = %self.slug,
            tool = %tool,
            handle,
            "started a tool process"
        );
        Ok(handle)
    }

    /// Reports the state of one process.
    ///
    /// # Parameters
    ///
    /// - `handle`: the handle [`ProcessTable::spawn`] returned.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError::UnknownHandle`] when the handle was released or
    /// never issued, and [`ProcessError::SpawnFailed`] when the operating system
    /// cannot report on a running process.
    pub(crate) async fn poll(&self, handle: ProcessHandle) -> Result<ProcessState, ProcessError> {
        self.reap().await;
        let mut processes = self.inner.processes.lock().await;
        let Some(entry) = processes.get_mut(&handle) else {
            return Err(ProcessError::UnknownHandle);
        };
        if matches!(entry.state, ProcessState::Running) {
            match entry.child.try_wait() {
                Ok(Some(status)) => {
                    entry.state = ProcessState::Exited(status.code().unwrap_or(-1));
                    entry.exited_at = Some(Instant::now());
                    drop(entry.slot.take());
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        target: "hearthsound::plugin",
                        plugin = %self.plugin,
                        instance = %self.slug,
                        handle,
                        error = %error,
                        "waiting for a tool process failed"
                    );
                    entry.state = ProcessState::Killed;
                    entry.exited_at = Some(Instant::now());
                    drop(entry.slot.take());
                }
            }
        }
        Ok(entry.state)
    }

    /// Returns and drains the captured standard output.
    ///
    /// # Parameters
    ///
    /// - `handle`: the handle [`ProcessTable::spawn`] returned.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError::UnknownHandle`] when the handle was released or
    /// never issued.
    pub(crate) async fn read_stdout(&self, handle: ProcessHandle) -> Result<Vec<u8>, ProcessError> {
        let processes = self.inner.processes.lock().await;
        let Some(entry) = processes.get(&handle) else {
            return Err(ProcessError::UnknownHandle);
        };
        Ok(entry.stdout.drain())
    }

    /// Returns and drains the captured standard error.
    ///
    /// # Parameters
    ///
    /// - `handle`: the handle [`ProcessTable::spawn`] returned.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError::UnknownHandle`] when the handle was released or
    /// never issued.
    pub(crate) async fn read_stderr(&self, handle: ProcessHandle) -> Result<Vec<u8>, ProcessError> {
        let processes = self.inner.processes.lock().await;
        let Some(entry) = processes.get(&handle) else {
            return Err(ProcessError::UnknownHandle);
        };
        Ok(entry.stderr.drain())
    }

    /// Kills a process when it still runs and releases its handle.
    ///
    /// Releasing the handle drops the process object, which also kills a
    /// process that is still running because every child is started with
    /// `kill_on_drop`, and drops the entry's live-process slot.
    ///
    /// # Parameters
    ///
    /// - `handle`: the handle [`ProcessTable::spawn`] returned. An unknown
    ///   handle is ignored, because the interface has no error channel here.
    pub(crate) async fn kill(&self, handle: ProcessHandle) {
        let mut processes = self.inner.processes.lock().await;
        if let Some(mut entry) = processes.remove(&handle) {
            let _ = entry.child.start_kill();
        }
    }

    /// Kills every process started under `staging` and releases its handle.
    ///
    /// The orchestrator calls this when an acquisition reaches a terminal
    /// state, so a tool a plugin left running cannot keep writing into a
    /// directory that is about to be removed. Every removed entry releases its
    /// live-process slot.
    ///
    /// # Parameters
    ///
    /// - `staging`: the acquisition's staging directory.
    pub(crate) async fn kill_staging(&self, staging: &Path) {
        let mut processes = self.inner.processes.lock().await;
        let released: Vec<ProcessHandle> = processes
            .iter()
            .filter(|(_, entry)| entry.staging == staging)
            .map(|(handle, _)| *handle)
            .collect();
        for handle in released {
            if let Some(mut entry) = processes.remove(&handle) {
                let _ = entry.child.start_kill();
                tracing::debug!(
                    target: "hearthsound::plugin",
                    plugin = %self.plugin,
                    instance = %self.slug,
                    handle,
                    "killed a tool process left by an acquisition"
                );
            }
        }
    }

    /// Forgets the handles of processes that exited long enough ago.
    async fn reap(&self) {
        let mut processes = self.inner.processes.lock().await;
        let now = Instant::now();
        processes.retain(|_, entry| match entry.exited_at {
            Some(at) => now.duration_since(at) < EXITED_RETENTION,
            None => true,
        });
    }
}

/// Marks a process timed out and kills it.
///
/// # Parameters
///
/// - `inner`: the table the process lives in.
/// - `handle`: the process to time out.
async fn time_out(inner: &TableInner, handle: ProcessHandle) {
    let mut processes = inner.processes.lock().await;
    let Some(entry) = processes.get_mut(&handle) else {
        return;
    };
    if !matches!(entry.state, ProcessState::Running) {
        return;
    }
    let _ = entry.child.start_kill();
    entry.state = ProcessState::TimedOut;
    entry.exited_at = Some(Instant::now());
    drop(entry.slot.take());
}

/// Checks that a plugin path may be resolved inside the staging directory.
///
/// # Parameters
///
/// - `path`: the plugin's relative path.
///
/// # Returns
///
/// The path as a relative path.
///
/// # Errors
///
/// Returns [`ProcessError::Denied`] when the path is absolute or carries a `..`
/// component.
fn checked_relative(path: &str) -> Result<&Path, ProcessError> {
    let relative = Path::new(path);
    if relative.is_absolute() {
        return Err(ProcessError::Denied(format!("`{path}` is absolute")));
    }
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ProcessError::Denied(format!(
            "`{path}` contains a component that leaves the staging directory"
        )));
    }
    Ok(relative)
}

/// Creates the parent directory of an output path inside the staging directory.
///
/// The components of the parent are walked one at a time from `base`, and each
/// one is inspected with [`tokio::fs::symlink_metadata`] before it is entered.
/// A symlink the plugin placed inside the staging directory therefore cannot
/// redirect the creation elsewhere, and because only one level is created at a
/// time, `create_dir` cannot descend a symlink either.
///
/// # Parameters
///
/// - `base`: the canonical staging directory.
/// - `relative`: the plugin's relative output path.
///
/// # Returns
///
/// The parent directory of `relative`, created when it was missing.
///
/// # Errors
///
/// Returns [`ProcessError::Denied`] when a component is a symlink or is not a
/// directory, or when the directory cannot be inspected or created.
async fn create_parent_inside(base: &Path, relative: &Path) -> Result<PathBuf, ProcessError> {
    let mut current = base.to_path_buf();
    let Some(parent) = relative.parent() else {
        return Ok(current);
    };
    for component in parent.components() {
        let name = match component {
            Component::Normal(name) => name,
            Component::CurDir => continue,
            _ => {
                return Err(ProcessError::Denied(
                    "the output path leaves the staging directory".to_owned(),
                ));
            }
        };
        current.push(name);
        match tokio::fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ProcessError::Denied(
                    "the output path leaves the staging directory".to_owned(),
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(ProcessError::Denied(
                    "the output path leaves the staging directory".to_owned(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&current).await.map_err(|error| {
                    ProcessError::Denied(format!("the output directory is unusable: {error}"))
                })?;
            }
            Err(error) => {
                return Err(ProcessError::Denied(format!(
                    "the output directory is unusable: {error}"
                )));
            }
        }
    }
    Ok(current)
}

/// Resolves a working directory inside the staging directory.
///
/// # Parameters
///
/// - `base`: the canonical staging directory.
/// - `path`: the plugin's relative path.
///
/// # Returns
///
/// The canonical directory.
///
/// # Errors
///
/// Returns [`ProcessError::Denied`] when the path is absolute, carries a `..`
/// component, or resolves outside the staging directory through a symlink.
async fn resolve_inside(base: &Path, path: &str) -> Result<PathBuf, ProcessError> {
    let joined = base.join(checked_relative(path)?);
    match tokio::fs::canonicalize(&joined).await {
        Ok(resolved) if !resolved.starts_with(base) => Err(ProcessError::Denied(format!(
            "`{path}` leaves the staging directory"
        ))),
        Ok(resolved) => Ok(resolved),
        Err(error) => Err(ProcessError::Denied(format!(
            "`{path}` cannot be resolved: {error}"
        ))),
    }
}

/// Reads a pipe into `buffer` until it closes.
///
/// # Parameters
///
/// - `pipe`: the child's standard output or standard error.
/// - `buffer`: the buffer the bytes are captured in.
fn read_pipe<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut pipe: R,
    buffer: Arc<OutputBuffer>,
) {
    tokio::spawn(async move {
        let mut chunk = vec![0u8; 8192];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => buffer.push(&chunk[..read]),
                Err(_) => break,
            }
        }
    });
}

/// Returns whether `path` names an executable file.
///
/// # Parameters
///
/// - `path`: the configured executable path.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Locks a mutex, ignoring poisoning.
///
/// # Parameters
///
/// - `mutex`: the lock to take.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use hearthsound_config::{SettingSources, SettingsRegistry};

    use super::*;

    /// The tool the tests configure.
    const TOOL: &str = "yt-dlp";

    /// Builds a table whose `yt-dlp` is `path`.
    fn table(path: &Path) -> ProcessTable {
        let mut sources = SettingSources::default();
        sources.env.insert(
            "HEARTHSOUND__TOOLS__YT-DLP__PATH".to_owned(),
            path.display().to_string(),
        );
        sources.env.insert(
            "HEARTHSOUND__TOOLS__YT-DLP__ENV_ALLOWLIST".to_owned(),
            "PATH,HOME,HEARTHSOUND_TEST_ALLOWED".to_owned(),
        );
        let settings = SettingsStore::resolve(&SettingsRegistry::builtin(), &sources).unwrap();
        ProcessTable::new(
            "com.example.process-fixture",
            &Slug::new("probe").unwrap(),
            &[TOOL.to_owned()],
            Arc::new(settings),
        )
    }

    /// Builds a table whose tool has no configured path.
    fn unconfigured() -> ProcessTable {
        let settings =
            SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default())
                .unwrap();
        ProcessTable::new(
            "com.example.process-fixture",
            &Slug::new("probe").unwrap(),
            &[TOOL.to_owned()],
            Arc::new(settings),
        )
    }

    /// Writes an executable shell script into `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    /// The default spawn options of a test.
    fn options() -> SpawnOptions {
        SpawnOptions {
            cwd: None,
            env: Vec::new(),
            stdout: StdoutMode::Capture,
            timeout_ms: 5_000,
        }
    }

    /// Polls until the process is no longer running.
    async fn wait_exit(table: &ProcessTable, handle: ProcessHandle) -> ProcessState {
        for _ in 0..200 {
            let state = table.poll(handle).await.unwrap();
            if !matches!(state, ProcessState::Running) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the process did not finish");
    }

    #[tokio::test]
    async fn an_unlisted_tool_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let table = table(&script(dir.path(), "tool", "#!/bin/sh\nexit 0\n"));

        let error = table
            .spawn(Some(dir.path()), "ffmpeg", Vec::new(), options())
            .await
            .unwrap_err();

        assert!(matches!(error, ProcessError::Denied(reason) if reason.contains("ffmpeg")));
    }

    #[tokio::test]
    async fn an_empty_path_is_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        let table = unconfigured();

        let error = table
            .spawn(Some(dir.path()), TOOL, Vec::new(), options())
            .await
            .unwrap_err();

        assert!(matches!(error, ProcessError::NotConfigured(_)));
    }

    #[tokio::test]
    async fn a_spawn_without_a_staging_directory_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let table = table(&script(dir.path(), "tool", "#!/bin/sh\nexit 0\n"));

        let error = table
            .spawn(None, TOOL, Vec::new(), options())
            .await
            .unwrap_err();

        assert!(matches!(error, ProcessError::Denied(_)));
    }

    #[tokio::test]
    async fn a_cwd_that_leaves_the_staging_directory_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let table = table(&script(dir.path(), "tool", "#!/bin/sh\nexit 0\n"));
        let mut options = options();
        options.cwd = Some("../outside".to_owned());

        let error = table
            .spawn(Some(&staging), TOOL, Vec::new(), options)
            .await
            .unwrap_err();

        assert!(matches!(error, ProcessError::Denied(_)));
    }

    #[tokio::test]
    async fn an_output_file_that_leaves_the_staging_directory_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let table = table(&script(dir.path(), "tool", "#!/bin/sh\nexit 0\n"));
        let mut options = options();
        options.stdout = StdoutMode::File("../out.txt".to_owned());

        let error = table
            .spawn(Some(&staging), TOOL, Vec::new(), options)
            .await
            .unwrap_err();

        assert!(matches!(error, ProcessError::Denied(_)));
    }

    #[tokio::test]
    async fn a_plugin_environment_entry_must_be_allowlisted() {
        let dir = tempfile::tempdir().unwrap();
        let table = table(&script(dir.path(), "tool", "#!/bin/sh\nexit 0\n"));
        let mut options = options();
        options.env = vec![("SECRET".to_owned(), "value".to_owned())];

        let error = table
            .spawn(Some(dir.path()), TOOL, Vec::new(), options)
            .await
            .unwrap_err();

        assert!(matches!(error, ProcessError::Denied(reason) if reason.contains("SECRET")));
    }

    #[tokio::test]
    async fn a_timed_out_process_is_killed_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let table = table(Path::new("/bin/sh"));
        let mut options = options();
        options.timeout_ms = 200;

        let handle = table
            .spawn(
                Some(dir.path()),
                TOOL,
                vec!["-c".to_owned(), "sleep 5".to_owned()],
                options,
            )
            .await
            .unwrap();
        let state = wait_exit(&table, handle).await;

        assert!(matches!(state, ProcessState::TimedOut));
        assert_eq!(
            table.inner.slots.available_permits(),
            MAX_LIVE_PROCESSES,
            "a timed-out process must release its slot"
        );
    }

    #[tokio::test]
    async fn the_environment_holds_only_the_allowlisted_names() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let script = script(dir.path(), "tool", "#!/bin/sh\nenv\nprintf 'done\\n'\n");
        let table = table(&script);
        let mut options = options();
        options.env = vec![("HEARTHSOUND_TEST_ALLOWED".to_owned(), "seen".to_owned())];

        let handle = table
            .spawn(Some(&staging), TOOL, Vec::new(), options)
            .await
            .unwrap();
        assert!(matches!(
            wait_exit(&table, handle).await,
            ProcessState::Exited(0)
        ));
        let output = String::from_utf8(table.read_stdout(handle).await.unwrap()).unwrap();

        assert!(
            output
                .lines()
                .any(|line| line == "HEARTHSOUND_TEST_ALLOWED=seen"),
            "the plugin's allowlisted entry should reach the process: {output}"
        );
        for line in output.lines().filter(|line| line.contains('=')) {
            let name = line.split('=').next().unwrap();
            assert!(
                matches!(
                    name,
                    "PATH" | "HOME" | "HEARTHSOUND_TEST_ALLOWED" | "PWD" | "OLDPWD" | "SHLVL" | "_"
                ),
                "`{name}` should not have been passed to the process: {output}"
            );
        }
    }

    #[tokio::test]
    async fn output_beyond_the_cap_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let script = script(
            dir.path(),
            "tool",
            "#!/bin/sh\nhead -c 1500000 /dev/zero | tr '\\0' 'a'\n",
        );
        let table = table(&script);

        let handle = table
            .spawn(Some(&staging), TOOL, Vec::new(), options())
            .await
            .unwrap();
        assert!(matches!(
            wait_exit(&table, handle).await,
            ProcessState::Exited(0)
        ));
        let output = table.read_stdout(handle).await.unwrap();

        assert_eq!(output.len(), MAX_UNREAD_OUTPUT);
        assert!(output.iter().all(|byte| *byte == b'a'));
    }

    #[tokio::test]
    async fn a_stdout_file_receives_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let script = script(dir.path(), "tool", "#!/bin/sh\nprintf 'written\\n'\n");
        let table = table(&script);
        let mut options = options();
        options.stdout = StdoutMode::File("out/log.txt".to_owned());

        let handle = table
            .spawn(Some(&staging), TOOL, Vec::new(), options)
            .await
            .unwrap();
        assert!(matches!(
            wait_exit(&table, handle).await,
            ProcessState::Exited(0)
        ));
        let written = std::fs::read_to_string(staging.join("out/log.txt")).unwrap();

        assert_eq!(written, "written\n");
        assert!(table.read_stdout(handle).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_output_directory_symlinked_outside_the_staging_directory_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, staging.join("out")).unwrap();
        let table = table(&script(
            dir.path(),
            "tool",
            "#!/bin/sh\nprintf 'written\\n'\n",
        ));
        let mut options = options();
        options.stdout = StdoutMode::File("out/x.log".to_owned());

        let error = table
            .spawn(Some(&staging), TOOL, Vec::new(), options)
            .await
            .unwrap_err();

        assert_eq!(
            error,
            ProcessError::Denied("the output path leaves the staging directory".to_owned())
        );
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "the denied spawn must not write outside the staging directory"
        );
    }

    #[tokio::test]
    async fn an_output_file_symlinked_outside_the_staging_directory_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(staging.join("out")).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(outside.join("missing.log"), staging.join("out/leaf.log"))
            .unwrap();
        let table = table(&script(
            dir.path(),
            "tool",
            "#!/bin/sh\nprintf 'written\\n'\n",
        ));
        let mut options = options();
        options.stdout = StdoutMode::File("out/leaf.log".to_owned());

        let error = table
            .spawn(Some(&staging), TOOL, Vec::new(), options)
            .await
            .unwrap_err();

        assert_eq!(
            error,
            ProcessError::Denied("the output path leaves the staging directory".to_owned())
        );
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "the symlink at the leaf must not be followed"
        );
    }

    #[tokio::test]
    async fn a_nested_output_directory_is_created_inside_the_staging_directory() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let table = table(&script(
            dir.path(),
            "tool",
            "#!/bin/sh\nprintf 'nested\\n'\n",
        ));
        let mut options = options();
        options.stdout = StdoutMode::File("a/b/log.txt".to_owned());

        let handle = table
            .spawn(Some(&staging), TOOL, Vec::new(), options)
            .await
            .unwrap();
        assert!(matches!(
            wait_exit(&table, handle).await,
            ProcessState::Exited(0)
        ));

        assert_eq!(
            std::fs::read_to_string(staging.join("a/b/log.txt")).unwrap(),
            "nested\n"
        );
        assert!(staging.join("a/b").is_dir());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_fresh_table_grants_every_live_process_slot_once() {
        let table = table(Path::new("/bin/sh"));
        let mut held = Vec::new();
        for _ in 0..MAX_LIVE_PROCESSES {
            held.push(Arc::clone(&table.inner.slots).try_acquire_owned().unwrap());
        }

        assert!(
            Arc::clone(&table.inner.slots).try_acquire_owned().is_err(),
            "a fresh table must refuse one live-process slot more than the cap"
        );
        assert_eq!(held.len(), MAX_LIVE_PROCESSES);
    }

    #[tokio::test]
    async fn a_stopped_entry_releases_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let table = table(Path::new("/bin/sh"));
        let handle = table
            .spawn(
                Some(dir.path()),
                TOOL,
                vec!["-c".to_owned(), "sleep 5".to_owned()],
                options(),
            )
            .await
            .unwrap();
        let mut held = Vec::new();
        for _ in 0..MAX_LIVE_PROCESSES - 1 {
            held.push(Arc::clone(&table.inner.slots).try_acquire_owned().unwrap());
        }
        assert!(Arc::clone(&table.inner.slots).try_acquire_owned().is_err());

        {
            let mut processes = table.inner.processes.lock().await;
            let entry = processes.get_mut(&handle).unwrap();
            entry.state = ProcessState::Exited(0);
            drop(entry.slot.take());
        }

        assert!(
            Arc::clone(&table.inner.slots).try_acquire_owned().is_ok(),
            "a slot must be available once the entry leaves Running"
        );
        table.kill(handle).await;
    }

    #[tokio::test]
    async fn the_live_process_cap_refuses_the_next_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let table = table(Path::new("/bin/sh"));
        let mut handles = Vec::new();
        for _ in 0..MAX_LIVE_PROCESSES {
            handles.push(
                table
                    .spawn(
                        Some(&staging),
                        TOOL,
                        vec!["-c".to_owned(), "sleep 30".to_owned()],
                        options(),
                    )
                    .await
                    .unwrap(),
            );
        }
        for handle in &handles {
            assert_eq!(
                table.poll(*handle).await.unwrap(),
                ProcessState::Running,
                "the first {MAX_LIVE_PROCESSES} processes must all be live"
            );
        }

        let error = table
            .spawn(
                Some(&staging),
                TOOL,
                vec!["-c".to_owned(), "sleep 30".to_owned()],
                options(),
            )
            .await
            .unwrap_err();
        assert_eq!(error, ProcessError::Denied("too many processes".to_owned()));

        for handle in handles {
            table.kill(handle).await;
        }
        assert_eq!(
            table.inner.slots.available_permits(),
            MAX_LIVE_PROCESSES,
            "releasing the handles must release their slots"
        );

        let handle = table
            .spawn(
                Some(&staging),
                TOOL,
                vec!["-c".to_owned(), "exit 0".to_owned()],
                options(),
            )
            .await
            .unwrap();
        assert!(matches!(
            wait_exit(&table, handle).await,
            ProcessState::Exited(0)
        ));
        assert_eq!(
            table.inner.slots.available_permits(),
            MAX_LIVE_PROCESSES,
            "poll must release the slot once the process has exited"
        );
    }

    #[tokio::test]
    async fn kill_releases_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let table = table(Path::new("/bin/sh"));
        let handle = table
            .spawn(
                Some(dir.path()),
                TOOL,
                vec!["-c".to_owned(), "sleep 5".to_owned()],
                options(),
            )
            .await
            .unwrap();

        table.kill(handle).await;

        assert!(matches!(
            table.poll(handle).await.unwrap_err(),
            ProcessError::UnknownHandle
        ));
        assert!(matches!(
            table.read_stdout(handle).await.unwrap_err(),
            ProcessError::UnknownHandle
        ));
    }

    #[tokio::test]
    async fn releasing_a_staging_directory_kills_its_processes() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let table = table(Path::new("/bin/sh"));
        let under_first = table
            .spawn(
                Some(&first),
                TOOL,
                vec!["-c".to_owned(), "sleep 5".to_owned()],
                options(),
            )
            .await
            .unwrap();
        let under_second = table
            .spawn(
                Some(&second),
                TOOL,
                vec!["-c".to_owned(), "sleep 5".to_owned()],
                options(),
            )
            .await
            .unwrap();

        table.kill_staging(&first).await;

        assert!(matches!(
            table.poll(under_first).await.unwrap_err(),
            ProcessError::UnknownHandle
        ));
        assert!(matches!(
            table.poll(under_second).await.unwrap(),
            ProcessState::Running
        ));
        table.kill(under_second).await;
    }
}
