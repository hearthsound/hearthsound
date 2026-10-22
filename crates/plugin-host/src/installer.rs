//! Installing plugins from indexes and repository URLs.
//!
//! [`PluginInstaller`] is the way into the host for a plugin the server does not
//! already hold as a file. It resolves where the plugin comes from - the
//! official index, a repository an administrator registered, or a repository URL
//! that is registered on demand - reads the index, picks the version to install,
//! downloads it, and checks the download against the digest and size the index
//! declares before [`PluginAdmin::install_archive`] grants its permissions and
//! enables it. An index can therefore distribute a plugin without being trusted
//! with anything but the bytes it publishes.
//!
//! # What a download leaves behind
//!
//! The archive is written to a hidden temporary file inside the plugins
//! directory and removed again on every outcome, so a refused or failed install
//! leaves no archive for a later run to pick up, and nothing that looks like an
//! installed version. The only record of an attempt is the repository row's
//! last-fetch time and error, which is what an administrator reads to see
//! whether a repository still answers.
//!
//! # Repository references
//!
//! [`RepositoryRef`] names the source. The official index is always the
//! compiled-in one and is always verified. A repository by id must already be
//! registered and enabled. A repository by URL is registered when it is new,
//! which `plugins.allow_third_party_repositories` gates; an administrator who
//! switched that setting off gets [`InstallError::RepositoriesDisabled`] rather
//! than a new row.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::new_uuid_v7;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::admin::{PluginAdmin, to_hex};
use crate::error::InstallError;
use crate::index::{Index, IndexClient, IndexPlugin, IndexVersion, Unavailable};
use crate::install::PluginRegistration;
use crate::manifest::SUPPORTED_API;
use crate::repo::{PluginRepository, PluginSource, RepoError, RepositoryRow, now_millis};

/// How long one plugin archive download may take in total.
///
/// The limit is the one the fixture downloader builds its plugins with, so a
/// development machine and a server accept the same archives.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// The largest plugin archive the installer accepts.
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

/// Where an administrator installs a plugin from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryRef {
    /// The official plugin index.
    Official,
    /// A registered repository, by its id.
    Id(Uuid),
    /// A repository by the URL of its index document. It is registered when it
    /// is not known yet.
    Url(String),
}

/// Installs plugins the server downloads from an index.
///
/// One installer serves every source: it holds the admin write path, the
/// repository rows, the index client, the HTTP client archives are downloaded
/// with, and the directory installs live in.
pub struct PluginInstaller {
    /// The administrative install path every archive goes through.
    admin: Arc<PluginAdmin>,
    /// The repository rows: read for the repositories a reference names,
    /// written when a URL is registered on demand, and updated with the outcome
    /// of every fetch.
    repository: PluginRepository,
    /// The client that downloads and verifies index documents.
    index: IndexClient,
    /// The client archives are downloaded with.
    http: reqwest::Client,
    /// The directory installs live in; downloads are staged inside it.
    plugins_dir: PathBuf,
}

impl PluginInstaller {
    /// Creates an installer.
    ///
    /// # Parameters
    ///
    /// - `admin`: the administrative install path every archive goes through.
    /// - `repository`: the plugin repository rows.
    /// - `index`: the client that downloads and verifies index documents.
    /// - `http`: the client archives are downloaded with.
    /// - `plugins_dir`: the directory installed plugins live under. Downloads
    ///   are staged in it as `<plugins_dir>/.download-<uuid>` before they are
    ///   installed, and removed from it again.
    pub fn new(
        admin: Arc<PluginAdmin>,
        repository: PluginRepository,
        index: IndexClient,
        http: reqwest::Client,
        plugins_dir: PathBuf,
    ) -> PluginInstaller {
        PluginInstaller {
            admin,
            repository,
            index,
            http,
            plugins_dir,
        }
    }

    /// Returns the settings the installer reads its gates from.
    ///
    /// The snapshot is the plugin host's, so `plugins.*` settings are read the
    /// same way the host reads them.
    pub(crate) fn settings(&self) -> &Arc<SettingsStore> {
        self.admin.host().settings()
    }

    /// Installs a plugin an index publishes.
    ///
    /// The index is read first, the version picked from it, the archive
    /// downloaded to a temporary file inside the plugins directory, and the
    /// download's size and SHA-256 checked against the index entry before the
    /// archive is installed. The plugin is granted the permissions its
    /// manifest requests and enabled; an upgrade that adds a permission is
    /// staged for review instead.
    ///
    /// # Parameters
    ///
    /// - `plugin_id`: the plugin id from the plugin's manifest.
    /// - `version`: the version to install, or `None` for the newest version
    ///   that targets the API this server hosts.
    /// - `repository`: where to install from, `None` for the official index.
    ///
    /// # Returns
    ///
    /// The registration of the installed plugin.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::RepositoriesDisabled`] when a URL that is not
    /// registered yet is given while `plugins.allow_third_party_repositories` is
    /// off, [`InstallError::RepositoryDisabled`] when the named repository is
    /// switched off, [`InstallError::UnknownRepository`] when no repository has
    /// the given id, [`InstallError::Index`] when the index cannot be read or
    /// its signature does not verify, [`InstallError::UnknownPlugin`] when the
    /// index lists no such plugin, [`InstallError::Unavailable`] when the index
    /// cannot offer a version this server hosts,
    /// [`InstallError::UnreadableDigest`] when the entry declares no readable
    /// digest, [`InstallError::Download`] when the archive cannot be
    /// downloaded, [`InstallError::SizeMismatch`] and
    /// [`InstallError::IntegrityFailed`] when the download is not what the index
    /// declares, and the admin's install errors otherwise - for example
    /// [`InstallError::AlreadyInstalled`] for a version already installed.
    pub async fn install_from_index(
        &self,
        plugin_id: &str,
        version: Option<&str>,
        repository: Option<RepositoryRef>,
    ) -> Result<PluginRegistration, InstallError> {
        let source = self.resolve(repository).await?;
        let entry = source
            .index
            .plugin(plugin_id)
            .ok_or_else(|| InstallError::UnknownPlugin {
                id: plugin_id.to_owned(),
                index: source.url.clone(),
            })?;
        let version = select(entry, version).map_err(InstallError::from)?;
        let digest = version
            .sha256_digest()
            .ok_or(InstallError::UnreadableDigest {
                id: plugin_id.to_owned(),
                version: version.version.clone(),
                index: source.url.clone(),
            })?;

        let bytes = self.download(&version.url).await?;
        if bytes.len() as u64 != version.size {
            return Err(InstallError::SizeMismatch {
                declared: version.size,
                received: bytes.len() as u64,
            });
        }
        let actual: [u8; 32] = Sha256::digest(&bytes).into();
        if actual != digest {
            return Err(InstallError::IntegrityFailed {
                expected: to_hex(&digest),
                actual: to_hex(&actual),
            });
        }

        // The guard removes the download when it leaves this scope, whichever
        // way the install ends.
        let download = Download::write(&self.plugins_dir, &bytes).await?;
        self.admin
            .install_archive(
                download.path(),
                Some(digest),
                source.source(),
                source.repository_id(),
            )
            .await
    }

    /// Installs a plugin from a repository's index URL.
    ///
    /// The repository is registered when it is new, which
    /// `plugins.allow_third_party_repositories` gates, and used as it stands
    /// when it is already known.
    ///
    /// # Parameters
    ///
    /// - `plugin_id`: the plugin id from the plugin's manifest.
    /// - `url`: the URL of the repository's index document.
    /// - `version`: the version to install, or `None` for the newest version
    ///   that targets the API this server hosts.
    ///
    /// # Returns
    ///
    /// The registration of the installed plugin.
    ///
    /// # Errors
    ///
    /// Returns what [`PluginInstaller::install_from_index`] documents for an
    /// install from a repository URL.
    pub async fn install_from_url(
        &self,
        plugin_id: &str,
        url: &str,
        version: Option<&str>,
    ) -> Result<PluginRegistration, InstallError> {
        self.install_from_index(plugin_id, version, Some(RepositoryRef::Url(url.to_owned())))
            .await
    }

    /// Lists every plugin the reachable indexes offer.
    ///
    /// A source that cannot be read is reported in the report's errors and does
    /// not stop the listing, so one unreachable repository cannot hide the
    /// official index or the repositories that do answer.
    ///
    /// # Parameters
    ///
    /// - `repository`: the sources to list. `None` covers the official index and
    ///   every enabled repository the administrator registered; a reference
    ///   covers that source alone, registering a new URL when
    ///   `plugins.allow_third_party_repositories` is on.
    ///
    /// # Returns
    ///
    /// The plugins, one entry per index that lists the plugin, and the sources
    /// that could not be read.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::Record`] when the repository rows cannot be read
    /// and [`InstallError::UnknownRepository`] when a repository id names no
    /// row. The failure of an index itself is reported in the report's errors
    /// instead.
    pub async fn available(
        &self,
        repository: Option<RepositoryRef>,
    ) -> Result<AvailableReport, InstallError> {
        let mut report = AvailableReport::default();
        for source in self.references(repository).await? {
            match self.resolve(Some(source.reference)).await {
                Ok(resolved) => report.merge(resolved),
                Err(error) => report.errors.push(AvailableError {
                    source: source.url,
                    detail: error.to_string(),
                }),
            }
        }
        Ok(report)
    }

    /// Reads the index one reference names.
    ///
    /// A repository's index fetch is recorded on its row, so a listing updates
    /// `last_fetched_at` and `last_error` as an install does.
    ///
    /// # Parameters
    ///
    /// - `reference`: the index to read, `None` for the official index.
    ///
    /// # Returns
    ///
    /// The index and the repository row it came from, when it is not the
    /// official index.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::RepositoriesDisabled`],
    /// [`InstallError::RepositoryDisabled`] and
    /// [`InstallError::UnknownRepository`] for a reference that cannot be
    /// resolved, [`InstallError::Index`] when the document cannot be downloaded
    /// or its signature does not verify, and [`InstallError::Record`] when a
    /// repository row cannot be read or written.
    pub(crate) async fn resolve(
        &self,
        reference: Option<RepositoryRef>,
    ) -> Result<ResolvedSource, InstallError> {
        match reference.unwrap_or(RepositoryRef::Official) {
            RepositoryRef::Official => {
                let url = self.index.official_url().to_owned();
                let index = self.index.fetch(&url).await?;
                Ok(ResolvedSource {
                    url,
                    index,
                    repository: None,
                })
            }
            RepositoryRef::Id(id) => {
                let row = self
                    .repository
                    .repository(id)
                    .await?
                    .ok_or(InstallError::UnknownRepository { id })?;
                self.fetch(row).await
            }
            RepositoryRef::Url(url) => {
                let row = self.repository_for_url(&url).await?;
                self.fetch(row).await
            }
        }
    }

    /// Downloads and verifies a repository's index, recording the outcome.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::RepositoryDisabled`] for a repository that is
    /// switched off, [`InstallError::Index`] when the document cannot be
    /// downloaded or its signature does not verify, and
    /// [`InstallError::Record`] when the row cannot be written.
    async fn fetch(&self, row: RepositoryRow) -> Result<ResolvedSource, InstallError> {
        if !row.enabled {
            return Err(InstallError::RepositoryDisabled { url: row.url });
        }
        match self
            .index
            .fetch_with_key(&row.url, row.public_key.as_deref())
            .await
        {
            Ok(index) => {
                self.repository
                    .record_fetch(row.id, now_millis(), None)
                    .await?;
                Ok(ResolvedSource {
                    url: row.url.clone(),
                    index,
                    repository: Some(row),
                })
            }
            Err(error) => {
                self.repository
                    .record_fetch(row.id, now_millis(), Some(&error.to_string()))
                    .await?;
                Err(InstallError::Index(error))
            }
        }
    }

    /// Returns the repository registered under `url`, registering it when it is
    /// new.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::RepositoriesDisabled`] when the URL is not
    /// registered yet and `plugins.allow_third_party_repositories` is off, and
    /// [`InstallError::Record`] when the row cannot be read or written.
    async fn repository_for_url(&self, url: &str) -> Result<RepositoryRow, InstallError> {
        if let Some(row) = self.repository.repository_by_url(url).await? {
            return Ok(row);
        }
        if !self
            .settings()
            .get(keys::PLUGINS_ALLOW_THIRD_PARTY_REPOSITORIES)?
        {
            return Err(InstallError::RepositoriesDisabled);
        }
        match self
            .repository
            .insert_repository(url, None, None, true)
            .await
        {
            Ok(row) => Ok(row),
            // Two callers can register the same URL at once; the row the other
            // one wrote is the one to use.
            Err(RepoError::DuplicateUrl(_)) => {
                match self.repository.repository_by_url(url).await? {
                    Some(row) => Ok(row),
                    // The row was removed between the write and the read, so the
                    // duplicate the insert reported is what stands.
                    None => Err(InstallError::Record(RepoError::DuplicateUrl(
                        url.to_owned(),
                    ))),
                }
            }
            Err(error) => Err(InstallError::Record(error)),
        }
    }

    /// Lists the indexes a reference covers, with the URL to report them by.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::UnknownRepository`] when a repository id names no
    /// row and [`InstallError::Record`] when the rows cannot be read.
    async fn references(
        &self,
        repository: Option<RepositoryRef>,
    ) -> Result<Vec<IndexRef>, InstallError> {
        let official_uri = self.index.official_url().to_owned();
        Ok(match repository {
            Some(RepositoryRef::Official) => vec![IndexRef {
                url: official_uri,
                reference: RepositoryRef::Official,
            }],
            Some(RepositoryRef::Id(id)) => vec![IndexRef {
                url: self
                    .repository
                    .repository(id)
                    .await?
                    .ok_or(InstallError::UnknownRepository { id })?
                    .url,
                reference: RepositoryRef::Id(id),
            }],
            Some(RepositoryRef::Url(url)) => vec![IndexRef {
                url: url.clone(),
                reference: RepositoryRef::Url(url),
            }],
            None => {
                let mut references = vec![IndexRef {
                    url: official_uri,
                    reference: RepositoryRef::Official,
                }];
                for row in self.repository.repositories().await? {
                    if row.enabled {
                        references.push(IndexRef {
                            url: row.url,
                            reference: RepositoryRef::Id(row.id),
                        });
                    }
                }
                references
            }
        })
    }

    /// Downloads one archive.
    ///
    /// # Parameters
    ///
    /// - `url`: the URL the index declares for the version.
    ///
    /// # Returns
    ///
    /// The archive's bytes.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::Download`] when the request fails, the server
    /// answers anything but a success, or the archive is larger than
    /// [`MAX_ARCHIVE_BYTES`].
    async fn download(&self, url: &str) -> Result<Vec<u8>, InstallError> {
        let failure = |reason: String| InstallError::Download {
            url: url.to_owned(),
            reason,
        };
        let mut response = self
            .http
            .get(url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|error| failure(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(failure(format!("the server answered {status}")));
        }
        if let Some(length) = response.content_length()
            && length > MAX_ARCHIVE_BYTES
        {
            return Err(failure(format!(
                "the archive declares {length} bytes, above the {MAX_ARCHIVE_BYTES} byte limit"
            )));
        }

        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| failure(error.to_string()))?
        {
            if body.len() as u64 + chunk.len() as u64 > MAX_ARCHIVE_BYTES {
                return Err(failure(format!(
                    "the archive is larger than the {MAX_ARCHIVE_BYTES} byte limit"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// One index a plugin can be installed from.
pub(crate) struct ResolvedSource {
    /// The URL the index document was fetched from.
    pub(crate) url: String,
    /// The index document.
    pub(crate) index: Index,
    /// The repository the document came from, `None` for the official index.
    pub(crate) repository: Option<RepositoryRow>,
}

impl ResolvedSource {
    /// Returns the source an install from this index records.
    ///
    /// # Returns
    ///
    /// [`PluginSource::Official`] for the official index and
    /// [`PluginSource::Repository`] for a repository an administrator added.
    pub(crate) fn source(&self) -> PluginSource {
        match self.repository {
            Some(_) => PluginSource::Repository,
            None => PluginSource::Official,
        }
    }

    /// Returns the repository the index came from.
    ///
    /// # Returns
    ///
    /// The repository id, or `None` for the official index.
    pub(crate) fn repository_id(&self) -> Option<Uuid> {
        self.repository.as_ref().map(|row| row.id)
    }

    /// Reports whether the index's signature was verified.
    ///
    /// # Returns
    ///
    /// `true` for the official index, for a repository whose administrator
    /// supplied a public key, and `false` for an unsigned repository, whose
    /// plugins are shown as unverified.
    fn verified(&self) -> bool {
        self.repository
            .as_ref()
            .is_none_or(|row| row.public_key.is_some())
    }
}

/// One index a listing covers.
struct IndexRef {
    /// The URL to report the source by when it cannot be read.
    url: String,
    /// Where to fetch it from.
    reference: RepositoryRef,
}

/// Everything an administrator can install right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AvailableReport {
    /// The plugins the reachable indexes offer, in the order the indexes list
    /// them: the official index first, then the repositories by URL. A plugin
    /// several sources offer appears once per source.
    pub plugins: Vec<AvailablePlugin>,
    /// The indexes that could not be read.
    pub errors: Vec<AvailableError>,
}

impl AvailableReport {
    /// Adds every plugin one index offers.
    ///
    /// # Parameters
    ///
    /// - `source`: the index to list, with the repository it came from.
    fn merge(&mut self, source: ResolvedSource) {
        let repository_id = source.repository_id();
        let verified = source.verified();
        for plugin in &source.index.plugins {
            self.plugins.push(AvailablePlugin {
                id: plugin.id.clone(),
                name: plugin.name.clone(),
                description: plugin.description.clone(),
                provides: plugin.provides.clone(),
                homepage: plugin.homepage.clone(),
                license: plugin.license.clone(),
                official: plugin.official,
                verified,
                repository_id,
                version: plugin
                    .newest_compatible()
                    .map(|version| version.version.clone()),
                unavailable: plugin.unavailable(),
            });
        }
    }
}

/// One plugin one index offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailablePlugin {
    /// The plugin id from the plugin's manifest.
    pub id: String,
    /// The display name.
    pub name: String,
    /// The one-line description.
    pub description: String,
    /// The layers the plugin provides, as the manifest spells them.
    pub provides: Vec<String>,
    /// The project's home page.
    pub homepage: String,
    /// The license the plugin is distributed under.
    pub license: String,
    /// Whether the index says the project itself publishes the plugin.
    pub official: bool,
    /// Whether the signature of the index it is listed by was verified.
    pub verified: bool,
    /// The repository the plugin comes from, `None` for the official index.
    pub repository_id: Option<Uuid>,
    /// The newest version this server can install, when the index offers one.
    pub version: Option<String>,
    /// Why the plugin cannot be installed here, when it cannot.
    pub unavailable: Option<Unavailable>,
}

/// One index a listing could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableError {
    /// The URL of the index that could not be read.
    pub source: String,
    /// Why it could not be read.
    pub detail: String,
}

/// A downloaded archive that is removed when it is dropped.
struct Download {
    /// The temporary file the archive was written to.
    path: PathBuf,
}

impl Download {
    /// Writes an archive to a hidden temporary file inside the plugins
    /// directory.
    ///
    /// # Parameters
    ///
    /// - `plugins_dir`: the directory installed plugins live under; it is
    ///   created when it does not exist yet.
    /// - `bytes`: the archive.
    ///
    /// # Returns
    ///
    /// The download, which removes its file when it is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::Io`] when the directory or the file cannot be
    /// written.
    async fn write(plugins_dir: &Path, bytes: &[u8]) -> Result<Download, InstallError> {
        tokio::fs::create_dir_all(plugins_dir)
            .await
            .map_err(|source| InstallError::Io {
                path: plugins_dir.to_path_buf(),
                source,
            })?;
        let path = plugins_dir.join(format!(".download-{}", new_uuid_v7()));
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|source| InstallError::Io {
                path: path.clone(),
                source,
            })?;
        Ok(Download { path })
    }

    /// Returns the path of the downloaded archive.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Download {
    /// Removes the downloaded archive.
    ///
    /// A file that is already gone is not an error, and a removal that fails is
    /// logged rather than reported: the install it belongs to has already been
    /// answered, and the next install overwrites nothing.
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "could not remove a plugin download"
            );
        }
    }
}

/// Picks the version to install out of one plugin's index entry.
///
/// # Parameters
///
/// - `entry`: the plugin's entry in the index.
/// - `version`: the requested version, or `None` for the newest version that
///   targets the API this server hosts.
///
/// # Returns
///
/// The version entry to install.
///
/// # Errors
///
/// Returns [`Unavailable::NoSuchVersion`] when the index publishes no such
/// version and [`Unavailable::NoCompatibleVersion`] when the requested entry,
/// or every entry, targets another API version.
fn select<'entry>(
    entry: &'entry IndexPlugin,
    version: Option<&str>,
) -> Result<&'entry IndexVersion, Unavailable> {
    let Some(requested) = version else {
        return entry.newest_compatible().ok_or_else(|| incompatible(entry));
    };
    let listed = entry
        .version(requested)
        .ok_or_else(|| Unavailable::NoSuchVersion {
            requested: requested.to_owned(),
            offered: entry
                .versions
                .iter()
                .map(|version| version.version.clone())
                .collect(),
        })?;
    if listed.api != SUPPORTED_API {
        return Err(incompatible(entry));
    }
    Ok(listed)
}

/// Reports why one index entry offers no installable version.
///
/// # Parameters
///
/// - `entry`: the plugin's entry in the index.
///
/// # Returns
///
/// The reason, carrying the API versions the entry offers.
fn incompatible(entry: &IndexPlugin) -> Unavailable {
    Unavailable::NoCompatibleVersion {
        supported: SUPPORTED_API,
        offered: entry
            .versions
            .iter()
            .map(|version| version.api.clone())
            .collect(),
    }
}
