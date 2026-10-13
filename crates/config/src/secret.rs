//! The secret key file and the AES-256-GCM secret box.
//!
//! Every secret the server stores at rest - app passwords, provider and plugin
//! configuration - is sealed with one AES-256-GCM key. The key is 32 random
//! bytes read from `HEARTHSOUND_SECRET_KEY` (base64, which wins) or from the key
//! file at `secrets.key_file`. The file is created on first start with owner-only
//! permissions; a file that other users can read is only warned about, because
//! refusing to start would lock an operator out of their own data.
//!
//! The key material itself is never rendered: [`SecretKey`] and [`SecretBox`]
//! redact their `Debug` output, so a key cannot leak through a log line, a
//! `tracing` field, or an error message.

use std::collections::BTreeMap;
use std::fmt;
use std::io::ErrorKind;
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use data_encoding::{BASE64, BASE64_NOPAD};
use rand::RngExt;
use rand::rng;

/// The length of the AES-256-GCM key, in bytes.
pub const KEY_LEN: usize = 32;

/// The length of the AES-256-GCM nonce, in bytes.
pub const NONCE_LEN: usize = 12;

/// The environment variable holding the key as base64.
///
/// It wins over the key file so an operator can inject the key from a secret
/// manager without touching the data directory.
pub const SECRET_KEY_ENV: &str = "HEARTHSOUND_SECRET_KEY";

/// The AES-256-GCM key material.
///
/// # Examples
///
/// ```
/// use hearthsound_config::secret::SecretKey;
///
/// let key = SecretKey::generate();
/// assert!(!format!("{key:?}").contains("0x"));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey([u8; KEY_LEN]);

impl SecretKey {
    /// Generates a key from the operating system's random number generator.
    ///
    /// # Returns
    ///
    /// A fresh 32-byte key.
    pub fn generate() -> SecretKey {
        let mut bytes = [0u8; KEY_LEN];
        rng().fill(&mut bytes[..]);
        SecretKey(bytes)
    }

    /// Wraps 32 bytes of key material.
    ///
    /// # Parameters
    ///
    /// - `bytes`: the key material.
    ///
    /// # Returns
    ///
    /// The key.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> SecretKey {
        SecretKey(bytes)
    }

    /// Parses a base64-encoded key.
    ///
    /// Both the padded and the unpadded standard alphabets are accepted, because
    /// operators paste either into the environment variable.
    ///
    /// # Parameters
    ///
    /// - `text`: the base64 text, whose surrounding whitespace is ignored.
    ///
    /// # Returns
    ///
    /// The key.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::InvalidBase64`] when the text is not base64, and
    /// [`SecretError::InvalidKeyLen`] when it does not decode to 32 bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_config::secret::SecretKey;
    ///
    /// let key = SecretKey::generate();
    /// assert!(SecretKey::from_base64("not base64").is_err());
    /// ```
    pub fn from_base64(text: &str) -> Result<SecretKey, SecretError> {
        let text = text.trim();
        let bytes = BASE64
            .decode(text.as_bytes())
            .or_else(|_| BASE64_NOPAD.decode(text.as_bytes()))
            .map_err(|error| SecretError::InvalidBase64 {
                reason: error.to_string(),
            })?;
        let bytes: [u8; KEY_LEN] = bytes
            .try_into()
            .map_err(|bytes: Vec<u8>| SecretError::InvalidKeyLen { len: bytes.len() })?;
        Ok(SecretKey(bytes))
    }

    /// Loads the key from the environment or from the key file.
    ///
    /// `HEARTHSOUND_SECRET_KEY` wins when it is set and non-empty; in that case
    /// the key file is neither read nor created. Otherwise the file is read, and
    /// created with 32 fresh random bytes and owner-only permissions when it does
    /// not exist yet, together with its parent directory.
    ///
    /// # Parameters
    ///
    /// - `path`: the key file.
    /// - `env`: the environment to read, keyed by full variable name.
    ///
    /// # Returns
    ///
    /// The 32-byte key.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::InvalidBase64`] or [`SecretError::InvalidKeyLen`]
    /// for malformed key material, and [`SecretError::Io`] when the file cannot
    /// be read, created, or written.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::collections::BTreeMap;
    /// use std::path::Path;
    /// use hearthsound_config::secret::SecretKey;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let key = SecretKey::load(Path::new("/var/lib/hearthsound/secret.key"), &BTreeMap::new())?;
    /// # let _ = key;
    /// # Ok(())
    /// # }
    /// ```
    pub fn load(path: &Path, env: &BTreeMap<String, String>) -> Result<SecretKey, SecretError> {
        if let Some(text) = env
            .get(SECRET_KEY_ENV)
            .filter(|text| !text.trim().is_empty())
        {
            return SecretKey::from_base64(text);
        }
        read_or_create(path)
    }

    /// Loads the key with the real process environment.
    ///
    /// # Parameters
    ///
    /// - `path`: the key file.
    ///
    /// # Returns
    ///
    /// The 32-byte key.
    ///
    /// # Errors
    ///
    /// Same as [`SecretKey::load`].
    pub fn load_from_process_env(path: &Path) -> Result<SecretKey, SecretError> {
        let env = std::env::var(SECRET_KEY_ENV)
            .ok()
            .map(|value| (SECRET_KEY_ENV.to_owned(), value));
        let env: BTreeMap<String, String> = env.into_iter().collect();
        SecretKey::load(path, &env)
    }

    /// Builds the secret box this key seals with.
    ///
    /// # Returns
    ///
    /// A box that encrypts with this key.
    pub fn secret_box(&self) -> SecretBox {
        SecretBox::new(self.clone())
    }
}

impl fmt::Debug for SecretKey {
    /// Reports the type without the key material.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

/// An AES-256-GCM box that seals secrets with a fresh random nonce.
///
/// # Examples
///
/// ```
/// use hearthsound_config::secret::SecretKey;
///
/// let secret_box = SecretKey::generate().secret_box();
/// let (ciphertext, nonce) = secret_box.seal(b"hunter2").unwrap();
/// assert_eq!(secret_box.open(&ciphertext, &nonce).unwrap(), b"hunter2");
/// ```
#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
}

impl SecretBox {
    /// Builds a box over a key.
    ///
    /// # Parameters
    ///
    /// - `key`: the 32-byte AES-256-GCM key.
    ///
    /// # Returns
    ///
    /// The box.
    pub fn new(key: SecretKey) -> SecretBox {
        SecretBox {
            cipher: Aes256Gcm::new(&Key::<Aes256Gcm>::from(key.0)),
        }
    }

    /// Seals a plaintext.
    ///
    /// # Parameters
    ///
    /// - `plaintext`: the bytes to encrypt.
    ///
    /// # Returns
    ///
    /// The ciphertext, with the 16-byte authentication tag appended, and the
    /// nonce to pass back to [`SecretBox::open`]. The nonce comes from the
    /// operating system's random number generator and is unique per call.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::Seal`] when the cipher refuses the input, which
    /// cannot happen for a 96-bit nonce.
    pub fn seal(&self, plaintext: &[u8]) -> Result<(Vec<u8>, [u8; NONCE_LEN]), SecretError> {
        let mut nonce = [0u8; NONCE_LEN];
        rng().fill(&mut nonce[..]);
        let ciphertext = self
            .cipher
            .encrypt(&Nonce::from(nonce), plaintext)
            .map_err(|_| SecretError::Seal)?;
        Ok((ciphertext, nonce))
    }

    /// Opens a sealed plaintext.
    ///
    /// # Parameters
    ///
    /// - `ciphertext`: the ciphertext and authentication tag.
    /// - `nonce`: the nonce returned by [`SecretBox::seal`].
    ///
    /// # Returns
    ///
    /// The original plaintext.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::InvalidNonceLen`] when the nonce is not 12 bytes,
    /// and [`SecretError::Tampered`] when the ciphertext does not authenticate
    /// under this key and nonce.
    pub fn open(&self, ciphertext: &[u8], nonce: &[u8]) -> Result<Vec<u8>, SecretError> {
        let nonce: [u8; NONCE_LEN] = nonce
            .try_into()
            .map_err(|_| SecretError::InvalidNonceLen { len: nonce.len() })?;
        self.cipher
            .decrypt(&Nonce::from(nonce), ciphertext)
            .map_err(|_| SecretError::Tampered)
    }
}

impl fmt::Debug for SecretBox {
    /// Reports the type without the key material.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBox(<redacted>)")
    }
}

/// An error from loading key material, sealing, or opening a secret.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The key material was not base64.
    #[error("the secret key is not valid base64: {reason}")]
    InvalidBase64 {
        /// The decoder's complaint.
        reason: String,
    },
    /// The key material did not decode to 32 bytes.
    #[error("the secret key must be {KEY_LEN} bytes, found {len}")]
    InvalidKeyLen {
        /// The length that was supplied.
        len: usize,
    },
    /// The nonce was not 12 bytes.
    #[error("the nonce must be {NONCE_LEN} bytes, found {len}")]
    InvalidNonceLen {
        /// The length that was supplied.
        len: usize,
    },
    /// The key file could not be read, created, or written.
    #[error("secret key file `{path}` could not be used: {source}")]
    Io {
        /// The key file.
        path: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The cipher refused to seal the input.
    #[error("the secret could not be sealed")]
    Seal,
    /// The ciphertext did not authenticate.
    #[error("the sealed secret is not authentic")]
    Tampered,
}

/// Reads the key file, creating it when it does not exist.
///
/// # Parameters
///
/// - `path`: the key file.
///
/// # Returns
///
/// The key the file holds.
///
/// # Errors
///
/// Returns [`SecretError::Io`] when the file cannot be read or created and
/// [`SecretError::InvalidKeyLen`] when it does not hold 32 bytes.
fn read_or_create(path: &Path) -> Result<SecretKey, SecretError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            warn_if_permissive(path);
            key_from_file_bytes(bytes)
        }
        Err(source) if source.kind() == ErrorKind::NotFound => {
            let key = SecretKey::generate();
            match write_new_file(path, &key) {
                Ok(()) => Ok(key),
                // Another process won the race to create the file; use its key.
                Err(source) if source.kind() == ErrorKind::AlreadyExists => {
                    let bytes = std::fs::read(path).map_err(|source| SecretError::Io {
                        path: path.display().to_string(),
                        source,
                    })?;
                    key_from_file_bytes(bytes)
                }
                Err(source) => Err(SecretError::Io {
                    path: path.display().to_string(),
                    source,
                }),
            }
        }
        Err(source) => Err(SecretError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Interprets the bytes of a key file.
///
/// # Parameters
///
/// - `bytes`: the file contents.
///
/// # Returns
///
/// The key.
///
/// # Errors
///
/// Returns [`SecretError::InvalidKeyLen`] when the file does not hold 32 bytes.
fn key_from_file_bytes(bytes: Vec<u8>) -> Result<SecretKey, SecretError> {
    let bytes: [u8; KEY_LEN] = bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| SecretError::InvalidKeyLen { len: bytes.len() })?;
    Ok(SecretKey(bytes))
}

/// Creates the key file with owner-only permissions, creating its parent
/// directory first.
///
/// # Parameters
///
/// - `path`: the key file.
/// - `key`: the key to write.
///
/// # Errors
///
/// Returns the underlying IO error when the directory cannot be created, the
/// file already exists, or the key cannot be written.
fn write_new_file(path: &Path, key: &SecretKey) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = create_private_file(path)?;
    use std::io::Write as _;
    file.write_all(&key.0)?;
    file.sync_all()
}

/// Opens a new file that only its owner can read or write.
///
/// # Parameters
///
/// - `path`: the file to create.
///
/// # Errors
///
/// Returns the underlying IO error when the file exists or cannot be created.
#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    // The creation mode is filtered through the umask, so set it explicitly.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Opens a new file that only its owner can read or write.
///
/// The permission bits are a unix concept; elsewhere the file inherits the
/// platform's default protection.
///
/// # Parameters
///
/// - `path`: the file to create.
///
/// # Errors
///
/// Returns the underlying IO error when the file exists or cannot be created.
#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Warns when an existing key file is readable by other users.
///
/// # Parameters
///
/// - `path`: the key file to inspect. A file that cannot be stat'ed is left to
///   the caller's read to report.
#[cfg(unix)]
fn warn_if_permissive(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(metadata) = std::fs::metadata(path) {
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = %format!("{mode:o}"),
                "secret key file is readable by other users"
            );
        }
    }
}

/// Warns when an existing key file is readable by other users.
///
/// The permission bits are a unix concept, so this does nothing elsewhere.
///
/// # Parameters
///
/// - `path`: the key file to inspect.
#[cfg(not(unix))]
fn warn_if_permissive(path: &Path) {
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a key file with the given contents.
    fn key_file(contents: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    /// Builds an environment map from `(name, value)` pairs.
    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn sealing_round_trips() {
        let secret_box = SecretKey::generate().secret_box();
        let (ciphertext, nonce) = secret_box.seal(b"a provider api key").unwrap();
        assert_ne!(ciphertext, b"a provider api key");
        assert_eq!(ciphertext.len(), b"a provider api key".len() + 16);
        assert_eq!(
            secret_box.open(&ciphertext, &nonce).unwrap(),
            b"a provider api key"
        );

        let (empty, nonce) = secret_box.seal(b"").unwrap();
        assert_eq!(empty.len(), 16);
        assert_eq!(secret_box.open(&empty, &nonce).unwrap(), b"");
    }

    #[test]
    fn nonces_differ_between_seals() {
        let secret_box = SecretKey::generate().secret_box();
        let (first, first_nonce) = secret_box.seal(b"same").unwrap();
        let (second, second_nonce) = secret_box.seal(b"same").unwrap();
        assert_ne!(first_nonce, second_nonce);
        assert_ne!(first, second);
    }

    #[test]
    fn tampered_and_mismatched_inputs_are_rejected() {
        let key = SecretKey::generate();
        let secret_box = key.secret_box();
        let (mut ciphertext, nonce) = secret_box.seal(b"hunter2").unwrap();

        ciphertext[0] ^= 0x01;
        assert!(matches!(
            secret_box.open(&ciphertext, &nonce),
            Err(SecretError::Tampered)
        ));
        ciphertext[0] ^= 0x01;

        let mut wrong_nonce = nonce;
        wrong_nonce[0] ^= 0x01;
        assert!(matches!(
            secret_box.open(&ciphertext, &wrong_nonce),
            Err(SecretError::Tampered)
        ));

        let other = SecretKey::generate().secret_box();
        assert!(matches!(
            other.open(&ciphertext, &nonce),
            Err(SecretError::Tampered)
        ));

        assert!(matches!(
            secret_box.open(&ciphertext, &nonce[..NONCE_LEN - 1]),
            Err(SecretError::InvalidNonceLen { len: 11 })
        ));
    }

    #[test]
    fn key_files_are_created_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("secret.key");

        let key = SecretKey::load(&path, &BTreeMap::new()).unwrap();
        let stored = std::fs::read(&path).unwrap();
        assert_eq!(stored.len(), KEY_LEN);
        assert_eq!(stored, key.0);

        // Loading again reads the same key instead of generating a new one.
        assert_eq!(SecretKey::load(&path, &BTreeMap::new()).unwrap(), key);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode was {mode:o}");
        }
    }

    #[test]
    fn the_environment_key_wins_over_the_file() {
        let (_dir, path) = key_file(&[7u8; KEY_LEN]);
        let from_env = SecretKey::generate();

        let env = env_of(&[(SECRET_KEY_ENV, &BASE64.encode(&[7u8; KEY_LEN]))]);
        assert_eq!(
            SecretKey::load(&path, &env).unwrap(),
            SecretKey::from_bytes([7; KEY_LEN])
        );

        let env = env_of(&[(SECRET_KEY_ENV, &BASE64.encode(&[9u8; KEY_LEN]))]);
        let loaded = SecretKey::load(&path, &env).unwrap();
        assert_eq!(loaded, SecretKey::from_bytes([9; KEY_LEN]));
        assert_ne!(loaded, from_env);
    }

    #[test]
    fn an_environment_key_without_a_key_file_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        let env = env_of(&[(SECRET_KEY_ENV, &BASE64.encode(&[3u8; KEY_LEN]))]);

        let key = SecretKey::load(&path, &env).unwrap();
        assert_eq!(key, SecretKey::from_bytes([3; KEY_LEN]));
        assert!(!path.exists());
    }

    #[test]
    fn base64_without_padding_is_accepted() {
        let bytes = [5u8; KEY_LEN];
        let padded = BASE64.encode(&bytes);
        let unpadded = BASE64_NOPAD.encode(&bytes);
        assert_ne!(padded, unpadded);
        assert_eq!(
            SecretKey::from_base64(&padded).unwrap(),
            SecretKey::from_base64(&unpadded).unwrap()
        );
        assert_eq!(
            SecretKey::from_base64(&format!("  {unpadded}\n")).unwrap(),
            SecretKey::from_bytes(bytes)
        );
    }

    #[test]
    fn wrong_lengths_and_encodings_are_rejected() {
        let (_dir, path) = key_file(&[1u8; 16]);
        let error = SecretKey::load(&path, &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, SecretError::InvalidKeyLen { len: 16 }));

        let env = env_of(&[(SECRET_KEY_ENV, &BASE64.encode(&[1u8; 31]))]);
        let error = SecretKey::load(&path, &env).unwrap_err();
        assert!(matches!(error, SecretError::InvalidKeyLen { len: 31 }));

        let env = env_of(&[(SECRET_KEY_ENV, "not base64!!")]);
        let error = SecretKey::load(&path, &env).unwrap_err();
        assert!(matches!(error, SecretError::InvalidBase64 { .. }));

        // An empty environment variable is treated as unset.
        let env = env_of(&[(SECRET_KEY_ENV, "  ")]);
        let error = SecretKey::load(&path, &env).unwrap_err();
        assert!(matches!(error, SecretError::InvalidKeyLen { len: 16 }));
    }

    #[test]
    fn a_permissive_key_file_is_warned_about_but_still_used() {
        let (_dir, path) = key_file(&[4u8; KEY_LEN]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        assert_eq!(
            SecretKey::load(&path, &BTreeMap::new()).unwrap(),
            SecretKey::from_bytes([4; KEY_LEN])
        );
    }

    #[test]
    fn secrets_are_not_printed() {
        let key = SecretKey::generate();
        assert_eq!(format!("{key:?}"), "SecretKey(<redacted>)");
        let secret_box = key.secret_box();
        assert_eq!(format!("{secret_box:?}"), "SecretBox(<redacted>)");
    }

    #[test]
    fn an_unreadable_key_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        std::fs::create_dir(&path).unwrap();

        let error = SecretKey::load(&path, &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, SecretError::Io { .. }));
    }
}
