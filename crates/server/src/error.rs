//! The error type of the composition root.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Something that stopped the server from starting or from serving.
///
/// Every variant names the step that failed, so an operator reading the log
/// line knows whether to fix a path, the database, a setting, the secret key, or
/// the listening address.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// A directory of the data or storage tree could not be created.
    #[error("could not create directory `{path}`: {source}")]
    Directory {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The database could not be opened, migrated, or queried.
    #[error(transparent)]
    Database(#[from] hearthsound_db::DbError),
    /// The runtime settings could not be loaded from their layers.
    #[error(transparent)]
    Settings(#[from] hearthsound_config::settings::SettingsError),
    /// The account tables could not be read while the services were composed.
    #[error(transparent)]
    Auth(#[from] hearthsound_auth::AuthError),
    /// A runtime setting could not be read as its declared type.
    #[error(transparent)]
    Setting(#[from] hearthsound_config::ConfigError),
    /// The secret key could not be read, created, or decoded.
    #[error(transparent)]
    Secret(#[from] hearthsound_config::secret::SecretError),
    /// The wasmtime engine, the host linker, or a component could not be built.
    #[error(transparent)]
    PluginHost(#[from] hearthsound_plugin_host::HostError),
    /// The plugin, instance, or layer tables could not be read.
    #[error(transparent)]
    Provider(#[from] hearthsound_plugin_host::RepoError),
    /// The client that downloads plugin indexes and archives could not be
    /// built.
    #[error("could not build the outbound HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
    /// The plugin index client could not be built, which can only happen when
    /// the compiled-in official index key is not a minisign key.
    #[error(transparent)]
    PluginIndex(#[from] hearthsound_plugin_host::IndexError),
    /// A system schedule could not be stored.
    #[error(transparent)]
    Jobs(#[from] hearthsound_jobs::JobsError),
    /// `server.trusted_proxies` holds something that is not an address or CIDR
    /// block.
    #[error("invalid trusted proxy: {0}")]
    Proxy(#[from] hearthsound_auth::InvalidProxy),
    /// `server.cors_origins` holds something that is not `*`, `null`, or a
    /// `scheme://host[:port]` origin.
    #[error(
        "invalid CORS origin `{origin}`: expected `*`, `null`, or `scheme://host[:port]` without a path"
    )]
    CorsOrigin {
        /// The configured value.
        origin: String,
    },
    /// The configured bind address and port do not resolve to a socket address.
    #[error("`{addr}` is not a usable listen address: {source}")]
    Listen {
        /// The `bind:port` value as configured.
        addr: String,
        /// The underlying resolution error.
        #[source]
        source: std::io::Error,
    },
    /// The listening socket could not be bound or inspected.
    #[error("could not listen on {addr}: {source}")]
    Bind {
        /// The address the server tried to bind.
        addr: SocketAddr,
        /// The underlying socket error.
        #[source]
        source: std::io::Error,
    },
    /// The HTTP server stopped with an error.
    #[error("the HTTP server stopped: {0}")]
    Serve(#[source] std::io::Error),
    /// The server stopped but its HTTP task panicked or was cancelled.
    #[error("the HTTP task did not finish: {0}")]
    HttpTask(#[source] tokio::task::JoinError),
}
