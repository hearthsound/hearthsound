//! The serving process: the HTTP listeners, the background workers, shutdown.
//!
//! [`Server::start`] binds every address `server.bind` resolves to, spawns the
//! job runner, the scheduler, the folder watcher, and the worker that reacts to
//! writes made by other connections, and serves the axum application. [`Server::run`] adds the
//! signal handling a deployment needs: it starts, waits for `SIGINT` or
//! `SIGTERM`, and then stops the listener and every worker.
//!
//! Shutdown is orderly in one direction: the stop signal ends the long-lived
//! responses (the event stream ends, and a stream or download still open after
//! [`HTTP_DRAIN_TIMEOUT`] is cut), the HTTP server stops accepting new
//! connections and finishes the responses in flight, the job runner finishes the
//! jobs it holds (up to its shutdown grace period), the scheduler stops between
//! ticks, the folder watcher drops its watchers, and the data-version watcher
//! stops polling. A client therefore cannot hold shutdown open, and a worker
//! that panicked is reported so the process exits non-zero. A handler abandoned
//! by the runner's grace period keeps its claim and runs again after its lock
//! expires, which is why handlers must tolerate running twice.

use std::sync::Arc;

use hearthsound_core::Availability;
use hearthsound_core::events::{
    EventBus, LibraryChange, LibraryChanged, ServerEvent, TrackAvailability,
};
use hearthsound_jobs::Shutdown;
use hearthsound_library::maintenance::MaintenanceSignal;
use hearthsound_library::scan::ScanRequest;
use std::future::Future;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::error::ServerError;
use crate::services::Services;

/// The delays between the attempts to list the library folders at startup.
///
/// A database that another process is still writing, or a storage tree that is
/// not mounted yet, is the case this covers: the listing is retried a few times
/// before the startup scan is given up on.
const STARTUP_SCAN_DELAYS: [Duration; 4] = [Duration::from_secs(2); 4];

/// How long the listener gets to finish the responses in flight.
///
/// An audio stream or a download still open after this is cut: shutdown is the
/// operator's decision, and a client must not be able to hold the process open.
const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A running server: the HTTP listeners plus the background workers.
pub struct Server {
    /// The services the listener and the workers share.
    services: Arc<Services>,
    /// The signal every worker and the HTTP server stop on.
    shutdown_signal: Shutdown,
    /// The addresses the listeners are bound to.
    ///
    /// A host name may resolve to several addresses - `localhost` is both
    /// loopback addresses - and every one of them is served.
    listeners: Vec<std::net::SocketAddr>,
    /// The tasks serving HTTP, one per listener.
    http: Vec<JoinHandle<Result<(), std::io::Error>>>,
    /// The tasks running the job runner, the scheduler, and the watcher, each
    /// with the name it is reported under.
    workers: Vec<(&'static str, JoinHandle<()>)>,
}

impl Server {
    /// Binds every address `server.bind` resolves to and starts serving.
    ///
    /// A host name that resolves to several addresses - `localhost` is both
    /// loopback addresses - is served on all of them, so a client that reaches
    /// the name over either protocol is answered. An address that cannot be
    /// bound is skipped with a warning, because one protocol being unavailable -
    /// a host without IPv6, or a port already taken on one loopback address -
    /// must not stop the server from serving the others.
    ///
    /// # Parameters
    ///
    /// - `services`: the composed services.
    ///
    /// # Returns
    ///
    /// The running server, whose [`Server::local_addrs`] reports the bound
    /// addresses.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Listen`] when the configured bind address and port
    /// do not resolve, and the last [`ServerError::Bind`] when no resolved
    /// address can be bound at all.
    pub async fn start(services: Arc<Services>) -> Result<Server, ServerError> {
        let addresses = services.listen_addresses()?;
        let mut listeners = Vec::new();
        let mut http = Vec::new();
        let mut failure = None;
        for address in addresses {
            match bind(address).await {
                Ok((listener, bound)) => {
                    listeners.push(bound);
                    http.push(serve(
                        listener,
                        crate::http::router(&services),
                        services.shutdown(),
                    ));
                }
                Err(error) => {
                    tracing::warn!(
                        address = %address,
                        %error,
                        "could not bind a resolved address; continuing with the others",
                    );
                    failure = Some(error);
                }
            }
        }
        // Nothing could be bound: the last failure is what the operator has to
        // act on. A partial success keeps serving, because one protocol being
        // unavailable must not take the process down.
        if let Some(error) = failure.filter(|_| http.is_empty()) {
            return Err(error);
        }
        Server::finish(services, listeners, http).await
    }

    /// Binds `addr` and starts serving.
    ///
    /// A test passes `127.0.0.1:0` and reads the ephemeral port back from
    /// [`Server::local_addr`].
    ///
    /// # Parameters
    ///
    /// - `services`: the composed services.
    /// - `addr`: the address to bind.
    ///
    /// # Returns
    ///
    /// The running server.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Bind`] when the socket cannot be bound or its
    /// local address cannot be read.
    pub async fn start_on(
        services: Arc<Services>,
        addr: std::net::SocketAddr,
    ) -> Result<Server, ServerError> {
        let (listener, bound) = bind(addr).await?;
        let http = vec![serve(
            listener,
            crate::http::router(&services),
            services.shutdown(),
        )];
        Server::finish(services, vec![bound], http).await
    }

    /// Starts the workers and reports the bound addresses.
    ///
    /// # Parameters
    ///
    /// - `services`: the composed services.
    /// - `listeners`: the addresses the listeners are bound to.
    /// - `http`: the tasks serving them.
    ///
    /// # Returns
    ///
    /// The running server.
    async fn finish(
        services: Arc<Services>,
        listeners: Vec<std::net::SocketAddr>,
        http: Vec<JoinHandle<Result<(), std::io::Error>>>,
    ) -> Result<Server, ServerError> {
        let shutdown_signal = services.shutdown();
        let workers = spawn_workers(&services, shutdown_signal.clone());

        // The one bootstrap credential of the whole server: a fresh database
        // has no account, and the only way to create one is this link. It is
        // logged exactly once per process and stops working as soon as it is
        // used.
        let first = listeners[0];
        if let Some(token) = services.setup().token().await {
            let url = setup_url(&services.config().server.base_url, first, &token);
            tracing::warn!(url = %url, "No administrator exists yet; open the setup URL to create one");
        }

        tracing::info!(addresses = ?listeners, "serving");

        Ok(Server {
            services,
            shutdown_signal,
            listeners,
            http,
            workers,
        })
    }

    /// Returns the first address the server is bound to.
    ///
    /// A test that bound an ephemeral port reads it back here.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.listeners[0]
    }

    /// Returns every address the server is bound to.
    pub fn local_addrs(&self) -> &[std::net::SocketAddr] {
        &self.listeners
    }

    /// Returns the services the server is running.
    pub fn services(&self) -> &Arc<Services> {
        &self.services
    }

    /// Stops serving and waits for every worker to finish.
    ///
    /// The stop signal ends the event streams, so no client can hold the
    /// listener open; the listener then drains the responses in flight up to
    /// [`HTTP_DRAIN_TIMEOUT`]; the job runner finishes or abandons the jobs it
    /// holds according to its grace period; the scheduler and the watcher stop
    /// between ticks.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the listener stopped without an error and every worker
    /// finished cleanly.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Serve`] when the HTTP server stopped with an I/O
    /// error, [`ServerError::HttpTask`] when its task panicked or was cancelled,
    /// and [`ServerError::Workers`] when a worker panicked. Every failure is
    /// logged as it happens, and the first one is returned.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # async fn example(services: Arc<hearthsound_server::Services>) -> Result<(), Box<dyn std::error::Error>> {
    /// let server = hearthsound_server::Server::start_on(services, "127.0.0.1:0".parse()?).await?;
    /// server.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn shutdown(self) -> Result<(), ServerError> {
        let Server {
            shutdown_signal,
            http,
            workers,
            ..
        } = self;
        shutdown_signal.trigger();
        let drained = drain_http(http).await;
        let stopped = stop_workers(workers).await;
        drained?;
        stopped
    }

    /// Waits until the HTTP server stops, then for every worker.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the listener stopped without an error and every worker
    /// finished cleanly.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Serve`] when the HTTP server stopped with an I/O
    /// error, [`ServerError::HttpTask`] when its task panicked or was
    /// cancelled, and [`ServerError::Workers`] when a worker panicked.
    pub async fn wait(self) -> Result<(), ServerError> {
        let Server { http, workers, .. } = self;
        let drained = drain_http(http).await;
        let stopped = stop_workers(workers).await;
        drained?;
        stopped
    }

    /// Starts the server, waits for a termination signal, and shuts down.
    ///
    /// This is what `hearthsound serve` runs.
    ///
    /// # Parameters
    ///
    /// - `services`: the composed services.
    ///
    /// # Returns
    ///
    /// `Ok(())` once every worker has stopped.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Server::start`] and [`Server::shutdown`], which
    /// `hearthsound serve` turns into a non-zero exit.
    pub async fn run(services: Arc<Services>) -> Result<(), ServerError> {
        let server = Server::start(services).await?;
        wait_for_signal().await;
        tracing::info!("a termination signal arrived; shutting down");
        server.shutdown().await
    }
}

/// Spawns the job runner, the scheduler, the indexer, the folder watchers, the
/// storage maintenance worker, and the data-version watcher.
///
/// The data-version watcher is what makes a command-line write take effect on
/// a running server: it polls `PRAGMA data_version` and reloads the settings
/// and the provider registry when another connection commits (see
/// [`Services::watch_external_changes`](crate::Services::watch_external_changes)).
///
/// # Parameters
///
/// - `services`: the composed services.
/// - `shutdown`: the signal the workers stop on.
///
/// # Returns
///
/// The tasks, in spawn order.
fn spawn_workers(
    services: &Arc<Services>,
    shutdown: Shutdown,
) -> Vec<(&'static str, JoinHandle<()>)> {
    let runner = Arc::clone(services.runner());
    let runner_shutdown = shutdown.clone();
    let runner_task = tokio::spawn(async move { runner.run(runner_shutdown).await });

    let scheduler = Arc::clone(services.scheduler());
    let scheduler_shutdown = shutdown.clone();
    let scheduler_task = tokio::spawn(async move { scheduler.run(scheduler_shutdown).await });

    let watchers = Arc::clone(services);
    let watchers_shutdown = shutdown.clone();
    let watchers_task =
        tokio::spawn(async move { watch_folders(watchers, watchers_shutdown).await });

    let maintainer = Arc::clone(services.maintainer());
    let signal = services.maintenance().clone();
    let maintainer_shutdown = shutdown.clone();
    let maintainer_task =
        tokio::spawn(async move { maintainer.run(signal, maintainer_shutdown).await });

    let indexer = Arc::clone(services.catalog().indexer());
    let indexer_shutdown = shutdown.clone();
    let indexer_task = tokio::spawn(async move { indexer.run(indexer_shutdown).await });

    let deliverer = Arc::clone(services.scrobble_deliverer());
    let deliverer_shutdown = shutdown.clone();
    let deliverer_task = tokio::spawn(async move { deliverer.run(deliverer_shutdown).await });

    let events = Arc::clone(services.events());
    let signal = services.maintenance().clone();
    let events_shutdown = shutdown.clone();
    let events_task =
        tokio::spawn(async move { wake_on_new_files(events, signal, events_shutdown).await });

    let external = Arc::clone(services);
    let external_task =
        tokio::spawn(async move { external.watch_external_changes(shutdown).await });

    vec![
        ("job runner", runner_task),
        ("scheduler", scheduler_task),
        ("indexer", indexer_task),
        ("folder watcher", watchers_task),
        ("storage maintainer", maintainer_task),
        ("scrobble deliverer", deliverer_task),
        ("event wake-up", events_task),
        ("data-version watcher", external_task),
    ]
}

/// Calls `attempt` until it succeeds, waiting between the failures.
///
/// The waits are bounded by `delays`, so the call makes `delays.len() + 1`
/// attempts at most, and each wait ends early when `shutdown` is triggered: a
/// starting server must not keep retrying a listing for a process that is
/// already stopping.
///
/// # Parameters
///
/// - `delays`: the wait after each failed attempt, in order. An empty slice
///   means one attempt.
/// - `shutdown`: the stop signal that abandons the retries.
/// - `context`: what the attempts were for, recorded on each failure.
/// - `attempt`: the work, called with the number of the attempt, counting from
///   one.
///
/// # Returns
///
/// The first successful value, or `None` when every attempt failed or the
/// process was stopping.
pub(crate) async fn with_retries<T, F, Fut, E>(
    delays: &[Duration],
    shutdown: &Shutdown,
    context: &str,
    mut attempt: F,
) -> Option<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let total = u32::try_from(delays.len()).unwrap_or(u32::MAX - 1) + 1;
    for index in 0..total {
        let number = index + 1;
        match attempt(number).await {
            Ok(value) => return Some(value),
            Err(error) => {
                tracing::warn!(attempt = number, attempts = total, %error, context, "an attempt failed");
            }
        }
        if let Some(delay) = delays.get(usize::try_from(index).unwrap_or(usize::MAX)) {
            tokio::select! {
                () = shutdown.wait() => return None,
                () = tokio::time::sleep(*delay) => {}
            }
        }
    }
    None
}

/// Binds one address.
///
/// # Parameters
///
/// - `addr`: the address to bind.
///
/// # Returns
///
/// The listener and the address it is bound to, which carries the port the
/// operating system chose when the requested port was zero.
///
/// # Errors
///
/// Returns [`ServerError::Bind`] when the socket cannot be bound or its local
/// address cannot be read.
async fn bind(
    addr: std::net::SocketAddr,
) -> Result<(TcpListener, std::net::SocketAddr), ServerError> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| ServerError::Bind { addr, source })?;
    let bound = listener
        .local_addr()
        .map_err(|source| ServerError::Bind { addr, source })?;
    Ok((listener, bound))
}

/// Spawns the task that serves one listener.
///
/// # Parameters
///
/// - `listener`: the bound listener.
/// - `app`: the router every request is answered with.
/// - `shutdown`: the signal that stops accepting and drains the responses.
///
/// # Returns
///
/// The serving task.
fn serve(
    listener: TcpListener,
    app: axum::Router,
    shutdown: Shutdown,
) -> JoinHandle<Result<(), std::io::Error>> {
    tokio::spawn(async move {
        // The connect information is what lets a handler read the peer
        // address: the login rate limit is keyed on the client address, and
        // `server.trusted_proxies` decides whether a forwarding header
        // names the client.
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.wait().await })
        .await
    })
}

/// Waits for the HTTP tasks, cutting the connections after [`HTTP_DRAIN_TIMEOUT`].
///
/// # Parameters
///
/// - `http`: the tasks serving the listeners.
///
/// # Returns
///
/// `Ok(())` when every listener stopped without an error, including when the
/// drain deadline cut the connections still open.
///
/// # Errors
///
/// Returns [`ServerError::Serve`] when a listener stopped with an I/O error and
/// [`ServerError::HttpTask`] when its task panicked or was cancelled. Each
/// failure is logged as it happens and the first one is returned.
async fn drain_http(http: Vec<JoinHandle<Result<(), std::io::Error>>>) -> Result<(), ServerError> {
    drain_http_within(http, HTTP_DRAIN_TIMEOUT).await
}

/// Waits for the HTTP tasks, bounded by one `deadline` for all of them.
///
/// # Parameters
///
/// - `http`: the tasks serving the listeners.
/// - `deadline`: how long they get to finish the responses in flight.
///
/// # Returns
///
/// `Ok(())` when every listener stopped within the deadline, and when the
/// deadline cut the rest.
///
/// # Errors
///
/// Returns [`ServerError::Serve`] when a listener stopped with an I/O error and
/// [`ServerError::HttpTask`] when its task panicked or was cancelled.
async fn drain_http_within(
    http: Vec<JoinHandle<Result<(), std::io::Error>>>,
    deadline: Duration,
) -> Result<(), ServerError> {
    let expiry = tokio::time::Instant::now() + deadline;
    let mut failure = None;
    for mut listener in http {
        let left = expiry.saturating_duration_since(tokio::time::Instant::now());
        let expiry = tokio::time::sleep(left);
        tokio::pin!(expiry);
        let finished = tokio::select! {
            finished = &mut listener => Some(finished),
            () = &mut expiry => None,
        };
        let error = match finished {
            None => {
                tracing::warn!(
                    deadline_ms = deadline.as_millis() as u64,
                    "the listener did not finish its responses within the shutdown deadline; the connections were cut",
                );
                listener.abort();
                None
            }
            Some(Ok(Ok(()))) => None,
            Some(Ok(Err(source))) => Some(ServerError::Serve(source)),
            Some(Err(error)) => Some(ServerError::HttpTask(error)),
        };
        if let Some(error) = error {
            tracing::error!(%error, "a listener stopped with an error");
            failure.get_or_insert(error);
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Joins every worker, reporting the ones that did not finish cleanly.
///
/// # Parameters
///
/// - `workers`: the worker tasks with the names they are reported under.
///
/// # Returns
///
/// `Ok(())` when every worker finished on its own.
///
/// # Errors
///
/// Returns [`ServerError::Workers`] with how many workers failed; each failure
/// is logged first, so the log names the worker that panicked.
async fn stop_workers(workers: Vec<(&'static str, JoinHandle<()>)>) -> Result<(), ServerError> {
    let mut failed = 0_usize;
    for (name, worker) in workers {
        if let Err(error) = worker.await {
            tracing::error!(worker = name, %error, "a server worker did not finish");
            failed += 1;
        }
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(ServerError::Workers { failed })
    }
}

/// Wakes the storage maintenance worker whenever a file arrives.
///
/// An import that lands in the cache, or a track added to the library by an
/// import or an upload, can push the cache over its limits, so each such event
/// asks for a maintenance pass. The worker's own debounce merges a burst of
/// imports into one pass. A subscriber that falls behind skips to the newest
/// events and asks for one pass, which covers whatever it missed.
///
/// # Parameters
///
/// - `events`: the bus the imports publish on.
/// - `signal`: the maintenance worker's signal.
/// - `shutdown`: ends the loop.
async fn wake_on_new_files(events: Arc<EventBus>, signal: MaintenanceSignal, shutdown: Shutdown) {
    let mut receiver = events.subscribe();
    loop {
        let received = tokio::select! {
            () = shutdown.wait() => return,
            received = receiver.recv() => received,
        };
        match received {
            Ok(envelope) => match envelope.event {
                ServerEvent::TrackAvailability(TrackAvailability {
                    availability: Availability::Cached,
                    ..
                })
                | ServerEvent::LibraryChanged(LibraryChanged {
                    change: LibraryChange::Added,
                    ..
                }) => signal.notify(),
                _ => {}
            },
            Err(broadcast::error::RecvError::Lagged(_)) => signal.notify(),
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Watches the folders flagged for watching and scans every folder once.
///
/// A filesystem watcher only reports what happens while it runs, and the `watch`
/// switch controls live detection rather than whether a folder is ever indexed,
/// so one incremental scan of every folder is requested when the server starts:
/// whatever changed while it was stopped is found then, watched or not. The
/// scan is incremental, so an unchanged folder costs a walk and no probes.
///
/// The scan queues the entities it changes for the deep index, so a restart
/// re-fetches what changed and leaves everything else stored.
///
/// # Parameters
///
/// - `services`: the services the watcher and the scan requests come from.
/// - `shutdown`: the signal that stops the watcher.
async fn watch_folders(services: Arc<Services>, shutdown: Shutdown) {
    let watcher = services.watcher();
    if let Err(error) = watcher.reconcile().await {
        tracing::error!(%error, "could not reconcile the folder watchers");
    }
    {
        let requested = with_retries(
            &STARTUP_SCAN_DELAYS,
            &shutdown,
            "could not list the library folders for a startup scan",
            |_attempt| {
                let services = Arc::clone(&services);
                async move {
                    services
                        .scanner()
                        .request_scan(services.queue(), &ScanRequest::all())
                        .await
                }
            },
        )
        .await;
        if requested.is_none() {
            tracing::error!(
                attempts = STARTUP_SCAN_DELAYS.len() + 1,
                "the startup scan was skipped: the library folders could not be listed",
            );
        }
    }

    // One sweep at startup, so a server that was down for a week queues what it
    // missed instead of waiting for the next half hour tick.
    if let Err(error) = services
        .queue()
        .enqueue(hearthsound_jobs::NewJob::new(
            crate::index_jobs::INDEX_SWEEP_JOB_KIND,
        ))
        .await
    {
        tracing::warn!(%error, "could not queue the startup index sweep");
    }

    shutdown.wait().await;
    watcher.stop();
}

/// Waits for `SIGINT` or `SIGTERM`.
///
/// A signal handler that cannot be installed is logged rather than fatal: the
/// server keeps serving, and the operator stops it some other way. On a platform
/// without unix signals, only `SIGINT` is awaited.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(error) => {
                tracing::error!(%error, "could not listen for SIGTERM; waiting only for SIGINT");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Builds the URL that opens the first-run setup.
///
/// # Parameters
///
/// - `base_url`: the value of `server.base_url`; an empty value derives the
///   address from what the server bound.
/// - `local_addr`: the address the listener is bound to.
/// - `token`: the setup gate's token.
///
/// # Returns
///
/// The URL, with the token in its query string. A server bound to an
/// unspecified address is reachable as `localhost` on the port it bound, and an
/// IPv6 address appears in brackets.
fn setup_url(base_url: &str, local_addr: std::net::SocketAddr, token: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if !base.is_empty() {
        return format!("{base}/setup?token={token}");
    }
    if local_addr.ip().is_unspecified() {
        return format!("http://localhost:{}/setup?token={token}", local_addr.port());
    }
    format!("http://{local_addr}/setup?token={token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_listener_that_stopped_with_an_io_error_is_reported() {
        let http = tokio::spawn(async { Err(std::io::Error::other("listener")) });
        let drained = drain_http(vec![http]).await;
        assert!(
            matches!(drained, Err(ServerError::Serve(_))),
            "an I/O error must be reported as a serve failure"
        );
    }

    #[tokio::test]
    async fn a_listener_that_never_finishes_is_cut_after_the_deadline() {
        let http = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok(())
        });
        let drained = drain_http_within(vec![http], Duration::from_millis(50)).await;
        assert!(
            drained.is_ok(),
            "cutting the connections is the deadline doing its job"
        );
    }

    #[tokio::test]
    async fn a_panicked_worker_is_reported() {
        let workers: Vec<(&'static str, JoinHandle<()>)> = vec![
            ("panicking", tokio::spawn(async { panic!("worker") })),
            ("finishing", tokio::spawn(async {})),
        ];
        let stopped = stop_workers(workers).await;
        assert!(
            matches!(stopped, Err(ServerError::Workers { failed: 1 })),
            "one panicked worker must be reported"
        );
    }

    #[tokio::test]
    async fn the_startup_folder_listing_is_retried() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let shutdown = Shutdown::new();
        let value = with_retries(
            &[Duration::from_millis(1); 4],
            &shutdown,
            "could not list the library folders for a startup scan",
            |_attempt| {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                async move {
                    if call < 3 {
                        Err("the database is busy")
                    } else {
                        Ok(call)
                    }
                }
            },
        )
        .await;
        assert_eq!(value, Some(3), "the third attempt succeeds");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_listing_that_never_succeeds_is_given_up_on() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let shutdown = Shutdown::new();
        let value: Option<()> = with_retries(
            &[Duration::from_millis(1); 2],
            &shutdown,
            "could not list the library folders for a startup scan",
            |_attempt| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { Err("still busy") }
            },
        )
        .await;
        assert_eq!(value, None);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "a retry schedule of two delays allows three attempts"
        );
    }

    #[tokio::test]
    async fn a_stopping_process_abandons_the_retries() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let shutdown = Shutdown::new();
        shutdown.trigger();
        let value: Option<()> = with_retries(
            &[Duration::from_secs(30); 4],
            &shutdown,
            "could not list the library folders for a startup scan",
            |_attempt| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { Err("still busy") }
            },
        )
        .await;
        assert_eq!(value, None);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the stop signal ends the retries before the attempts are spent"
        );
    }

    #[test]
    fn the_setup_url_follows_the_configured_base_url() {
        let addr: std::net::SocketAddr = "127.0.0.1:4327".parse().unwrap();
        assert_eq!(
            setup_url("https://music.example.com/", addr, "abc"),
            "https://music.example.com/setup?token=abc"
        );
        assert_eq!(
            setup_url("https://music.example.com", addr, "abc"),
            "https://music.example.com/setup?token=abc"
        );
    }

    #[test]
    fn the_setup_url_derives_the_address_without_a_base_url() {
        let anywhere: std::net::SocketAddr = "0.0.0.0:4327".parse().unwrap();
        assert_eq!(
            setup_url("", anywhere, "abc"),
            "http://localhost:4327/setup?token=abc"
        );

        let v6_any: std::net::SocketAddr = "[::]:4327".parse().unwrap();
        assert_eq!(
            setup_url("", v6_any, "abc"),
            "http://localhost:4327/setup?token=abc"
        );

        let loopback: std::net::SocketAddr = "127.0.0.1:4327".parse().unwrap();
        assert_eq!(
            setup_url("", loopback, "abc"),
            "http://127.0.0.1:4327/setup?token=abc"
        );

        let v6: std::net::SocketAddr = "[::1]:4327".parse().unwrap();
        assert_eq!(
            setup_url("", v6, "abc"),
            "http://[::1]:4327/setup?token=abc"
        );
    }
}
