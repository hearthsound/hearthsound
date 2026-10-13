//! The serving process: the HTTP listener, the background workers, shutdown.
//!
//! [`Server::start`] binds the configured address, spawns the job runner, the
//! scheduler, and the worker that reacts to writes made by other connections,
//! and serves the axum application. [`Server::run`] adds the signal handling a
//! deployment needs: it starts, waits for `SIGINT` or `SIGTERM`, and then stops
//! the listener and every worker.
//!
//! Shutdown is orderly in one direction: the HTTP server stops accepting new
//! connections and finishes the responses in flight, the job runner finishes the
//! jobs it holds (up to its shutdown grace period), the scheduler stops between
//! ticks, and the data-version watcher stops polling. A handler abandoned by the
//! runner's grace period keeps its claim and runs again after its lock expires,
//! which is why handlers must tolerate running twice.

use std::sync::Arc;

use hearthsound_jobs::Shutdown;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::error::ServerError;
use crate::services::Services;

/// A running server: the HTTP listener plus the background workers.
pub struct Server {
    /// The services the listener and the workers share.
    services: Arc<Services>,
    /// The signal every worker and the HTTP server stop on.
    shutdown: Shutdown,
    /// The address the listener is bound to.
    local_addr: std::net::SocketAddr,
    /// The task serving HTTP.
    http: JoinHandle<Result<(), std::io::Error>>,
    /// The tasks running the job runner, the scheduler, and the watcher.
    workers: Vec<JoinHandle<()>>,
}

impl Server {
    /// Binds the configured address and starts serving.
    ///
    /// # Parameters
    ///
    /// - `services`: the composed services.
    ///
    /// # Returns
    ///
    /// The running server, whose [`Server::local_addr`] reports the bound
    /// address.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Listen`] when the configured bind address and port
    /// do not resolve, and [`ServerError::Bind`] when the socket cannot be bound
    /// or inspected.
    pub async fn start(services: Arc<Services>) -> Result<Server, ServerError> {
        let addr = services.listen_address()?;
        Server::start_on(services, addr).await
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
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServerError::Bind { addr, source })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| ServerError::Bind { addr, source })?;

        let shutdown = Shutdown::new();
        let app = crate::http::router(&services);

        let http_shutdown = shutdown.clone();
        let http = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { http_shutdown.wait().await })
                .await
        });

        let workers = spawn_workers(&services, shutdown.clone());

        tracing::info!(address = %local_addr, "serving");

        Ok(Server {
            services,
            shutdown,
            local_addr,
            http,
            workers,
        })
    }

    /// Returns the address the listener is bound to.
    ///
    /// # Returns
    ///
    /// The bound address, with the port the operating system chose when the
    /// requested port was zero.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Returns the services the server is running.
    pub fn services(&self) -> &Arc<Services> {
        &self.services
    }

    /// Stops serving and waits for every worker to finish.
    ///
    /// The HTTP server drains the requests in flight; the job runner finishes or
    /// abandons the jobs it holds according to its grace period; the scheduler
    /// and the watcher stop between ticks.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # async fn example(services: Arc<hearthsound_server::Services>) -> Result<(), Box<dyn std::error::Error>> {
    /// let server = hearthsound_server::Server::start_on(services, "127.0.0.1:0".parse()?).await?;
    /// server.shutdown().await;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn shutdown(self) {
        self.shutdown.trigger();
        if let Err(error) = self.http.await {
            tracing::error!(%error, "the HTTP task did not finish cleanly");
        }
        for worker in self.workers {
            let _ = worker.await;
        }
    }

    /// Waits until the HTTP server stops, then for every worker.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the listener stopped without an error.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Serve`] when the HTTP server stopped with an I/O
    /// error and [`ServerError::HttpTask`] when its task panicked or was
    /// cancelled.
    pub async fn wait(self) -> Result<(), ServerError> {
        let Server { http, workers, .. } = self;
        let served = http.await.map_err(ServerError::HttpTask)?;
        for worker in workers {
            let _ = worker.await;
        }
        served.map_err(ServerError::Serve)
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
    /// Returns the errors of [`Server::start`] and [`Server::wait`].
    pub async fn run(services: Arc<Services>) -> Result<(), ServerError> {
        let server = Server::start(services).await?;
        wait_for_signal().await;
        tracing::info!("a termination signal arrived; shutting down");
        server.shutdown().await;
        Ok(())
    }
}

/// Spawns the job runner, the scheduler, and the data-version watcher.
///
/// # Parameters
///
/// - `services`: the composed services.
/// - `shutdown`: the signal the workers stop on.
///
/// # Returns
///
/// The tasks, in spawn order.
fn spawn_workers(services: &Arc<Services>, shutdown: Shutdown) -> Vec<JoinHandle<()>> {
    let runner = Arc::clone(services.runner());
    let runner_shutdown = shutdown.clone();
    let runner_task = tokio::spawn(async move { runner.run(runner_shutdown).await });

    let scheduler = Arc::clone(services.scheduler());
    let scheduler_shutdown = shutdown.clone();
    let scheduler_task = tokio::spawn(async move { scheduler.run(scheduler_shutdown).await });

    vec![runner_task, scheduler_task]
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
