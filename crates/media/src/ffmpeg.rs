//! Running `ffmpeg` child processes under one concurrency budget.
//!
//! Every ffmpeg invocation in the server - transcodes and verification decodes -
//! goes through one [`FfmpegRunner`], which holds the process budget in a
//! semaphore. A permit is taken before a child is spawned and released when the
//! child's handle is dropped, so a caller that drops a stream also gives its
//! budget back.
//!
//! Arguments are always passed as a vector: the runner never builds a shell
//! command line, and the child is killed if its handle is dropped without
//! being awaited.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), hearthsound_media::MediaError> {
//! use hearthsound_media::FfmpegRunner;
//!
//! let runner = FfmpegRunner::new(None, 8);
//! runner.run(["-hide_banner".into(), "-version".into()]).await?;
//! # Ok(())
//! # }
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::MediaError;

/// The binary name used when no path is configured.
const DEFAULT_BINARY: &str = "ffmpeg";

/// Spawns and limits `ffmpeg` child processes.
///
/// One runner serves the whole process: it owns the concurrency budget, so
/// transcodes and verification decodes compete for the same slots, and it can be
/// resized when the operator changes `stream.max_concurrent_transcodes`.
/// Cloning a runner shares the budget, which is how a task that outlives its
/// caller keeps spending from the same pool.
#[derive(Debug, Clone)]
pub struct FfmpegRunner {
    inner: Arc<FfmpegRunnerInner>,
}

/// The shared state behind a runner.
#[derive(Debug)]
struct FfmpegRunnerInner {
    binary: PathBuf,
    permits: Arc<Semaphore>,
    max_concurrent: AtomicUsize,
    in_flight: Arc<AtomicUsize>,
}

impl FfmpegRunner {
    /// Creates a runner.
    ///
    /// # Parameters
    ///
    /// - `binary`: the ffmpeg binary to run, or `None` for `ffmpeg` on `PATH`.
    /// - `max_concurrent`: how many children may run at once; values below one
    ///   are treated as one.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_media::FfmpegRunner;
    ///
    /// let runner = FfmpegRunner::new(Some("/usr/bin/ffmpeg".into()), 4);
    /// assert_eq!(runner.max_concurrent(), 4);
    /// assert_eq!(runner.in_flight(), 0);
    /// ```
    pub fn new(binary: Option<PathBuf>, max_concurrent: usize) -> FfmpegRunner {
        let cap = max_concurrent.max(1);
        FfmpegRunner {
            inner: Arc::new(FfmpegRunnerInner {
                binary: binary.unwrap_or_else(|| PathBuf::from(DEFAULT_BINARY)),
                permits: Arc::new(Semaphore::new(cap)),
                max_concurrent: AtomicUsize::new(cap),
                in_flight: Arc::new(AtomicUsize::new(0)),
            }),
        }
    }

    /// Returns the ffmpeg binary the runner executes.
    pub fn binary(&self) -> &Path {
        &self.inner.binary
    }

    /// Returns the number of children that may run at once.
    pub fn max_concurrent(&self) -> usize {
        self.inner.max_concurrent.load(Ordering::SeqCst)
    }

    /// Returns the number of children running right now.
    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::SeqCst)
    }

    /// Changes the concurrency cap.
    ///
    /// Growing the cap lets waiting callers start immediately; shrinking it
    /// takes budget away from callers that have not started yet, while children
    /// that already run keep their permit until they exit.
    ///
    /// # Parameters
    ///
    /// - `max_concurrent`: the new cap; values below one are treated as one.
    pub fn set_max_concurrent(&self, max_concurrent: usize) {
        let cap = max_concurrent.max(1);
        let previous = self.inner.max_concurrent.swap(cap, Ordering::SeqCst);
        if cap > previous {
            self.inner.permits.add_permits(cap - previous);
        } else if cap < previous {
            self.inner.permits.forget_permits(previous - cap);
        }
    }

    /// Runs ffmpeg to completion and discards its output.
    ///
    /// Stdin and stdout are connected to nothing; the exit status decides
    /// success. The child counts against the concurrency budget for as long as
    /// it runs.
    ///
    /// # Parameters
    ///
    /// - `args`: the argument vector, without the binary itself.
    ///
    /// # Returns
    ///
    /// `Ok(())` when ffmpeg exits successfully.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::Process`] when ffmpeg cannot be started, exits with
    /// a failure (the message carries the captured standard error), or the
    /// runner's budget is closed.
    pub async fn run(&self, args: impl IntoIterator<Item = OsString>) -> Result<(), MediaError> {
        let args: Vec<OsString> = args.into_iter().collect();
        let guard = self.acquire(&args).await?;
        let program = self.binary_display();
        let output = Command::new(&self.inner.binary)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|source| MediaError::Process {
                program: program.clone(),
                args: join_args(&args),
                message: source.to_string(),
            })?;
        drop(guard);
        if !output.status.success() {
            return Err(MediaError::Process {
                program,
                args: join_args(&args),
                message: format!(
                    "{}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        Ok(())
    }

    /// Spawns ffmpeg with piped standard output.
    ///
    /// Standard input is connected to nothing and standard output is piped, so a
    /// caller streams from the child. The returned [`FfmpegProcess`] holds a
    /// budget permit until it is dropped, so callers that stream from the child
    /// must keep the handle alive for as long as they read.
    ///
    /// # Parameters
    ///
    /// - `args`: the argument vector, without the binary itself.
    ///
    /// # Returns
    ///
    /// The running child.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::Process`] when ffmpeg cannot be started or the
    /// runner's budget is closed.
    pub async fn spawn(
        &self,
        args: impl IntoIterator<Item = OsString>,
    ) -> Result<FfmpegProcess, MediaError> {
        let args: Vec<OsString> = args.into_iter().collect();
        let guard = self.acquire(&args).await?;
        let program = self.binary_display();
        let mut child = Command::new(&self.inner.binary)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| MediaError::Process {
                program: program.clone(),
                args: join_args(&args),
                message: source.to_string(),
            })?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        tracing::debug!(
            program = %program,
            pid,
            args = %join_args(&args),
            "spawned ffmpeg"
        );
        let stderr_task = stderr.map(|stderr| {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(pid, line = %line, "ffmpeg stderr");
                }
            })
        });
        Ok(FfmpegProcess {
            child,
            stdout,
            stderr_task,
            pid,
            program,
            args,
            _guard: guard,
        })
    }

    /// Takes a permit from the budget.
    ///
    /// # Parameters
    ///
    /// - `args`: the argument vector, used in the error message.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::Process`] when the semaphore is closed, which only
    /// happens when the runner is dropped.
    async fn acquire(&self, args: &[OsString]) -> Result<PermitGuard, MediaError> {
        let permit = Arc::clone(&self.inner.permits)
            .acquire_owned()
            .await
            .map_err(|_| MediaError::Process {
                program: self.binary_display(),
                args: join_args(args),
                message: "the ffmpeg concurrency budget was closed".to_owned(),
            })?;
        self.inner.in_flight.fetch_add(1, Ordering::SeqCst);
        Ok(PermitGuard {
            _permit: permit,
            in_flight: Arc::clone(&self.inner.in_flight),
        })
    }

    /// Renders the binary path for a log line or an error message.
    fn binary_display(&self) -> String {
        self.inner.binary.display().to_string()
    }
}

/// A running ffmpeg child process.
///
/// Dropping the process kills the child, releases its concurrency permit, and
/// stops the task that forwards its standard error.
#[derive(Debug)]
pub struct FfmpegProcess {
    child: Child,
    stdout: Option<ChildStdout>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    pid: Option<u32>,
    program: String,
    args: Vec<OsString>,
    _guard: PermitGuard,
}

impl FfmpegProcess {
    /// Returns the process id, when the platform reports one.
    pub fn id(&self) -> Option<u32> {
        self.pid
    }

    /// Returns the ffmpeg binary the child runs.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Takes the child's standard output.
    ///
    /// # Returns
    ///
    /// The pipe, or `None` when it was already taken.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// Waits for the child to exit.
    ///
    /// # Returns
    ///
    /// The exit status.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::Process`] when the status cannot be read.
    pub async fn wait(&mut self) -> Result<ExitStatus, MediaError> {
        self.child
            .wait()
            .await
            .map_err(|source| MediaError::Process {
                program: self.program.clone(),
                args: join_args(&self.args),
                message: source.to_string(),
            })
    }

    /// Asks the child to terminate.
    ///
    /// The kill is asynchronous; [`FfmpegProcess::wait`] observes the result.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::Process`] when the signal cannot be delivered.
    pub fn kill(&mut self) -> Result<(), MediaError> {
        self.child
            .start_kill()
            .map_err(|source| MediaError::Process {
                program: self.program.clone(),
                args: join_args(&self.args),
                message: source.to_string(),
            })
    }
}

impl Drop for FfmpegProcess {
    /// Kills the child and stops the standard-error forwarder.
    fn drop(&mut self) {
        // `kill_on_drop` also covers a process that was never polled, but
        // killing explicitly makes the behavior independent of how the child
        // handle was created.
        let _ = self.child.start_kill();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

/// A held concurrency permit that keeps the in-flight count in step.
#[derive(Debug)]
struct PermitGuard {
    _permit: OwnedSemaphorePermit,
    in_flight: Arc<AtomicUsize>,
}

impl Drop for PermitGuard {
    /// Gives the budget back.
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Joins an argument vector for a log line or an error message.
///
/// # Parameters
///
/// - `args`: the argument vector.
fn join_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}
