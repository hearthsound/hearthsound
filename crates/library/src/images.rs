//! The stored images of entities: bytes on disk, rows in `image_blobs` and
//! `entity_images`.
//!
//! Every image the server shows is its own copy. A blob is written once, keyed
//! by the SHA-256 of its bytes, and every entity image row that points at those
//! bytes shares it; a scan stores the cover it extracted from a file, and the
//! indexer stores what a provider named. A provider row keeps the URL it came
//! from, so a blob that was pruned to reclaim space is fetched again from the
//! same place rather than by asking the provider anew.
//!
//! Blobs live under `<artwork>/blobs/<first byte>/<sha256>.<ext>`. Thumbnails
//! are derived on demand by [`hearthsound_library::thumbnails::ThumbnailCache`]
//! from whatever bytes are served, so a provider image is scaled exactly like a
//! local cover.

use std::path::{Path, PathBuf};

use hearthsound_core::providers::Image;
use hearthsound_core::{EntityKind, new_uuid_v7};
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::external_ids::kind_literal;
use crate::model::decode_uuid;
use crate::now_millis;

/// One `entity_images` row joined with its blob, as read back.
type ImageRow = (
    Vec<u8>,
    Option<Vec<u8>>,
    Option<String>,
    Option<Vec<u8>>,
    Option<String>,
    Option<String>,
);

/// One stored entity image as the serving path needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredImage {
    /// The `entity_images` row's primary key.
    pub id: Uuid,
    /// The provider instance that named the image, or `None` for a file image.
    pub instance_id: Option<Uuid>,
    /// The URL the image was found at, for a provider image.
    pub url: Option<String>,
    /// The blob's hash, when its bytes are stored.
    pub blob_hash: Option<[u8; 32]>,
    /// The blob's file name relative to the artwork directory, when stored.
    pub blob_path: Option<String>,
    /// The blob's content type, when stored.
    pub content_type: Option<String>,
}

/// The image bytes the server keeps for entities.
#[derive(Debug, Clone)]
pub struct ImageStore {
    root: PathBuf,
}

impl ImageStore {
    /// Builds a store rooted at the artwork directory.
    ///
    /// # Parameters
    ///
    /// - `root`: the directory images are written under, `<data>/artwork`.
    pub fn new(root: impl Into<PathBuf>) -> ImageStore {
        ImageStore { root: root.into() }
    }

    /// Returns the artwork directory the store writes to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stores bytes and returns their hash, writing the file only once.
    ///
    /// # Parameters
    ///
    /// - `bytes`: the encoded image.
    /// - `content_type`: the media type the bytes arrived with.
    ///
    /// # Returns
    ///
    /// The SHA-256 of the bytes, which is also the `image_blobs` key.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Io`] when the file cannot be written, and
    /// [`LibraryError::Database`] when the row cannot be recorded.
    pub async fn store_bytes(
        &self,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<[u8; 32], LibraryError> {
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let relative = blob_path(&hash, content_type);
        let target = self.root.join(&relative);
        if tokio::fs::metadata(&target).await.is_err() {
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|source| LibraryError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
            }
            let scratch = target.with_extension(format!("{}.tmp", Uuid::now_v7()));
            tokio::fs::write(&scratch, bytes)
                .await
                .map_err(|source| LibraryError::Io {
                    path: scratch.clone(),
                    source,
                })?;
            tokio::fs::rename(&scratch, &target)
                .await
                .map_err(|source| LibraryError::Io {
                    path: target.clone(),
                    source,
                })?;
        }
        Ok(hash)
    }

    /// Records a blob in the database, creating its row once.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `hash`: the blob's SHA-256.
    /// - `bytes`: the blob, used for its length only.
    /// - `content_type`: the blob's media type.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Database`] when the write fails.
    pub async fn record_blob(
        &self,
        conn: &mut SqliteConnection,
        hash: &[u8; 32],
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), LibraryError> {
        sqlx::query(
            "INSERT INTO image_blobs (hash, path, content_type, byte_len, created_at) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(hash.as_slice())
        .bind(blob_path(hash, content_type))
        .bind(content_type)
        .bind(bytes.len() as i64)
        .bind(now_millis())
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Stores bytes and records them in one step.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `bytes`: the encoded image.
    /// - `content_type`: the media type the bytes arrived with.
    ///
    /// # Returns
    ///
    /// The blob's SHA-256.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`ImageStore::store_bytes`] and
    /// [`ImageStore::record_blob`].
    pub async fn store(
        &self,
        conn: &mut SqliteConnection,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<[u8; 32], LibraryError> {
        let hash = self.store_bytes(bytes, content_type).await?;
        self.record_blob(conn, &hash, bytes, content_type).await?;
        Ok(hash)
    }

    /// Reads a stored blob's bytes.
    ///
    /// # Parameters
    ///
    /// - `relative`: the blob's path relative to the artwork directory.
    ///
    /// # Returns
    ///
    /// The bytes, or `None` when the file is gone.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Io`] when the file exists but cannot be read.
    pub async fn read_blob(&self, relative: &str) -> Result<Option<Vec<u8>>, LibraryError> {
        let path = self.root.join(relative);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(LibraryError::Io { path, source }),
        }
    }

    /// Removes blobs no entity image references, with their thumbnails.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    ///
    /// # Returns
    ///
    /// How many blobs were removed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Database`] when a statement fails and
    /// [`LibraryError::Io`] when a file cannot be removed for a reason other
    /// than being absent.
    pub async fn sweep_orphans(&self, conn: &mut SqliteConnection) -> Result<u64, LibraryError> {
        let rows: Vec<(Vec<u8>, String)> = sqlx::query_as(
            "SELECT hash, path FROM image_blobs b \
             WHERE NOT EXISTS (SELECT 1 FROM entity_images i WHERE i.blob_hash = b.hash)",
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut removed = 0;
        for (hash, relative) in rows {
            if hash.len() != 32 {
                continue;
            }
            let mut digest = [0u8; 32];
            digest.copy_from_slice(&hash);
            remove_if_present(&self.root.join(&relative)).await?;
            let thumb_prefix = thumb_prefix(&digest);
            let _ = hex(&digest);
            let thumbs = self.root.join("thumbs");
            if let Ok(mut entries) = tokio::fs::read_dir(&thumbs).await {
                while let Some(entry) =
                    entries
                        .next_entry()
                        .await
                        .map_err(|source| LibraryError::Io {
                            path: thumbs.clone(),
                            source,
                        })?
                {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with(&thumb_prefix) {
                        remove_if_present(&entry.path()).await?;
                    }
                }
            }
            sqlx::query("DELETE FROM image_blobs WHERE hash = ?")
                .bind(digest.as_slice())
                .execute(&mut *conn)
                .await?;
            removed += 1;
        }
        Ok(removed)
    }
}

/// Replaces the image a file supplies for one role.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
/// - `role`: `cover` or `portrait`.
/// - `blob_hash`: the stored blob.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails and
/// [`LibraryError::UnknownLiteral`] for a kind without images.
pub async fn set_file_image(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
    role: &str,
    blob_hash: &[u8; 32],
) -> Result<(), LibraryError> {
    let kind_literal = kind_literal(kind)?;
    sqlx::query(
        "DELETE FROM entity_images \
         WHERE entity_kind = ? AND entity_id = ? AND role = ? AND instance_id IS NULL",
    )
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .bind(role)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO entity_images (id, entity_kind, entity_id, role, instance_id, position, \
                                    url, width, height, blob_hash, fetched_at) \
         VALUES (?, ?, ?, ?, NULL, 0, NULL, NULL, NULL, ?, ?)",
    )
    .bind(new_uuid_v7().as_bytes().as_slice())
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .bind(role)
    .bind(blob_hash.as_slice())
    .bind(now_millis())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Replaces the images one provider named for a role.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
/// - `role`: `cover` or `portrait`.
/// - `instance`: the provider instance that named them.
/// - `images`: the renditions, largest first as the provider ordered them.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails and
/// [`LibraryError::UnknownLiteral`] for a kind without images.
pub async fn put_provider_images(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
    role: &str,
    instance: Uuid,
    images: &[Image],
) -> Result<(), LibraryError> {
    let kind_literal = kind_literal(kind)?;
    sqlx::query(
        "DELETE FROM entity_images \
         WHERE entity_kind = ? AND entity_id = ? AND role = ? AND instance_id = ?",
    )
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .bind(role)
    .bind(instance.as_bytes().as_slice())
    .execute(&mut *conn)
    .await?;
    let now = now_millis();
    for (position, image) in images.iter().enumerate() {
        sqlx::query(
            "INSERT INTO entity_images (id, entity_kind, entity_id, role, instance_id, position, \
                                        url, width, height, blob_hash, fetched_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)",
        )
        .bind(new_uuid_v7().as_bytes().as_slice())
        .bind(kind_literal)
        .bind(entity.as_bytes().as_slice())
        .bind(role)
        .bind(instance.as_bytes().as_slice())
        .bind(position as i64)
        .bind(&image.url)
        .bind(image.width.map(i64::from))
        .bind(image.height.map(i64::from))
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Records that one image row's bytes are now stored.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `image`: the `entity_images` row.
/// - `blob_hash`: the blob the row's bytes were stored as.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails.
pub async fn attach_blob(
    conn: &mut SqliteConnection,
    image: Uuid,
    blob_hash: &[u8; 32],
) -> Result<(), LibraryError> {
    sqlx::query("UPDATE entity_images SET blob_hash = ? WHERE id = ?")
        .bind(blob_hash.as_slice())
        .bind(image.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Lists one entity's images of a role, file images first.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
/// - `role`: `cover` or `portrait`.
///
/// # Returns
///
/// The rows, ordered with the file's own image first and then by position.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub async fn images_of(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
    role: &str,
) -> Result<Vec<StoredImage>, LibraryError> {
    let kind_literal = kind_literal(kind)?;
    let rows: Vec<ImageRow> = sqlx::query_as(
        "SELECT i.id, i.instance_id, i.url, i.blob_hash, b.path, b.content_type \
         FROM entity_images i LEFT JOIN image_blobs b ON b.hash = i.blob_hash \
         WHERE i.entity_kind = ? AND i.entity_id = ? AND i.role = ? \
         ORDER BY i.instance_id IS NOT NULL, i.position",
    )
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .bind(role)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|(id, instance, url, hash, path, content_type)| {
            let blob_hash = match hash {
                Some(bytes) if bytes.len() == 32 => {
                    let mut digest = [0u8; 32];
                    digest.copy_from_slice(&bytes);
                    Some(digest)
                }
                Some(_) => None,
                None => None,
            };
            Ok(StoredImage {
                id: decode_uuid(&id)?,
                instance_id: match instance.as_deref() {
                    Some(bytes) => Some(decode_uuid(bytes)?),
                    None => None,
                },
                url,
                blob_hash,
                blob_path: path,
                content_type,
            })
        })
        .collect()
}

/// Deletes one image row.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `image`: the `entity_images` row.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails.
pub async fn delete_image(conn: &mut SqliteConnection, image: Uuid) -> Result<(), LibraryError> {
    sqlx::query("DELETE FROM entity_images WHERE id = ?")
        .bind(image.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Returns the path a blob is stored at, relative to the artwork directory.
///
/// # Parameters
///
/// - `hash`: the blob's SHA-256.
/// - `content_type`: the blob's media type, which picks the extension.
pub fn blob_path(hash: &[u8; 32], content_type: &str) -> String {
    let hex = hex(hash);
    let (bucket, _) = hex.split_at(2);
    format!("blobs/{bucket}/{hex}.{}", extension(content_type))
}

/// Returns the file extension for a media type.
///
/// # Parameters
///
/// - `content_type`: the media type, with or without parameters.
fn extension(content_type: &str) -> &'static str {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "jpg",
    }
}

/// Returns the prefix a blob's thumbnails are named with.
///
/// The thumbnail cache names a file `<hex of the first 16 bytes of the source
/// hash>-<size>.jpg`, so the first 32 characters identify every size of one
/// blob.
///
/// # Parameters
///
/// - `hash`: the blob's SHA-256.
fn thumb_prefix(hash: &[u8; 32]) -> String {
    hex(&hash[..16])
}

/// Returns the lowercase hex form of bytes.
///
/// # Parameters
///
/// - `bytes`: the bytes to encode.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Removes a file, ignoring one that is already gone.
///
/// # Parameters
///
/// - `path`: the file to remove.
async fn remove_if_present(path: &Path) -> Result<(), LibraryError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LibraryError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}
