//! Cover art lookup for local entities.
//!
//! A scanned or imported file may carry its cover as an embedded picture or as
//! an image beside it. [`cover_for_file`] answers that for one file: it reads the
//! embedded picture when the file has one, and otherwise looks for a sidecar
//! image in the file's directory. [`track_cover`] resolves the track's preferred
//! file first, so the artwork a client gets is the artwork of the file it would
//! be served.
//!
//! Nothing is cached here; the caller that serves `getCoverArt` decides whether
//! to keep the bytes.

use std::path::{Path, PathBuf};

use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::{EntityId, StorageClass};

use crate::error::LibraryError;
use crate::model::StorageRoots;
use crate::repository::LibraryRepository;

/// The sidecar image file names that are looked for, in order, before the
/// configured `library.cover_filename`.
pub const SIDECAR_CANDIDATES: [&str; 5] = [
    "cover.jpg",
    "cover.png",
    "folder.jpg",
    "folder.png",
    "front.jpg",
];

/// A cover image for a local entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverArt {
    /// An encoded image read from the file's embedded picture.
    Embedded {
        /// The image MIME type, for example `image/jpeg`.
        mime_type: String,
        /// The encoded image bytes.
        data: Vec<u8>,
    },
    /// An encoded image in a file beside the track.
    Sidecar {
        /// The absolute path of the image.
        path: PathBuf,
        /// The image MIME type derived from the file's extension.
        mime_type: String,
    },
}

/// Looks for the cover art of one media file.
///
/// # Parameters
///
/// - `path`: the absolute path of the media file.
/// - `has_embedded_art`: whether the probe found an embedded picture; `None`
///   means unknown, and the picture is looked for anyway.
/// - `cover_filename`: the configured `library.cover_filename`, tried after the
///   standard sidecar names.
///
/// # Returns
///
/// The embedded picture when the file has one, otherwise the first sidecar image
/// that exists, otherwise `None`.
///
/// # Errors
///
/// Returns [`LibraryError::Media`] when reading the embedded picture fails, and
/// [`LibraryError::Scan`] when a sidecar file cannot be inspected.
pub async fn cover_for_file(
    path: &Path,
    has_embedded_art: Option<bool>,
    cover_filename: &str,
) -> Result<Option<CoverArt>, LibraryError> {
    if has_embedded_art != Some(false)
        && let Some(artwork) = hearthsound_media::extract_picture(path).await?
    {
        return Ok(Some(CoverArt::Embedded {
            mime_type: artwork.mime_type().to_owned(),
            data: artwork.into_data(),
        }));
    }

    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    for name in SIDECAR_CANDIDATES
        .iter()
        .copied()
        .chain(std::iter::once(cover_filename))
    {
        let candidate = directory.join(name);
        match std::fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => {
                return Ok(Some(CoverArt::Sidecar {
                    mime_type: mime_for(name),
                    path: candidate,
                }));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LibraryError::Scan {
                    path: candidate,
                    source,
                });
            }
        }
    }
    Ok(None)
}

/// Looks for the cover art of a track's preferred file.
///
/// # Parameters
///
/// - `library`: the repository to read the track and its folder through.
/// - `settings`: the settings snapshot the cover filename is read from.
/// - `roots`: the managed storage roots.
/// - `track_id`: the track to look up.
///
/// # Returns
///
/// The cover art of the preferred file, or `None` when the track has no file or
/// its file has no artwork.
///
/// # Errors
///
/// Returns [`LibraryError`] when a read or the embedded-picture extraction fails.
pub async fn track_cover(
    library: &LibraryRepository,
    settings: &SettingsStore,
    roots: &StorageRoots,
    track_id: EntityId,
) -> Result<Option<CoverArt>, LibraryError> {
    let Some(file) = library.preferred_media_file(track_id).await? else {
        return Ok(None);
    };
    let folder_path = match (file.storage, file.folder_id) {
        (StorageClass::Folder, Some(folder_id)) => library
            .get_library_folder(folder_id)
            .await?
            .map(|folder| folder.path),
        _ => None,
    };
    let path = file.absolute_path(folder_path.as_deref(), roots);
    let cover_filename = settings.get(keys::LIBRARY_COVER_FILENAME)?;
    cover_for_file(&path, file.has_embedded_art, &cover_filename).await
}

/// Returns the image MIME type for a sidecar file name.
///
/// # Parameters
///
/// - `name`: the image file name.
///
/// # Returns
///
/// `image/jpeg` for a `.jpg` or `.jpeg` name, `image/png` for a `.png` name, and
/// `application/octet-stream` for anything else.
pub fn mime_for(name: &str) -> String {
    let extension = Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    match extension.as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg".to_owned(),
        Some("png") => "image/png".to_owned(),
        _ => "application/octet-stream".to_owned(),
    }
}
