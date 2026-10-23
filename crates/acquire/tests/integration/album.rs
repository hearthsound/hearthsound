//! Album-granular acquisition: one source folder, many tracks.
//!
//! An album job searches the album layer for the whole release, downloads one
//! folder, imports each file against the track it maps to and hands whatever
//! did not import to the per-track pipeline.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_acquire::orchestrator::{AcquireRequest, AlbumAcquireRequest};
use hearthsound_core::{AcquisitionState, Availability, EntityId, Granularity, Priority};
use serde_json::json;

use crate::common::{
    Env, INSTANCE, InstanceLimits, album_folder, album_instance_config, config, one_candidate,
};

/// An album request the tests admit directly.
fn request(album: EntityId) -> AlbumAcquireRequest {
    AlbumAcquireRequest {
        album_id: album,
        priority: Priority::Play,
        reason: "save album".to_owned(),
        requested_by: None,
        can_acquire: true,
        force: false,
    }
}

#[tokio::test]
async fn one_folder_imports_every_track_of_the_album() {
    let folders = json!([album_folder(
        "Hello Album",
        &[
            ("01 - Hello Album 1.flac", "flac", None),
            ("02 - Hello Album 2.flac", "flac", None),
        ],
    )]);
    let env = Env::new(
        album_instance_config(folders),
        InstanceLimits::default(),
        &[],
    )
    .await;
    let (album, tracks) = env.add_album("Hello Album", "Hello Artist", 2).await;
    for name in ["01 - Hello Album 1.flac", "02 - Hello Album 2.flac"] {
        env.place_album_file("Hello Album", name).await;
    }

    let outcomes = env
        .orchestrator
        .request_album(request(album))
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    let id = outcomes[0].outcome.acquisition_id().unwrap();
    for outcome in &outcomes {
        assert_eq!(
            outcome.outcome.acquisition_id(),
            Some(id),
            "every track of the album reports the same job"
        );
    }
    let row = env.acquisition(id).await;
    assert_eq!(row.granularity, Granularity::Album);
    assert_eq!(row.track_id, None);
    assert_eq!(row.album_id, Some(album));

    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Completed);

    for track in &tracks {
        assert_eq!(
            env.track(*track).await.availability,
            Availability::Cached,
            "every track of the folder was imported"
        );
    }
    let media_files: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM media_files")
        .fetch_one(env.database.readers())
        .await
        .unwrap();
    assert_eq!(media_files, 2);
    let acquisitions: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM acquisitions")
        .fetch_one(env.database.readers())
        .await
        .unwrap();
    assert_eq!(acquisitions, 1, "one album acquisition, no per-track rows");
    assert_eq!(env.downloads_started(INSTANCE).await, 1);
}

#[tokio::test]
async fn a_track_request_joins_the_albums_active_acquisition() {
    let folders = json!([album_folder(
        "Hello Album",
        &[
            ("01 - Hello Album 1.flac", "flac", None),
            ("02 - Hello Album 2.flac", "flac", None),
        ],
    )]);
    let env = Env::new(
        album_instance_config(folders),
        InstanceLimits::default(),
        &[],
    )
    .await;
    let (album, tracks) = env.add_album("Hello Album", "Hello Artist", 2).await;
    let outcomes = env
        .orchestrator
        .request_album(request(album))
        .await
        .unwrap();
    let album_id = outcomes[0].outcome.acquisition_id().unwrap();

    let joined = env
        .orchestrator
        .request(AcquireRequest::new(tracks[1], Priority::Play, "play"))
        .await
        .unwrap();
    let handle = joined.handle().expect("the request joins the album job");
    assert_eq!(handle.id, album_id);
    assert_eq!(handle.track_id, Some(tracks[1]));
    assert_eq!(
        env.track(tracks[1]).await.availability,
        Availability::Acquiring,
        "the track is being acquired by the album job"
    );
    let acquisitions: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM acquisitions")
        .fetch_one(env.database.readers())
        .await
        .unwrap();
    assert_eq!(acquisitions, 1);
}

#[tokio::test]
async fn a_track_whose_file_failed_gets_its_own_acquisition() {
    const TRACKS: usize = 10;
    let files: Vec<(String, &str, Option<u32>)> = (1..=TRACKS)
        .map(|number| {
            (
                format!("{number:02} - Hello Album {number}.flac"),
                "flac",
                None,
            )
        })
        .collect();
    let files: Vec<(&str, &str, Option<u32>)> = files
        .iter()
        .map(|(name, format, bitrate)| (name.as_str(), *format, *bitrate))
        .collect();
    let env = Env::new(
        album_instance_config(json!([album_folder("Hello Album", &files)])),
        InstanceLimits::default(),
        &[],
    )
    .await;
    let (album, tracks) = env.add_album("Hello Album", "Hello Artist", TRACKS).await;
    for number in 1..=TRACKS {
        let name = format!("{number:02} - Hello Album {number}.flac");
        if number == 3 {
            env.place_undecodable_album_file("Hello Album", &name).await;
        } else {
            env.place_album_file("Hello Album", &name).await;
        }
    }

    let outcomes = env
        .orchestrator
        .request_album(request(album))
        .await
        .unwrap();
    let id = outcomes[0].outcome.acquisition_id().unwrap();
    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(
        row.state,
        AcquisitionState::Completed,
        "one bad file does not lose the tracks that did import"
    );

    let media_files: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM media_files")
        .fetch_one(env.database.readers())
        .await
        .unwrap();
    assert_eq!(media_files, i64::try_from(TRACKS - 1).unwrap());
    let failed_track = tracks[2];
    env.wait_for_track_acquisition(failed_track).await;
    let row = sqlx::query!(
        "SELECT granularity FROM acquisitions WHERE track_id = ?",
        failed_track.uuid().as_bytes().to_vec(),
    )
    .fetch_one(env.database.readers())
    .await
    .unwrap();
    assert_eq!(row.granularity, "track");
}

#[tokio::test]
async fn an_album_without_a_folder_falls_back_to_track_acquisitions() {
    let env = Env::new(
        album_instance_config(json!([])),
        InstanceLimits::default(),
        &[],
    )
    .await;
    let (album, tracks) = env.add_album("Hello Album", "Hello Artist", 2).await;

    let outcomes = env
        .orchestrator
        .request_album(request(album))
        .await
        .unwrap();
    let album_id = outcomes[0].outcome.acquisition_id().unwrap();
    env.start();
    let row = env.wait_for_terminal(album_id).await;
    assert_eq!(row.state, AcquisitionState::Failed);

    let tracks = tracks.clone();
    for track in &tracks {
        env.wait_for_track_acquisition(*track).await;
    }
    for track in tracks {
        let granularity: String = sqlx::query_scalar!(
            "SELECT granularity FROM acquisitions WHERE track_id = ?",
            track.uuid().as_bytes().to_vec(),
        )
        .fetch_one(env.database.readers())
        .await
        .unwrap();
        assert_eq!(granularity, "track");
    }
}

#[tokio::test]
async fn an_instance_without_album_capability_fans_out_per_track() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    let (album, tracks) = env.add_album("Hello Album", "Hello Artist", 2).await;

    let outcomes = env
        .orchestrator
        .request_album(request(album))
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    let first = outcomes[0].outcome.acquisition_id().unwrap();
    let second = outcomes[1].outcome.acquisition_id().unwrap();
    assert_ne!(first, second, "each track has its own acquisition");
    for (outcome, track) in outcomes.iter().zip(&tracks) {
        assert_eq!(outcome.track_id, *track);
        let row = env
            .acquisition(outcome.outcome.acquisition_id().unwrap())
            .await;
        assert_eq!(row.granularity, Granularity::Track);
        assert_eq!(row.track_id, Some(*track));
    }
}
