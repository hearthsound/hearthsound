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

/// A discography a provider could not list is marked incomplete; one that
/// answered is complete and not cut.
#[tokio::test]
async fn a_discography_that_could_not_be_listed_is_marked_incomplete() {
    use hearthsound_core::{CatalogId, EntityKind, Slug};

    let env = discography_env().await;
    let service = env.service();
    let artist = CatalogId::new(
        Slug::new("disco").unwrap(),
        EntityKind::Artist,
        "disco-artist-1",
    )
    .unwrap();
    let page = service
        .artist_page(&ClientId::Catalog(artist), &scopes(), PageDepth::Full)
        .await
        .unwrap()
        .expect("the artist answers");
    assert!(
        page.discography_complete,
        "the fixture lists its three albums"
    );
    assert!(!page.discography_truncated);

    // A stored artist whose instance answers no discography keeps the page
    // answerable, and the page says the discography is incomplete.
    let (server, url) = crate::common::catalog_endpoint(
        200,
        r#"{"artists":[{"id":"ar-1","name":"Discography Artist"}]}"#,
        None,
    )
    .await;
    let _ = server;
    let env = crate::common::Env::new().await;
    env.instance(
        "httpcat",
        hearthsound_test_support::HTTP_CATALOG,
        crate::common::HTTP_CATALOG_ID,
        crate::common::http_config(&url),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    let (_, _, artist) = env
        .stored_album_with_track("An Album", "Discography Artist", 2001)
        .await;
    let page = env
        .service()
        .artist_page(&ClientId::Entity(artist), &scopes(), PageDepth::Full)
        .await
        .unwrap()
        .expect("the stored artist answers");
    assert!(
        !page.discography_complete,
        "a discography the instance could not answer is incomplete"
    );
    assert!(!page.albums.is_empty(), "the library album is still listed");
}

/// A stored artist page lists the discography another page's enrichment already
/// cached, before the artist's own record was ever read.
///
/// The alias names the catalog artist, and the listing is enough for the page:
/// requiring the artist's record as well is what made a stored artist show only
/// its library albums until a second visit.
#[tokio::test]
async fn a_stored_artist_lists_a_cached_discography_before_its_record_is_read() {
    let env = discography_env().await;
    let (_, _, artist) = env
        .stored_album_with_track("First Light", "Discography Artist", 2001)
        .await;
    let service = env.service();
    let catalog = crate::common::catalog_id(
        "disco",
        hearthsound_core::EntityKind::Artist,
        "disco-artist-1",
    );

    // Another page - the artist's album page, say - enriched the discography
    // without reading the artist's own record.
    service
        .artist_albums(&catalog, hearthsound_catalog::MAX_PAGE_ALBUMS, 0)
        .await
        .unwrap();
    assert!(
        service.cached_artist(&catalog).await.unwrap().is_none(),
        "the artist record itself was never cached"
    );
    crate::common::write_alias(
        &env.database,
        "p:disco",
        hearthsound_core::EntityKind::Artist,
        "disco-artist-1",
        artist,
    )
    .await;
    let page = service
        .artist_page(&ClientId::Entity(artist), &scopes(), PageDepth::Cached)
        .await
        .unwrap()
        .expect("the stored artist answers");

    assert!(
        page.albums
            .iter()
            .any(|album| matches!(album, PageAlbum::Catalog(_))),
        "the cached discography is listed on the first read: {:?}",
        page.albums.len()
    );
    assert_eq!(
        artist_record_rows(&env.database).await,
        0,
        "a cache-only page never reads the artist's own record from the provider"
    );

    // The listing's references are left behind for the covers: a card's cover is
    // requested by album id, and without them each card is a provider lookup.
    let listed = page
        .albums
        .iter()
        .find_map(|album| match album {
            PageAlbum::Catalog(reference) => Some(reference.id.clone()),
            PageAlbum::Stored(_) => None,
        })
        .expect("the page lists a catalog album");
    assert!(
        service.cached_album_ref(&listed).await.unwrap().is_some(),
        "the listing promoted the album's reference into the cache"
    );
}

/// Counts the cached album records of the discography fixture's artist.
///
/// # Parameters
///
/// - `database`: the database to read.
///
/// # Returns
///
/// The number of `artist|id=` rows, which only a provider read writes.
async fn artist_record_rows(database: &hearthsound_db::Database) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM catalog_cache WHERE request_key LIKE 'artist|id=%'")
        .fetch_one(database.readers())
        .await
        .unwrap()
}

/// A library artist no catalog instance knows still gets its biography.
///
/// The metadata layer answers by name, so a biography does not depend on a
/// catalog pairing: without the run the page of a stored act never had one.
#[tokio::test]
async fn a_library_artist_gets_its_biography_without_a_catalog_counterpart() {
    use hearthsound_core::providers::ArtistInfoRequest;

    let env = Env::new().await;
    env.instance(
        "metadata",
        hearthsound_test_support::METADATA_FIXTURE,
        crate::common::METADATA_ID,
        serde_json::json!({
            "artists": [{
                "name": "Self Defense Family",
                "biography": "A band from upstate New York.",
            }],
        }),
    )
    .await;
    env.assign(Layer::Metadata, &["metadata"]).await;
    let (_, _, artist) = env
        .stored_album_with_track(
            "Have You Considered Punk Music",
            "Self Defense Family",
            2018,
        )
        .await;
    let service = env.service();

    let report = service.enrich(&ClientId::Entity(artist)).await.unwrap();
    assert!(
        !report.enriched,
        "there is no catalog discography to add to a library-only artist"
    );

    // The page reads with the stored artist's own identifiers, which is what the
    // biography has to be found under.
    let info = service
        .cached_artist_info(&ArtistInfoRequest {
            name: "Self Defense Family".to_owned(),
            ids: vec![hearthsound_core::providers::ExternalId {
                namespace: "mb-artist".to_owned(),
                value: "44444444-4444-4444-8444-444444444444".to_owned(),
            }],
        })
        .await;
    assert_eq!(
        info.and_then(|info| info.biography).as_deref(),
        Some("A band from upstate New York.")
    );
}

/// A chart leaves each track behind for its cover.
///
/// A song row's cover is requested by track id, and the track's record names the
/// album whose image it is. Without the promotion every chart row's cover is a
/// provider lookup, which on a rate-limited instance is what left rows blank.
#[tokio::test]
async fn a_cached_chart_leaves_its_tracks_behind_for_their_covers() {
    let env = discography_env().await;
    let service = env.service();
    let artist = crate::common::catalog_id(
        "disco",
        hearthsound_core::EntityKind::Artist,
        "disco-artist-1",
    );

    let chart = service.cached_artist_top_tracks(&artist, 5).await.unwrap();
    assert!(chart.is_none(), "nothing is cached before the first read");

    service.artist_top_tracks(&artist, 5).await.unwrap();
    let chart = service
        .cached_artist_top_tracks(&artist, 5)
        .await
        .unwrap()
        .expect("the chart was cached");
    assert!(!chart.is_empty());
    let track = crate::common::catalog_id(
        "disco",
        hearthsound_core::EntityKind::Track,
        chart[0].id.provider_id(),
    );
    assert!(
        service.cached_track(&track).await.unwrap().is_some(),
        "the chart leaves each track behind for its cover"
    );
}
