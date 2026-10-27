//! Merging: two rows that turn out to name one artist become the older row, and
//! the retired id keeps answering with the survivor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use hearthsound_catalog::Viewer;
use hearthsound_core::providers::{
    ArtistCredit, CatalogAlbum, CatalogArtist, MB_ARTIST, provider_namespace,
};
use hearthsound_core::{EntityId, EntityKind, IdPrefix, Slug};
use hearthsound_library::ArtistInput;

use crate::common::{self, Env};

/// The MusicBrainz id the library artist holds.
const LIBRARY_MBID: &str = "mb-artist-library";

/// The provider's own id for the same name.
const PROVIDER_ARTIST: &str = "provider-artist-1";

/// The MusicBrainz id the provider states beside its own, which no row holds:
/// the resolution plays safe and mints a row of its own.
const PROVIDER_MBID: &str = "mb-artist-provider";

#[tokio::test]
async fn an_album_credit_carrying_both_ids_merges_the_rows_into_the_older() {
    let env = Env::new().await;
    let slug = Slug::new("remote").unwrap();

    // The library artist comes first, so it is the older row.
    let library = env.library();
    let mut input = ArtistInput::new("A");
    input.ids = vec![common::external_id(MB_ARTIST, LIBRARY_MBID)];
    let library_id = library.upsert_artist(&input).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;

    let service = env.service();
    let resolver = service.resolver().clone();
    let instance = uuid::Uuid::now_v7();
    let mut connection = env.database.writer().acquire().await.unwrap();
    let provider_id = resolver
        .artist(
            &mut connection,
            instance,
            &slug,
            &CatalogArtist {
                id: PROVIDER_ARTIST.to_owned(),
                name: "A".to_owned(),
                images: Vec::new(),
                ids: vec![common::external_id(MB_ARTIST, PROVIDER_MBID)],
                album_count: None,
            },
        )
        .await
        .unwrap();
    drop(connection);
    let provider = EntityId::from_uuid(IdPrefix::Artist, provider_id);
    assert_ne!(
        provider, library_id,
        "a provider hit that states another canonical id does not adopt the library row"
    );
    assert!(
        library_id.uuid() < provider.uuid(),
        "the library row is older, so it is the merge survivor"
    );

    // An album page credits the artist with the provider's own id and the
    // library row's MusicBrainz id: the two ids prove the rows are one.
    let credit = ArtistCredit {
        name: "A".to_owned(),
        ids: vec![
            common::external_id(&provider_namespace(&slug), PROVIDER_ARTIST),
            common::external_id(MB_ARTIST, LIBRARY_MBID),
        ],
    };
    let album = CatalogAlbum {
        id: "provider-album".to_owned(),
        title: "A Album".to_owned(),
        artists: vec![credit],
        release_date: Some("2001-01-01".to_owned()),
        album_type: Some("album".to_owned()),
        upc: None,
        genres: Vec::new(),
        images: Vec::new(),
        tracks: Vec::new(),
        ids: Vec::new(),
        edition: None,
    };
    let mut connection = env.database.writer().acquire().await.unwrap();
    resolver
        .album(&mut connection, instance, &slug, &album)
        .await
        .unwrap();
    let canonical = hearthsound_library::entities::canonical(
        &mut connection,
        EntityKind::Artist,
        provider.uuid(),
    )
    .await
    .unwrap();
    drop(connection);

    assert_eq!(
        canonical,
        Some(library_id.uuid()),
        "the retired id resolves to the older row"
    );
    assert_eq!(env.count_rows("artists").await, 1, "one artist row remains");
    assert_eq!(env.count_rows("entity_redirects").await, 1);
    let ids = env.ids_of(EntityKind::Artist, library_id).await;
    assert!(
        ids.iter()
            .any(|id| id.namespace == provider_namespace(&slug) && id.value == PROVIDER_ARTIST),
        "the survivor holds the provider's own id"
    );

    let user = env.user().await;
    let view = service
        .views()
        .artist(
            provider,
            Viewer {
                user,
                vendor: false,
            },
        )
        .await
        .unwrap()
        .expect("the retired id answers with the survivor");
    assert_eq!(view.artist.id, library_id);
    assert_eq!(
        service
            .canonical(EntityKind::Artist, provider)
            .await
            .unwrap(),
        Some(library_id)
    );
}
