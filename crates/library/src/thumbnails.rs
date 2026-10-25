//! Scaled copies of local cover art, cached on disk.
//!
//! A client asks `getCoverArt` for a size, and a local cover is stored at
//! whatever size the file carries: an embedded picture is often 1000 pixels or
//! more, which a 40-pixel track row has no use for. This module turns the bytes
//! of a local cover into the smallest of a few fixed sizes and keeps the result
//! under `<data>/artwork/thumbs`, so the second request for a size is a file
//! read rather than another ffmpeg run.
//!
//! The file name is the first bytes of the source's SHA-256 plus the size, so a
//! cover that changes gets a new file and needs no invalidation: the hash of the
//! bytes decides the name, and the same bytes always name the same file. Old
//! files are not evicted here; they are small JPEGs under the artwork
//! directory, which the storage policy already manages.
//!
//! Scaling itself is ffmpeg's, through [`FfmpegRunner::scale_image`], so it
//! shares the server's transcode concurrency budget. A failure to scale is not
//! a failure to serve: the caller answers with the original bytes and logs the
//! reason.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use std::sync::Arc;
//!
//! use hearthsound_library::thumbnails::{THUMBNAIL_SIZES, ThumbnailCache, thumbnail_size};
//! use hearthsound_media::FfmpegRunner;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let cache = ThumbnailCache::new(
//!     Path::new("/var/lib/hearthsound/artwork/thumbs"),
//!     Arc::new(FfmpegRunner::new(None, 8)),
//! );
//! let size = thumbnail_size(Some(40)).unwrap_or(THUMBNAIL_SIZES[0]);
//! let scaled = cache.scaled(&[], size).await?;
//! println!("{} bytes", scaled.len());
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hearthsound_media::FfmpegRunner;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::LibraryError;

/// The sizes a local cover is scaled to, in pixels.
///
/// Three fixed sizes rather than one per request: a card, a track row and a now
/// playing panel each need one of them, and a fixed set means a cover is encoded
/// once per size for every client that asks for it.
pub const THUMBNAIL_SIZES: [u32; 3] = [96, 300, 600];

/// How many bytes of the source's hash name the cached file.
///
/// Sixteen bytes, the same length the endpoint's entity tag uses: collision
/// enough to be irrelevant and short enough for a file name.
const KEY_BYTES: usize = 16;

/// Returns the cached size that serves a requested size.
///
/// # Parameters
///
/// - `requested`: the size the client asked for, or `None` when it asked for
///   none.
///
/// # Returns
///
/// The smallest of [`THUMBNAIL_SIZES`] that is at least `requested`, or `None`
/// when nothing was requested or the request is larger than every size, in
/// which case the original is served. `None` also covers a client that asks for
/// a tiny size: [`THUMBNAIL_SIZES`] starts at 96, and serving an 8-pixel version
/// of a cover would cost an encode to save bytes nobody notices.
pub fn thumbnail_size(requested: Option<u32>) -> Option<u32> {
    let requested = requested?;
    THUMBNAIL_SIZES
        .iter()
        .copied()
        .find(|size| *size >= requested)
}

/// A disk cache of scaled local cover art.
///
/// The cache holds no state of its own beyond its directory: whether a scaled
/// copy exists is a question for the filesystem, so a restart or a second
/// process on the same data directory reuses what an earlier run wrote.
#[derive(Debug)]
pub struct ThumbnailCache {
    /// The directory scaled copies are written to.
    dir: PathBuf,
    /// The runner the scaling runs through.
    ffmpeg: Arc<FfmpegRunner>,
}

impl ThumbnailCache {
    /// Builds a cache over a directory.
    ///
    /// The directory is created on the first write, so a server that never
    /// serves a local cover leaves no empty directory behind. Leftovers of an
    /// earlier run - a scratch copy or a half-written encode - are removed once,
    /// before anything can use the directory; nothing else writes there, so the
    /// sweep cannot race a live request.
    ///
    /// # Parameters
    ///
    /// - `dir`: the directory scaled copies are written to, which is
    ///   `<data>/artwork/thumbs`.
    /// - `ffmpeg`: the runner the scaling runs through.
    ///
    /// # Returns
    ///
    /// The cache.
    pub fn new(dir: impl Into<PathBuf>, ffmpeg: Arc<FfmpegRunner>) -> ThumbnailCache {
        let dir = dir.into();
        sweep_scratch(&dir);
        ThumbnailCache { dir, ffmpeg }
    }

    /// Returns the bytes of a source image scaled to a size, from disk or fresh.
    ///
    /// # Parameters
    ///
    /// - `source`: the encoded image a local cover holds.
    /// - `size`: the target size, which a caller takes from
    ///   [`thumbnail_size`].
    ///
    /// # Returns
    ///
    /// The bytes of a JPEG whose longest side is at most `size`, or the source
    /// bytes themselves when the source is already that small, because the
    /// filter never enlarges. A cached file is read, not re-encoded. An encode
    /// that fails is retried on the next request and leaves no scratch file
    /// behind; the cache key is the hash of the source bytes, so a negative
    /// entry could never be invalidated by repairing the cover.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Io`] when the source cannot be written, the
    /// scaled file cannot be read or renamed, or the directory cannot be
    /// created, and [`LibraryError::Media`] when ffmpeg cannot scale the source.
    pub async fn scaled(&self, source: &[u8], size: u32) -> Result<Vec<u8>, LibraryError> {
        let key = source_key(source);
        let target = self.dir.join(format!("{key}-{size}.jpg"));
        if let Some(bytes) = read_if_present(&target).await {
            return Ok(bytes);
        }
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|source| LibraryError::Io {
                path: self.dir.clone(),
                source,
            })?;
        let source_path = self.dir.join(format!("{key}-{}.src", Uuid::now_v7()));
        let scratch_path = self
            .dir
            .join(format!("{key}-{size}-{}.tmp", Uuid::now_v7()));
        tokio::fs::write(&source_path, source)
            .await
            .map_err(|error| LibraryError::Io {
                path: source_path.clone(),
                source: error,
            })?;
        let scaled = self
            .ffmpeg
            .scale_image(&source_path, &scratch_path, size)
            .await;
        // The source copy is scratch either way: the caller already holds the
        // bytes, so a failed encode leaves nothing to inspect.
        let _ = tokio::fs::remove_file(&source_path).await;
        if let Err(error) = scaled {
            // A broken cover is asked for again on the next request - the key is
            // the hash of the source bytes, so a negative cache entry could never
            // be invalidated by fixing the file - but each attempt leaves no
            // file behind.
            let _ = tokio::fs::remove_file(&scratch_path).await;
            return Err(error.into());
        }
        // The rename is what publishes the file: two callers that miss the same
        // key both encode, and the second rename replaces the first, which costs
        // one encode and nothing else.
        tokio::fs::rename(&scratch_path, &target)
            .await
            .map_err(|error| LibraryError::Io {
                path: target.clone(),
                source: error,
            })?;
        tokio::fs::read(&target)
            .await
            .map_err(|error| LibraryError::Io {
                path: target,
                source: error,
            })
    }
}

/// Removes the scratch files an earlier run left in the cache directory.
///
/// A `.src` copy and a `.tmp` encode are written before the finished JPEG
/// appears, and a process that stopped between the two, or an encode that failed
/// before this module cleaned up after itself, leaves them behind. Nothing else
/// writes into the directory, so anything with those extensions is a leftover.
///
/// # Parameters
///
/// - `dir`: the cache directory; a directory that does not exist holds nothing.
fn sweep_scratch(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::debug!(dir = %dir.display(), %error, "could not read the thumbnail directory");
            return;
        }
    };
    let mut removed = 0_u64;
    for entry in entries.flatten() {
        let scratch = entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "tmp" || extension == "src");
        if scratch && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::debug!(
            dir = %dir.display(),
            removed,
            "removed the scratch files of an earlier run"
        );
    }
}

/// Returns the key that names a source image's cached copies.
///
/// # Parameters
///
/// - `source`: the encoded source image.
///
/// # Returns
///
/// The first [`KEY_BYTES`] of the source's SHA-256, lowercase hex.
fn source_key(source: &[u8]) -> String {
    let digest = Sha256::digest(source);
    data_encoding::HEXLOWER.encode(&digest[..KEY_BYTES])
}

/// Reads a cached file when it exists.
///
/// A file that has been removed between the metadata check and the read, and a
/// file that cannot be read at all, are both a miss: the caller encodes the
/// image rather than failing the request.
///
/// # Parameters
///
/// - `path`: the cached file's path.
///
/// # Returns
///
/// The file's bytes, or `None` when it does not exist or cannot be read.
async fn read_if_present(path: &Path) -> Option<Vec<u8>> {
    tokio::fs::read(path).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The requested size maps to the smallest cached size that covers it.
    #[test]
    fn a_requested_size_picks_the_smallest_cached_size_that_covers_it() {
        assert_eq!(thumbnail_size(None), None);
        assert_eq!(thumbnail_size(Some(40)), Some(96));
        assert_eq!(thumbnail_size(Some(96)), Some(96));
        assert_eq!(thumbnail_size(Some(97)), Some(300));
        assert_eq!(thumbnail_size(Some(600)), Some(600));
        assert_eq!(
            thumbnail_size(Some(601)),
            None,
            "a request larger than every cached size is served the original"
        );
    }

    /// A new cache sweeps the scratch files an earlier run left behind.
    #[test]
    fn a_new_cache_removes_the_scratch_files_of_an_earlier_run() {
        let dir = tempfile::tempdir().unwrap();
        let thumbs = dir.path().join("thumbs");
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(thumbs.join("a.tmp"), b"half an encode").unwrap();
        std::fs::write(thumbs.join("b.src"), b"a source copy").unwrap();
        std::fs::write(thumbs.join("c-64.jpg"), b"a finished cover").unwrap();

        let _cache = ThumbnailCache::new(&thumbs, Arc::new(FfmpegRunner::new(None, 1)));

        let mut left: Vec<String> = std::fs::read_dir(&thumbs)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["c-64.jpg"]);
    }

    /// A cache over a directory that does not exist yet sweeps nothing and does
    /// not create it.
    #[test]
    fn a_missing_directory_is_not_created_by_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let thumbs = dir.path().join("thumbs");
        let _cache = ThumbnailCache::new(&thumbs, Arc::new(FfmpegRunner::new(None, 1)));
        assert!(!thumbs.exists());
    }

    /// The same bytes always name the same file, and different bytes do not.
    #[test]
    fn the_key_follows_the_source_bytes() {
        assert_eq!(source_key(b"one cover"), source_key(b"one cover"));
        assert_ne!(source_key(b"one cover"), source_key(b"another cover"));
    }
}
