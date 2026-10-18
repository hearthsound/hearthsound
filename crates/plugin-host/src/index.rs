//! Plugin indexes and the signatures that authenticate them.
//!
//! A plugin index lists the plugins one repository publishes, with one entry per
//! version of each plugin. The document is JSON ([`Index`]) and, for the
//! official index and for any repository whose administrator supplied a public
//! key, it is also signed with minisign, so the server can tell the repository's
//! own document from one an attacker substituted. [`IndexClient`] downloads
//! `index.json` together with its `index.json.minisig` signature and verifies it
//! before anything reads the list.
//!
//! # Fail-closed rules
//!
//! - A document at the client's official URL is always verified: a missing
//!   signature ([`IndexError::Unsigned`]) and one that does not verify
//!   ([`IndexError::SignatureInvalid`]) both reject the document.
//! - A repository fetch that carries a public key is verified the same way.
//! - A repository fetch without one is accepted unsigned, and the caller reports
//!   its plugins as unverified.
//!
//! # Examples
//!
//! ```
//! use hearthsound_plugin_host::index::Index;
//!
//! let document = r#"{
//!   "formatVersion": 1,
//!   "name": "Example index",
//!   "generatedAt": "2026-01-01T00:00:00Z",
//!   "plugins": []
//! }"#;
//! let index: Index = serde_json::from_str(document).unwrap();
//! assert_eq!(index.format_version, 1);
//! assert_eq!(index.name, "Example index");
//! ```

use std::time::Duration;

use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};

use crate::manifest::{Permissions, SUPPORTED_API};

/// The URL the official plugin index is served at.
///
/// The index is served from the project's pages until the plugins domain serves
/// it. The production URL, `https://plugins.hearthsound.app/v1/index.json`,
/// replaces this constant once that domain answers.
pub const OFFICIAL_INDEX_URL: &str = "https://hearthsound.github.io/plugins/v1/index.json";

/// The minisign public key the official plugin index is signed with.
///
/// The private half is held by the index publisher's build and is never part of
/// this repository.
pub const OFFICIAL_INDEX_PUBLIC_KEY: &str =
    "RWSNpbSdmjDfPZT84aLZeDuF5LxdBt3dieHVj1h+xHQ0aAy0LhH6pURN";

/// The time one index document download may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The most bytes read from one index document.
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;

/// One index document.
///
/// The struct matches the index format v1 exactly; a document that carries an
/// unknown member or a member of the wrong type is rejected as
/// [`IndexError::Malformed`] rather than read in part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Index {
    /// The index format version; only version 1 is defined.
    pub format_version: u32,
    /// The repository's display name.
    pub name: String,
    /// When the index was generated, as an RFC 3339 timestamp.
    pub generated_at: String,
    /// The plugins the index lists.
    pub plugins: Vec<IndexPlugin>,
}

impl Index {
    /// Returns the entry for one plugin.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id from the plugin's manifest.
    ///
    /// # Returns
    ///
    /// The plugin's entry, or `None` when the index lists no such plugin.
    pub fn plugin(&self, id: &str) -> Option<&IndexPlugin> {
        self.plugins.iter().find(|plugin| plugin.id == id)
    }
}

/// One plugin an index lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexPlugin {
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
    /// Whether the Hearthsound organization publishes the plugin.
    ///
    /// A plugin whose repository is not signed and whose entry is not official
    /// is shown to administrators as unverified.
    pub official: bool,
    /// The published versions, in the order the index lists them.
    pub versions: Vec<IndexVersion>,
}

impl IndexPlugin {
    /// Returns the version entry with this exact version string.
    ///
    /// # Parameters
    ///
    /// - `version`: the version to look for, as the index spells it.
    ///
    /// # Returns
    ///
    /// The version entry, or `None` when the index lists no such version.
    pub fn version(&self, version: &str) -> Option<&IndexVersion> {
        self.versions.iter().find(|entry| entry.version == version)
    }

    /// Returns the newest version that targets the API this server hosts.
    ///
    /// A version is compatible when its `api` equals the API version the
    /// manifest module supports. Versions are compared as semantic versions; an
    /// entry whose `version` is not a semantic version is ignored, because the
    /// newest usable version cannot be decided for it.
    ///
    /// # Returns
    ///
    /// The newest compatible version, or `None` when the plugin offers none.
    pub fn newest_compatible(&self) -> Option<&IndexVersion> {
        self.versions
            .iter()
            .filter(|entry| entry.api == SUPPORTED_API)
            .filter_map(|entry| {
                semver::Version::parse(&entry.version)
                    .ok()
                    .map(|version| (version, entry))
            })
            .max_by(|left, right| left.0.cmp(&right.0))
            .map(|(_, entry)| entry)
    }

    /// Returns why this plugin cannot be installed here, when it cannot.
    ///
    /// # Returns
    ///
    /// [`Unavailable::NoCompatibleVersion`] when no version targets the API
    /// version this server hosts, `None` when at least one does.
    pub fn unavailable(&self) -> Option<Unavailable> {
        if self.versions.iter().any(|entry| entry.api == SUPPORTED_API) {
            return None;
        }
        Some(Unavailable::NoCompatibleVersion {
            supported: SUPPORTED_API,
            offered: self
                .versions
                .iter()
                .map(|entry| entry.api.clone())
                .collect(),
        })
    }
}

/// One published version of a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexVersion {
    /// The plugin's version.
    pub version: String,
    /// The plugin API version the component targets.
    pub api: String,
    /// The download URL of the `.hsplugin` archive.
    pub url: String,
    /// The SHA-256 of the archive, as lowercase hexadecimal.
    pub sha256: String,
    /// The archive size in bytes.
    pub size: u64,
    /// When the version was published, as an RFC 3339 timestamp.
    pub published_at: String,
    /// The release notes.
    pub changelog: String,
    /// The permissions the version requests, so an administrator can preview
    /// them before the archive is downloaded.
    pub permissions: Permissions,
}

impl IndexVersion {
    /// Decodes the declared SHA-256 into its thirty-two bytes.
    ///
    /// # Returns
    ///
    /// The digest, or `None` when `sha256` is not sixty-four lowercase
    /// hexadecimal characters. A caller that needs the digest to check a
    /// download must treat `None` as a broken entry rather than a match.
    pub fn sha256_digest(&self) -> Option<[u8; 32]> {
        if self.sha256.len() != 64 {
            return None;
        }
        let text = self.sha256.as_bytes();
        let mut digest = [0u8; 32];
        for (index, slot) in digest.iter_mut().enumerate() {
            let high = hex_digit(text[2 * index])?;
            let low = hex_digit(text[2 * index + 1])?;
            *slot = (high << 4) | low;
        }
        Some(digest)
    }
}

/// Decodes one lowercase hexadecimal digit.
///
/// # Parameters
///
/// - `byte`: the ASCII byte to decode.
///
/// # Returns
///
/// The digit's value, or `None` when the byte is not `0`-`9` or `a`-`f`.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Why an index lists a plugin this server cannot install.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable {
    /// Every version of the plugin targets an API version this server does not
    /// host.
    #[error("no version targets {supported}; the index offers {}", .offered.join(", "))]
    NoCompatibleVersion {
        /// The API version this server hosts.
        supported: &'static str,
        /// The API versions the offered versions target, in index order.
        offered: Vec<String>,
    },
}

/// Why an index could not be read.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The document could not be downloaded.
    #[error("the index at {url} cannot be reached: {reason}")]
    Unreachable {
        /// The URL that was requested.
        url: String,
        /// What went wrong.
        reason: String,
    },
    /// The downloaded document is not an index of the expected shape.
    #[error("the index at {url} is malformed: {reason}")]
    Malformed {
        /// The URL that was requested.
        url: String,
        /// Why the document was rejected.
        reason: String,
    },
    /// The document's signature does not verify with the expected key.
    #[error("the signature of the index at {url} is not valid for this key")]
    SignatureInvalid {
        /// The URL that was requested.
        url: String,
    },
    /// The document carries no signature where one is required.
    #[error("the index at {url} is not signed")]
    Unsigned {
        /// The URL that was requested.
        url: String,
    },
}

/// Downloads plugin indexes and verifies their signatures.
///
/// A client is bound to one repository - the official index by default - whose
/// URL must always be signed. Other URLs are fetched through the same client;
/// they are verified when the caller supplies the repository's public key.
#[derive(Clone)]
pub struct IndexClient {
    http: reqwest::Client,
    official_url: String,
    official_public_key: PublicKey,
}

impl IndexClient {
    /// Creates a client.
    ///
    /// # Parameters
    ///
    /// - `http`: the HTTP client every request is made with.
    /// - `official_url`: the URL of the official index. A document fetched from
    ///   exactly this URL must carry a signature from `official_public_key`.
    /// - `official_public_key`: the minisign public key the official index is
    ///   signed with, usually parsed from [`OFFICIAL_INDEX_PUBLIC_KEY`].
    pub fn new(
        http: reqwest::Client,
        official_url: String,
        official_public_key: PublicKey,
    ) -> IndexClient {
        IndexClient {
            http,
            official_url,
            official_public_key,
        }
    }

    /// Downloads and verifies one index.
    ///
    /// The index document is read from `url` and its signature from
    /// `<url>.minisig`. A document fetched from the client's official URL is
    /// verified against the official key and rejected without a valid signature.
    /// The signature of any other document is read but not checked, because a
    /// public key for that repository is what [`IndexClient::fetch_with_key`]
    /// takes.
    ///
    /// # Parameters
    ///
    /// - `url`: the URL of the index document.
    ///
    /// # Returns
    ///
    /// The verified index.
    ///
    /// # Errors
    ///
    /// - [`IndexError::Unreachable`]: the document was not served, or the
    ///   request failed.
    /// - [`IndexError::Malformed`]: the body is not an index of the expected
    ///   shape, or is larger than the accepted size.
    /// - [`IndexError::Unsigned`] and [`IndexError::SignatureInvalid`]: the
    ///   official index carries no signature, or one that does not verify.
    pub async fn fetch(&self, url: &str) -> Result<Index, IndexError> {
        self.fetch_with_key(url, None).await
    }

    /// Downloads one index and verifies it against a repository's key.
    ///
    /// A document fetched from the client's official URL is always verified
    /// against the official key, whatever `public_key` holds.
    ///
    /// # Parameters
    ///
    /// - `url`: the URL of the index document; its signature is read from
    ///   `<url>.minisig`.
    /// - `public_key`: the repository's minisign public key, either the base64
    ///   key line or the whole `public.minisign` file, or `None` for a
    ///   repository the administrator did not give a key. Text that cannot be
    ///   read as a public key rejects the document as
    ///   [`IndexError::SignatureInvalid`].
    ///
    /// # Returns
    ///
    /// The index. A repository without a key is returned without its signature
    /// being read.
    ///
    /// # Errors
    ///
    /// - [`IndexError::Unreachable`]: the document was not served, or the
    ///   request failed.
    /// - [`IndexError::Malformed`]: the body is not an index of the expected
    ///   shape, or is larger than the accepted size.
    /// - [`IndexError::Unsigned`] and [`IndexError::SignatureInvalid`]: the
    ///   document must be signed and carries no signature, or one that does not
    ///   verify.
    pub async fn fetch_with_key(
        &self,
        url: &str,
        public_key: Option<&str>,
    ) -> Result<Index, IndexError> {
        let official = url == self.official_url;
        let configured = match public_key.filter(|_| !official) {
            Some(text) => {
                Some(
                    parse_public_key(text).ok_or_else(|| IndexError::SignatureInvalid {
                        url: url.to_owned(),
                    })?,
                )
            }
            None => None,
        };
        let key = if official {
            Some(&self.official_public_key)
        } else {
            configured.as_ref()
        };

        let body = self
            .read(url)
            .await?
            .ok_or_else(|| IndexError::Unreachable {
                url: url.to_owned(),
                reason: "the server has no such document".to_owned(),
            })?;
        let index: Index =
            serde_json::from_slice(&body).map_err(|error| IndexError::Malformed {
                url: url.to_owned(),
                reason: error.to_string(),
            })?;

        let Some(key) = key else {
            return Ok(index);
        };
        let signature_url = format!("{url}.minisig");
        let Some(text) = self.read(&signature_url).await? else {
            return Err(IndexError::Unsigned {
                url: url.to_owned(),
            });
        };
        let signature = Signature::decode(&String::from_utf8_lossy(&text)).map_err(|_| {
            IndexError::SignatureInvalid {
                url: url.to_owned(),
            }
        })?;
        // Only signatures over the document's BLAKE2b hash are accepted; the
        // legacy format that older signers produce is not.
        key.verify(&body, &signature, false)
            .map_err(|_| IndexError::SignatureInvalid {
                url: url.to_owned(),
            })?;
        Ok(index)
    }

    /// Reads one document, bounded by the accepted size.
    ///
    /// # Parameters
    ///
    /// - `url`: the URL to request.
    ///
    /// # Returns
    ///
    /// The body, or `None` when the server answers that the document does not
    /// exist.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::Unreachable`] when the request fails or the server
    /// answers anything but a success or a missing document, and
    /// [`IndexError::Malformed`] when the body is larger than the accepted size.
    async fn read(&self, url: &str) -> Result<Option<Vec<u8>>, IndexError> {
        let unreachable = |error: reqwest::Error| IndexError::Unreachable {
            url: url.to_owned(),
            reason: error.to_string(),
        };
        let mut response = self
            .http
            .get(url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(unreachable)?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(IndexError::Unreachable {
                url: url.to_owned(),
                reason: format!("the server answered {status}"),
            });
        }
        if let Some(length) = response.content_length()
            && length > MAX_INDEX_BYTES
        {
            return Err(IndexError::Malformed {
                url: url.to_owned(),
                reason: format!(
                    "the document declares {length} bytes, above the {MAX_INDEX_BYTES} byte limit"
                ),
            });
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(unreachable)? {
            if body.len() as u64 + chunk.len() as u64 > MAX_INDEX_BYTES {
                return Err(IndexError::Malformed {
                    url: url.to_owned(),
                    reason: format!("the document is larger than the {MAX_INDEX_BYTES} byte limit"),
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(body))
    }
}

/// Parses an administrator's minisign public key.
///
/// # Parameters
///
/// - `text`: the base64 key line, or a whole `public.minisign` file whose second
///   line holds it.
///
/// # Returns
///
/// The public key, or `None` when the text is neither.
fn parse_public_key(text: &str) -> Option<PublicKey> {
    let text = text.trim();
    PublicKey::decode(text)
        .or_else(|_| PublicKey::from_base64(text))
        .ok()
}

#[cfg(test)]
mod tests {
    use minisign::KeyPair;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// The path the official index is served at in these tests.
    const OFFICIAL_PATH: &str = "/v1/index.json";

    /// The path a third-party repository's index is served at.
    const REPOSITORY_PATH: &str = "/repo/index.json";

    /// The SHA-256 every fixture document declares.
    const FIXTURE_SHA256: &str = "9c1f0b7a4e2d3c5f8a6b0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a";

    /// One signing key pair the tests publish an index with.
    struct Signer {
        /// The key pair itself.
        pair: KeyPair,
    }

    impl Signer {
        /// Generates a fresh key pair.
        fn new() -> Signer {
            Signer {
                pair: KeyPair::generate_unencrypted_keypair().unwrap(),
            }
        }

        /// Returns the base64 public key of this signer.
        fn public_key(&self) -> String {
            self.pair.pk.to_base64()
        }

        /// Returns the public key as the verifier uses it.
        fn verify_key(&self) -> PublicKey {
            PublicKey::from_base64(&self.public_key()).unwrap()
        }

        /// Signs one document and returns the `.minisig` text.
        fn sign(&self, document: &str) -> String {
            minisign::sign(
                Some(&self.pair.pk),
                &self.pair.sk,
                document.as_bytes(),
                None,
                None,
            )
            .unwrap()
            .to_string()
        }
    }

    /// Builds the HTTP client the tests fetch with.
    fn http() -> reqwest::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::new()
    }

    /// Builds an index document the shape of the format defines.
    fn document(official: bool) -> String {
        format!(
            r#"{{
              "formatVersion": 1,
              "name": "Example index",
              "generatedAt": "2026-01-01T00:00:00Z",
              "plugins": [
                {{
                  "id": "com.example.hello",
                  "name": "Hello",
                  "description": "An example catalog plugin.",
                  "provides": ["catalog"],
                  "homepage": "https://example.com/hello",
                  "license": "MIT",
                  "official": {official},
                  "versions": [
                    {{
                      "version": "1.0.0",
                      "api": "hearthsound:plugin@0.2.0",
                      "url": "https://example.com/com.example.hello-1.0.0.hsplugin",
                      "sha256": "{FIXTURE_SHA256}",
                      "size": 1452032,
                      "publishedAt": "2026-01-01T00:00:00Z",
                      "changelog": "First release.",
                      "permissions": {{
                        "http": {{"hosts": ["api.example.com"]}},
                        "kv": {{"maxBytes": 1048576}}
                      }}
                    }}
                  ]
                }}
              ]
            }}"#
        )
    }

    /// Serves one 200 response at `document_path`.
    async fn serve(server: &MockServer, document_path: &str, body: &str) {
        Mock::given(method("GET"))
            .and(path(document_path))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    /// Returns the official index URL the mock server serves.
    fn official_url(server: &MockServer) -> String {
        format!("{}{OFFICIAL_PATH}", server.uri())
    }

    /// Returns a client whose official index is the mock server's.
    fn official_client(server: &MockServer, signer: &Signer) -> IndexClient {
        IndexClient::new(http(), official_url(server), signer.verify_key())
    }

    #[tokio::test]
    async fn a_signed_official_index_is_read() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        let body = document(true);
        serve(&server, OFFICIAL_PATH, &body).await;
        serve(
            &server,
            &format!("{OFFICIAL_PATH}.minisig"),
            &signer.sign(&body),
        )
        .await;

        let index = official_client(&server, &signer)
            .fetch(&official_url(&server))
            .await
            .unwrap();

        assert_eq!(index.format_version, 1);
        assert_eq!(index.name, "Example index");
        assert_eq!(index.generated_at, "2026-01-01T00:00:00Z");

        let plugin = index.plugin("com.example.hello").unwrap();
        assert_eq!(plugin.name, "Hello");
        assert_eq!(plugin.description, "An example catalog plugin.");
        assert_eq!(plugin.provides, ["catalog"]);
        assert_eq!(plugin.homepage, "https://example.com/hello");
        assert_eq!(plugin.license, "MIT");
        assert!(plugin.official);

        let version = plugin.version("1.0.0").unwrap();
        assert_eq!(version.api, SUPPORTED_API);
        assert_eq!(
            version.url,
            "https://example.com/com.example.hello-1.0.0.hsplugin"
        );
        assert_eq!(version.sha256, FIXTURE_SHA256);
        assert_eq!(version.size, 1_452_032);
        assert_eq!(version.published_at, "2026-01-01T00:00:00Z");
        assert_eq!(version.changelog, "First release.");
        assert_eq!(
            version
                .permissions
                .http
                .as_ref()
                .map(|http| http.hosts.clone()),
            Some(vec!["api.example.com".to_owned()])
        );
        assert_eq!(
            version.permissions.kv.as_ref().and_then(|kv| kv.max_bytes),
            Some(1_048_576)
        );
        assert!(version.permissions.files.is_none());
        assert!(version.permissions.process.is_none());

        // Every field survives being written back out and read again.
        let text = serde_json::to_string(&index).unwrap();
        assert_eq!(serde_json::from_str::<Index>(&text).unwrap(), index);
    }

    #[tokio::test]
    async fn a_tampered_document_is_rejected() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        let body = document(true);
        let tampered = body.replace("1.0.0", "9.9.9");
        serve(&server, OFFICIAL_PATH, &tampered).await;
        serve(
            &server,
            &format!("{OFFICIAL_PATH}.minisig"),
            &signer.sign(&body),
        )
        .await;

        let error = official_client(&server, &signer)
            .fetch(&official_url(&server))
            .await
            .unwrap_err();

        assert!(
            matches!(error, IndexError::SignatureInvalid { .. }),
            "expected a signature error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_signature_from_another_key_is_rejected() {
        let server = MockServer::start().await;
        let publisher = Signer::new();
        let impostor = Signer::new();
        let body = document(true);
        serve(&server, OFFICIAL_PATH, &body).await;
        serve(
            &server,
            &format!("{OFFICIAL_PATH}.minisig"),
            &impostor.sign(&body),
        )
        .await;

        let error = official_client(&server, &publisher)
            .fetch(&official_url(&server))
            .await
            .unwrap_err();

        assert!(
            matches!(error, IndexError::SignatureInvalid { .. }),
            "expected a signature error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn the_official_index_must_be_signed() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        serve(&server, OFFICIAL_PATH, &document(true)).await;

        let error = official_client(&server, &signer)
            .fetch(&official_url(&server))
            .await
            .unwrap_err();

        assert!(
            matches!(error, IndexError::Unsigned { .. }),
            "expected an unsigned error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn the_official_url_always_uses_the_official_key() {
        let server = MockServer::start().await;
        let publisher = Signer::new();
        let impostor = Signer::new();
        let body = document(true);
        serve(&server, OFFICIAL_PATH, &body).await;
        serve(
            &server,
            &format!("{OFFICIAL_PATH}.minisig"),
            &impostor.sign(&body),
        )
        .await;

        // The caller offering the impostor's own key must not make the official
        // URL accept the impostor's document.
        let error = official_client(&server, &publisher)
            .fetch_with_key(&official_url(&server), Some(&impostor.public_key()))
            .await
            .unwrap_err();

        assert!(
            matches!(error, IndexError::SignatureInvalid { .. }),
            "expected a signature error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn an_unsigned_repository_is_accepted_unverified() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        serve(&server, REPOSITORY_PATH, &document(false)).await;
        // A repository without a key is read without its signature at all.
        Mock::given(method("GET"))
            .and(path(format!("{REPOSITORY_PATH}.minisig")))
            .respond_with(ResponseTemplate::new(200).set_body_string("ignored"))
            .expect(0)
            .mount(&server)
            .await;

        let url = format!("{}{REPOSITORY_PATH}", server.uri());
        let index = official_client(&server, &signer).fetch(&url).await.unwrap();

        let plugin = index.plugin("com.example.hello").unwrap();
        assert!(!plugin.official);
        assert!(plugin.newest_compatible().is_some());
    }

    #[tokio::test]
    async fn a_repository_with_a_key_verifies() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        let body = document(false);
        serve(&server, REPOSITORY_PATH, &body).await;
        serve(
            &server,
            &format!("{REPOSITORY_PATH}.minisig"),
            &signer.sign(&body),
        )
        .await;

        let url = format!("{}{REPOSITORY_PATH}", server.uri());
        let index = official_client(&server, &signer)
            .fetch_with_key(&url, Some(&signer.public_key()))
            .await
            .unwrap();

        assert_eq!(index.plugin("com.example.hello").unwrap().name, "Hello");
    }

    #[tokio::test]
    async fn a_repository_with_a_key_rejects_everything_else() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        let impostor = Signer::new();
        let body = document(false);
        serve(&server, REPOSITORY_PATH, &body).await;
        serve(
            &server,
            &format!("{REPOSITORY_PATH}.minisig"),
            &impostor.sign(&body),
        )
        .await;

        let url = format!("{}{REPOSITORY_PATH}", server.uri());
        let client = official_client(&server, &signer);

        // A signature from another key.
        let error = client
            .fetch_with_key(&url, Some(&signer.public_key()))
            .await
            .unwrap_err();
        assert!(
            matches!(error, IndexError::SignatureInvalid { .. }),
            "expected a signature error, got {error:?}"
        );

        // A configured key that is not a key at all.
        let error = client
            .fetch_with_key(&url, Some("not a minisign public key"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, IndexError::SignatureInvalid { .. }),
            "expected a signature error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_repository_that_must_be_signed_and_is_not_is_rejected() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        serve(&server, REPOSITORY_PATH, &document(false)).await;

        let url = format!("{}{REPOSITORY_PATH}", server.uri());
        let error = official_client(&server, &signer)
            .fetch_with_key(&url, Some(&signer.public_key()))
            .await
            .unwrap_err();

        assert!(
            matches!(error, IndexError::Unsigned { .. }),
            "expected an unsigned error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_index_is_reported() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        Mock::given(method("GET"))
            .and(path(OFFICIAL_PATH))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = official_client(&server, &signer);
        let error = client.fetch(&official_url(&server)).await.unwrap_err();
        assert!(
            matches!(error, IndexError::Unreachable { .. }),
            "expected an unreachable error, got {error:?}"
        );

        // A missing document and a closed port are unreachable too.
        let error = client
            .fetch(&format!("{}{REPOSITORY_PATH}", server.uri()))
            .await
            .unwrap_err();
        assert!(
            matches!(error, IndexError::Unreachable { .. }),
            "expected an unreachable error, got {error:?}"
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = client
            .fetch(&format!("http://127.0.0.1:{port}{REPOSITORY_PATH}"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, IndexError::Unreachable { .. }),
            "expected an unreachable error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_body_that_is_not_an_index_is_malformed() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        serve(&server, OFFICIAL_PATH, "not json at all").await;

        let error = official_client(&server, &signer)
            .fetch(&official_url(&server))
            .await
            .unwrap_err();
        assert!(
            matches!(error, IndexError::Malformed { .. }),
            "expected a malformed error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn an_index_larger_than_the_limit_is_malformed() {
        let server = MockServer::start().await;
        let signer = Signer::new();
        let oversized = format!("{}{}", document(true), "0".repeat(8 * 1024 * 1024));
        serve(&server, OFFICIAL_PATH, &oversized).await;

        let error = official_client(&server, &signer)
            .fetch(&official_url(&server))
            .await
            .unwrap_err();
        assert!(
            matches!(error, IndexError::Malformed { .. }),
            "expected a malformed error, got {error:?}"
        );
    }

    /// Builds a version entry of `plugin`.
    fn version(version: &str, api: &str) -> IndexVersion {
        IndexVersion {
            version: version.to_owned(),
            api: api.to_owned(),
            url: format!("https://example.com/com.example.hello-{version}.hsplugin"),
            sha256: FIXTURE_SHA256.to_owned(),
            size: 1,
            published_at: "2026-01-01T00:00:00Z".to_owned(),
            changelog: String::new(),
            permissions: Permissions::default(),
        }
    }

    /// Builds a plugin entry with the given versions.
    fn plugin(versions: Vec<IndexVersion>) -> IndexPlugin {
        IndexPlugin {
            id: "com.example.hello".to_owned(),
            name: "Hello".to_owned(),
            description: "An example catalog plugin.".to_owned(),
            provides: vec!["catalog".to_owned()],
            homepage: "https://example.com/hello".to_owned(),
            license: "MIT".to_owned(),
            official: true,
            versions,
        }
    }

    #[test]
    fn the_newest_compatible_version_is_selected() {
        let plugin = plugin(vec![
            version("0.1.0", SUPPORTED_API),
            version("0.3.0", "hearthsound:plugin@0.3.0"),
            version("0.2.0", SUPPORTED_API),
            version("not-a-version", SUPPORTED_API),
        ]);

        assert_eq!(plugin.newest_compatible().unwrap().version, "0.2.0");
        assert_eq!(plugin.unavailable(), None);
    }

    #[test]
    fn a_plugin_without_a_compatible_version_is_unavailable() {
        let plugin = plugin(vec![
            version("0.1.0", "hearthsound:plugin@0.1.0"),
            version("0.2.0", "hearthsound:plugin@0.3.0"),
        ]);

        assert!(plugin.newest_compatible().is_none());
        let unavailable = plugin.unavailable().unwrap();
        assert_eq!(
            unavailable,
            Unavailable::NoCompatibleVersion {
                supported: SUPPORTED_API,
                offered: vec![
                    "hearthsound:plugin@0.1.0".to_owned(),
                    "hearthsound:plugin@0.3.0".to_owned(),
                ],
            }
        );
        let message = unavailable.to_string();
        assert!(message.contains(SUPPORTED_API), "message was {message}");
        assert!(
            message.contains("hearthsound:plugin@0.3.0"),
            "message was {message}"
        );
    }

    #[test]
    fn a_declared_digest_decodes_to_its_bytes() {
        let entry = version("1.0.0", SUPPORTED_API);
        let digest = entry.sha256_digest().unwrap();
        assert_eq!(digest[0], 0x9c);
        assert_eq!(digest[1], 0x1f);
        assert_eq!(digest[31], 0x1a);

        let mut short = entry.clone();
        short.sha256 = FIXTURE_SHA256[..62].to_owned();
        assert_eq!(short.sha256_digest(), None);

        let mut uppercase = entry;
        uppercase.sha256 = FIXTURE_SHA256.to_uppercase();
        assert_eq!(uppercase.sha256_digest(), None);
    }
}
