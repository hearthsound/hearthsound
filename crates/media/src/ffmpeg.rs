//! Running `ffmpeg` child processes under one concurrency budget.
//!
//! Every ffmpeg invocation in the server - transcodes and verification decodes -
//! goes through one [`FfmpegRunner`], which holds the process budget in one
//! counter. A permit is taken before a child is spawned and released after the
//! child has exited, so a caller that drops a stream also gives its budget back
//! once ffmpeg is really gone, and the cap the runner reports is the cap in
//! force.
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
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::Notify;

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
    budget: Arc<ProcessBudget>,
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
        FfmpegRunner {
            inner: Arc::new(FfmpegRunnerInner {
                binary: binary.unwrap_or_else(|| PathBuf::from(DEFAULT_BINARY)),
                budget: ProcessBudget::new(max_concurrent),
            }),
        }
    }

    /// Returns the ffmpeg binary the runner executes.
    pub fn binary(&self) -> &Path {
        &self.inner.binary
    }

    /// Returns the number of children that may run at once.
    pub fn max_concurrent(&self) -> usize {
        self.inner.budget.cap()
    }

    /// Returns the number of children running right now.
    pub fn in_flight(&self) -> usize {
        self.inner.budget.in_use()
    }

    /// Changes the concurrency cap.
    ///
    /// Growing the cap lets waiting callers start immediately. A smaller cap
    /// applies as running children finish: the ones already running keep their
    /// slot, no new child starts until the count is below the new cap, and
    /// [`FfmpegRunner::max_concurrent`] always reports the cap in force, so the
    /// reported value and the children actually running converge.
    ///
    /// # Parameters
    ///
    /// - `max_concurrent`: the new cap; values below one are treated as one.
    pub fn set_max_concurrent(&self, max_concurrent: usize) {
        self.inner.budget.resize(max_concurrent);
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
    /// Returns [`MediaError::Process`] when ffmpeg cannot be started or exits
    /// with a failure (the message carries the captured standard error).
    pub async fn run(&self, args: impl IntoIterator<Item = OsString>) -> Result<(), MediaError> {
        let args: Vec<OsString> = args.into_iter().collect();
        let guard = self.inner.budget.acquire().await;
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

    /// Scales one image down to a bounding box and writes it as a JPEG.
    ///
    /// The child counts against the same concurrency budget as a transcode, so a
    /// burst of cover requests cannot start more ffmpeg processes than
    /// `stream.max_concurrent_transcodes` allows. An image smaller than the box
    /// is written at its own size: the filter never enlarges.
    ///
    /// # Parameters
    ///
    /// - `input`: the image to read.
    /// - `output`: the file to write; it is overwritten.
    /// - `max_side`: the longest side of the result, in pixels.
    ///
    /// # Returns
    ///
    /// `Ok(())` once the file is written.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::Process`] when ffmpeg cannot be started, cannot read
    /// the input, or exits with a failure.
    pub async fn scale_image(
        &self,
        input: &Path,
        output: &Path,
        max_side: u32,
    ) -> Result<(), MediaError> {
        // The commas inside `min(..)` are escaped because a bare comma would
        // separate two filters of the chain instead of two arguments of `min`.
        let filter = format!(
            "scale=min({max_side}\\,iw):min({max_side}\\,ih):force_original_aspect_ratio=decrease"
        );
        let args = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-i",
            &input.to_string_lossy(),
            "-vf",
            &filter,
            "-frames:v",
            "1",
            "-update",
            "1",
            "-pix_fmt",
            "yuvj420p",
            "-c:v",
            "mjpeg",
            "-q:v",
            "3",
            "-f",
            "image2",
            &output.to_string_lossy(),
        ];
        self.run(args.into_iter().map(OsString::from)).await
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
    /// Returns [`MediaError::Process`] when ffmpeg cannot be started.
    pub async fn spawn(
        &self,
        args: impl IntoIterator<Item = OsString>,
    ) -> Result<FfmpegProcess, MediaError> {
        let args: Vec<OsString> = args.into_iter().collect();
        let guard = self.inner.budget.acquire().await;
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
            inner: Some(ProcessInner { child, guard }),
            stdout,
            stderr_task,
            pid,
            program,
            args,
        })
    }

    /// Renders the binary path for a log line or an error message.
    fn binary_display(&self) -> String {
        self.inner.binary.display().to_string()
    }
}

/// A running ffmpeg child process.
///
/// Dropping the process kills the child and returns its concurrency slot only
/// after the child has exited, so the budget never admits a replacement while
/// the old process is still terminating. Outside a tokio runtime the kill is
/// asynchronous and the slot returns immediately.
#[derive(Debug)]
pub struct FfmpegProcess {
    /// The child and its permit, taken out when the process is dropped.
    inner: Option<ProcessInner>,
    stdout: Option<ChildStdout>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    pid: Option<u32>,
    program: String,
    args: Vec<OsString>,
}

/// The child and the permit that keeps its slot.
#[derive(Debug)]
struct ProcessInner {
    /// The running child.
    child: Child,
    /// The slot, returned once the child has exited.
    guard: PermitGuard,
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
        let Some(inner) = self.inner.as_mut() else {
            return Err(MediaError::Process {
                program: self.program.clone(),
                args: join_args(&self.args),
                message: "the process was already dropped".to_owned(),
            });
        };
        inner
            .child
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
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        inner
            .child
            .start_kill()
            .map_err(|source| MediaError::Process {
                program: self.program.clone(),
                args: join_args(&self.args),
                message: source.to_string(),
            })
    }
}

impl Drop for FfmpegProcess {
    /// Kills the child, and returns its slot once the child has exited.
    fn drop(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        let Some(inner) = self.inner.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                // The wait is what keeps the slot honest: the child is gone
                // from the process table before the permit comes back.
                tokio::spawn(async move {
                    let ProcessInner { mut child, guard } = inner;
                    let _ = child.kill().await;
                    drop(guard);
                });
            }
            Err(_) => {
                // No runtime to wait on: the kill is asynchronous, so the slot
                // returns at once.
                let ProcessInner { mut child, guard } = inner;
                let _ = child.start_kill();
                drop(child);
                drop(guard);
            }
        }
    }
}

/// The concurrency budget of the ffmpeg children.
///
/// One budget serves the whole process, so a transcode and a verification decode
/// compete for the same slots. The counter is an explicit one rather than a
/// semaphore because a cap that shrinks below the number of children already
/// running must not lose that budget: the smaller cap takes effect as the
/// children finish, and [`ProcessBudget::cap`] always reports the cap in force.
#[derive(Debug)]
struct ProcessBudget {
    /// The cap and the number of permits held, under one lock.
    state: Mutex<BudgetState>,
    /// Wakes a caller that waited for a free permit.
    changed: Notify,
}

/// The counters behind a [`ProcessBudget`].
#[derive(Debug)]
struct BudgetState {
    /// How many children may run at once.
    cap: usize,
    /// How many permits are held.
    in_use: usize,
}

impl ProcessBudget {
    /// Builds a budget that admits `cap` children at once.
    ///
    /// # Parameters
    ///
    /// - `cap`: how many children may run at once; values below one are treated
    ///   as one.
    fn new(cap: usize) -> Arc<ProcessBudget> {
        Arc::new(ProcessBudget {
            state: Mutex::new(BudgetState {
                cap: cap.max(1),
                in_use: 0,
            }),
            changed: Notify::new(),
        })
    }

    /// Takes a permit, waiting while the budget is full.
    ///
    /// The lock is never held across the wait, so a caller that is blocked here
    /// cannot hold up a permit being returned.
    async fn acquire(self: &Arc<Self>) -> PermitGuard {
        loop {
            // The wait is registered before the counters are read, so a permit
            // returned between the read and the wait is not missed.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.lock();
                if state.in_use < state.cap {
                    state.in_use += 1;
                    return PermitGuard {
                        budget: Arc::clone(self),
                    };
                }
            }
            notified.await;
        }
    }

    /// Changes the cap.
    ///
    /// # Parameters
    ///
    /// - `cap`: the new cap; values below one are treated as one.
    fn resize(&self, cap: usize) {
        let cap = cap.max(1);
        self.lock().cap = cap;
        // Every waiter re-reads the counters, so a raised cap admits them all.
        self.changed.notify_waiters();
    }

    /// Returns the cap in force.
    fn cap(&self) -> usize {
        self.lock().cap
    }

    /// Returns how many permits are held.
    fn in_use(&self) -> usize {
        self.lock().in_use
    }

    /// Locks the counters, ignoring a poisoned lock.
    ///
    /// The lock only guards two integers, so a panic elsewhere cannot leave them
    /// inconsistent; recovering the guard is always correct.
    fn lock(&self) -> std::sync::MutexGuard<'_, BudgetState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A held concurrency permit.
///
/// Dropping it returns the permit and wakes one caller waiting for a slot.
#[derive(Debug)]
struct PermitGuard {
    budget: Arc<ProcessBudget>,
}

impl Drop for PermitGuard {
    /// Gives the budget back.
    fn drop(&mut self) {
        {
            let mut state = self.budget.lock();
            state.in_use = state.in_use.saturating_sub(1);
        }
        self.budget.changed.notify_one();
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_smaller_cap_is_reached_once_busy_permits_are_returned() {
        let budget = ProcessBudget::new(4);
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(budget.acquire().await);
        }
        assert_eq!(budget.in_use(), 4);

        budget.resize(2);
        assert_eq!(budget.cap(), 2);
        assert_eq!(
            budget.in_use(),
            4,
            "children that already run keep their slots"
        );

        held.clear();
        assert_eq!(budget.in_use(), 0);

        let first = budget.acquire().await;
        let second = budget.acquire().await;
        assert_eq!(budget.in_use(), 2);
        let third = tokio::time::timeout(Duration::from_millis(50), budget.acquire()).await;
        assert!(
            third.is_err(),
            "a third caller started beyond the shrunken cap"
        );
        drop(first);
        drop(second);
    }

    #[tokio::test]
    async fn a_raise_after_a_shrink_adds_only_the_difference() {
        let budget = ProcessBudget::new(8);
        let mut held = Vec::new();
        for _ in 0..5 {
            held.push(budget.acquire().await);
        }

        budget.resize(2);
        budget.resize(8);
        assert_eq!(budget.cap(), 8);

        let mut extra = Vec::new();
        for _ in 0..3 {
            extra.push(budget.acquire().await);
        }
        assert_eq!(
            budget.in_use(),
            8,
            "the raised cap admits the difference, not the difference plus the busy slots"
        );
        let ninth = tokio::time::timeout(Duration::from_millis(50), budget.acquire()).await;
        assert!(ninth.is_err(), "the budget admitted more than its cap");
        held.clear();
        extra.clear();
        assert_eq!(budget.in_use(), 0);
    }
}
