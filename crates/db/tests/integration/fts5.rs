//! Confirms that the SQLite bundled with sqlx is built with FTS5, and that a
//! contentless FTS5 table indexes and deletes rows as the search index needs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_db::Database;

/// The contentless search index, copied from the data model.
const CREATE_SEARCH_FTS: &str = "CREATE VIRTUAL TABLE search_fts USING fts5(
  entity_kind UNINDEXED,
  entity_id   UNINDEXED,
  title,
  subtitle,
  content = '',
  tokenize = 'unicode61 remove_diacritics 2'
)";

async fn open_temp(dir: &Path) -> Database {
    Database::open(&dir.join("hearthsound.db")).await.unwrap()
}

#[tokio::test]
async fn bundled_sqlite_has_fts5() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp(dir.path()).await;

    let options: Vec<String> = sqlx::query_scalar("PRAGMA compile_options")
        .fetch_all(db.writer())
        .await
        .unwrap();
    assert!(
        options.iter().any(|option| option == "ENABLE_FTS5"),
        "bundled SQLite lacks ENABLE_FTS5; compile options: {options:?}"
    );

    db.close().await;
}

#[tokio::test]
async fn contentless_index_matches_without_diacritics_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp(dir.path()).await;

    sqlx::query(CREATE_SEARCH_FTS)
        .execute(db.writer())
        .await
        .unwrap();

    let title = "Caf\u{e9} Tacvba";
    sqlx::query(
        "INSERT INTO search_fts (rowid, entity_kind, entity_id, title, subtitle)
         VALUES (1, 'track', 'track-1', ?, ?)",
    )
    .bind(title)
    .bind(title)
    .execute(db.writer())
    .await
    .unwrap();

    let matches: Vec<i64> =
        sqlx::query_scalar("SELECT rowid FROM search_fts WHERE search_fts MATCH 'cafe'")
            .fetch_all(db.writer())
            .await
            .unwrap();
    assert_eq!(matches, vec![1]);

    sqlx::query(
        "INSERT INTO search_fts (search_fts, rowid, entity_kind, entity_id, title, subtitle)
         VALUES ('delete', 1, 'track', 'track-1', ?, ?)",
    )
    .bind(title)
    .bind(title)
    .execute(db.writer())
    .await
    .unwrap();

    let matches: Vec<i64> =
        sqlx::query_scalar("SELECT rowid FROM search_fts WHERE search_fts MATCH 'cafe'")
            .fetch_all(db.writer())
            .await
            .unwrap();
    assert!(matches.is_empty(), "the deleted row must no longer match");

    db.close().await;
}
