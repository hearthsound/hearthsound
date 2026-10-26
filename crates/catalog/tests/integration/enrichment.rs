//! Integration tests for the cache-only page reads and the enrichment run.
//!
//! A page read answers from the library and the cached catalog answers, and a
//! `catalog.enrich` run fetches what is missing. The assertions are made on the
//! page and on the cache table, so a provider call that should not happen is
//! visible as a cache row that should not exist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::{Env, count_cache_rows};
use hearthsound_catalog::{PageAlbum, PageDepth};
use hearthsound_core::{ClientId, Layer};
use hearthsound_library::query::Scope;
use hearthsound_test_support::DISCOGRAPHY_CATALOG;

/// Builds an environment with the discography fixture as the catalog layer.
///
/// The fixture's artist released three albums, one of them a remaster.
async fn discography_env() -> Env {
    let env = Env::new().await;
    env.instance(
        "disco",
        DISCOGRAPHY_CATALOG,
        "com.example.discography-catalog",
        serde_json::json!({}),
    )
    .await;
    env.assign(Layer::Catalog, &["disco"]).await;
    env
}

/// The scopes a page is read in for this test: the server-wide view.
fn scopes() -> [Scope; 1] {
    [Scope::server_for_principal(true)]
}

#[tokio::test]
async fn a_cached_page_answers_from_the_library_without_a_provider_call() {
    let env = discography_env().await;
    let (_, _, artist) = env
        .stored_album_with_track("First Light", "Discography Artist", 2001)
        .await;
    let service = env.service();

    let page = service
        .artist_page(&ClientId::Entity(artist), &scopes(), PageDepth::Cached)
        .await
        .unwrap()
        .expect("the stored artist answers");

    assert_eq!(page.albums.len(), 1, "only the library album is listed");
    assert!(matches!(page.albums[0], PageAlbum::Stored(_)));
    assert_eq!(
        count_cache_rows(&env.database).await,
        0,
        "a cached page must not call a provider"
    );
}

#[tokio::test]
async fn enriching_a_stored_artist_fills_the_page_for_the_next_visit() {
    let env = discography_env().await;
    let (_, _, artist) = env
        .stored_album_with_track("First Light", "Discography Artist", 2001)
        .await;
    let service = env.service();
    let id = ClientId::Entity(artist);

    let report = service.enrich(&id).await.unwrap();
    assert!(report.enriched, "the catalog answered the artist");
    assert!(report.artist.is_some(), "a discovery warm-up has a seed");
    assert!(
        count_cache_rows(&env.database).await > 0,
        "the run cached what it fetched"
    );

    let page = service
        .artist_page(&id, &scopes(), PageDepth::Cached)
        .await
        .unwrap()
        .expect("the page answers");
    assert!(
        page.albums.len() >= 3,
        "the catalog discography is listed from the cache: {}",
        page.albums.len()
    );
    let catalogs = page
        .albums
        .iter()
        .filter(|album| matches!(album, PageAlbum::Catalog(_)))
        .count();
    assert!(
        catalogs >= 2,
        "the albums the library lacks are catalog ones"
    );
}

#[tokio::test]
async fn enriching_a_catalog_album_fills_its_tracklist_and_artist_listing() {
    let env = discography_env().await;
    let service = env.service();
    let album = crate::common::catalog_id(
        "disco",
        hearthsound_core::EntityKind::Album,
        "disco-album-1",
    );

    let report = service
        .enrich(&ClientId::Catalog(album.clone()))
        .await
        .unwrap();
    assert!(report.enriched);
    assert!(service.cached_album(&album).await.unwrap().is_some());
}

#[tokio::test]
async fn one_enrichment_per_entity_and_listener_is_claimed_at_a_time() {
    let env = discography_env().await;
    let (_, _, artist) = env
        .stored_album_with_track("First Light", "Discography Artist", 2001)
        .await;
    let service = env.service();
    let id = ClientId::Entity(artist);
    let user = uuid::Uuid::from_u128(7);

    assert!(service.begin_enrichment(&id, user).await);
    assert!(
        !service.begin_enrichment(&id, user).await,
        "a burst of reads queues one job"
    );
    assert!(
        service
            .begin_enrichment(&id, uuid::Uuid::from_u128(8))
            .await,
        "another listener is a different request"
    );
    service.finish_enrichment(&id, user).await;
    assert!(
        service.begin_enrichment(&id, user).await,
        "a completed run can be asked for again"
    );
}

/// Two albums whose release years are two or more apart are different releases;
/// one year apart, or an unknown year, is the same album.
#[tokio::test]
async fn a_stored_and_a_catalog_album_are_paired_by_their_years() {
    use hearthsound_core::{CatalogId, EntityKind, Slug};

    let env = discography_env().await;
    let album = CatalogId::new(
        Slug::new("disco").unwrap(),
        EntityKind::Album,
        "disco-album-2",
    )
    .unwrap();

    // The catalog's album is "Second Wind (Remastered 2009)", released in 2009.
    for (year, pairs) in [
        (Some(2008), true),
        (Some(2009), true),
        (Some(1998), false),
        (Some(2011), false),
        (None, true),
    ] {
        // A stored album with a playable track is in the server scope, which is
        // what lets its page show the stored row rather than the catalog one.
        let (stored, track, _) = env
            .stored_album_with_track("Second Wind", "Discography Artist", year.unwrap_or(2001))
            .await;
        if year.is_none() {
            sqlx::query("UPDATE albums SET year = NULL WHERE id = ?")
                .bind(stored.uuid().as_bytes().as_slice())
                .execute(env.database.writer())
                .await
                .unwrap();
        }

        let page = env
            .service()
            .album_page(
                &ClientId::Catalog(album.clone()),
                &scopes(),
                hearthsound_catalog::PageDepth::Full,
            )
            .await
            .unwrap()
            .expect("the catalog album answers");
        let paired = matches!(page.head, hearthsound_catalog::AlbumHead::Stored(_));
        assert_eq!(
            paired, pairs,
            "a stored album released in {year:?} against the catalog's 2009 release"
        );

        // The page recorded the pairing, so the alias goes with the row: a
        // dangling alias would answer the next iteration with a deleted album.
        sqlx::query("DELETE FROM external_ids WHERE namespace = 'p:disco' AND value = ?")
            .bind("disco-album-2")
            .execute(env.database.writer())
            .await
            .unwrap();
        sqlx::query("DELETE FROM tracks WHERE id = ?")
            .bind(track.uuid().as_bytes().as_slice())
            .execute(env.database.writer())
            .await
            .unwrap();
        sqlx::query("DELETE FROM albums WHERE id = ?")
            .bind(stored.uuid().as_bytes().as_slice())
            .execute(env.database.writer())
            .await
            .unwrap();
    }
}

/// A catalog album one stored album claimed is never paired with a second stored
/// album of the same title, and a page read writes nothing new.
#[tokio::test]
async fn a_claimed_catalog_album_is_not_paired_again() {
    use hearthsound_core::{CatalogId, ClientId, EntityKind, Slug};

    let env = discography_env().await;
    let service = env.service();
    let (first, _, artist) = env
        .stored_album_with_track("Third Single", "Discography Artist", 2012)
        .await;
    // The trailing space keeps the file path distinct while the folded title
    // still names the same release.
    let (second, _, _) = env
        .stored_album_with_track("Third Single ", "Discography Artist", 2013)
        .await;
    let catalog = CatalogId::new(
        Slug::new("disco").unwrap(),
        EntityKind::Album,
        "disco-album-3",
    )
    .unwrap();

    // The first stored album claims the catalog album.
    let page = service
        .artist_page(
            &ClientId::Entity(artist),
            &scopes(),
            hearthsound_catalog::PageDepth::Full,
        )
        .await
        .unwrap()
        .expect("the artist answers");
    let claimed = service
        .entity_by_alias("p:disco", EntityKind::Album, "disco-album-3")
        .await
        .unwrap();
    assert!(claimed == Some(first) || claimed == Some(second));
    let other = if claimed == Some(first) {
        second
    } else {
        first
    };

    // A second read must not re-point the claim the store holds.
    let rows_before = alias_rows(&env, "disco-album-3").await;
    let again = service
        .artist_page(
            &ClientId::Entity(artist),
            &scopes(),
            hearthsound_catalog::PageDepth::Full,
        )
        .await
        .unwrap()
        .expect("the artist answers");
    assert_eq!(page.albums.len(), again.albums.len());
    assert_eq!(
        service
            .entity_by_alias("p:disco", EntityKind::Album, "disco-album-3")
            .await
            .unwrap(),
        claimed,
        "the existing claim stays"
    );
    assert_eq!(
        alias_rows(&env, "disco-album-3").await,
        rows_before,
        "a page read writes no second alias"
    );
    let _ = other;
    let _ = catalog;
}

/// Counts the alias rows of one provider id.
///
/// # Parameters
///
/// - `env`: the environment to read.
/// - `value`: the provider id.
async fn alias_rows(env: &Env, value: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM external_ids WHERE value = ?")
        .bind(value)
        .fetch_one(env.database.readers())
        .await
        .unwrap()
}
