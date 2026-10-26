//! Tests for the ffmpeg runner's process budget and lifecycle.

use std::time::Duration;

use hearthsound_media::{FfmpegRunner, MediaError};
use tokio::io::AsyncReadExt;

/// How long a freshly spawned ffmpeg may take to write its first bytes.
///
/// A spawn returns as soon as the child process exists, and the child still has
/// to start up, read its input, and fill the pipe. On an idle machine that
/// takes about a tenth of a second, but a loaded machine can stretch it past
/// several seconds, so tests wait for the bytes themselves instead of sleeping
/// for a fixed time.
const FIRST_BYTES_DEADLINE: Duration = Duration::from_secs(30);

/// How long a dropped child may take to exit and return its permit.
const RELEASE_DEADLINE: Duration = Duration::from_secs(10);

/// Waits until no child holds a permit.
///
/// A dropped process keeps its slot until it has actually exited, so a test
/// cannot read the count back immediately after the drop.
async fn wait_for_idle(runner: &FfmpegRunner) {
    let deadline = std::time::Instant::now() + RELEASE_DEADLINE;
    while runner.in_flight() > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped child never returned its permit"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The argument vector of an ffmpeg that runs until it is killed.
fn endless_args() -> Vec<std::ffi::OsString> {
    vec![
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "anullsrc=r=44100".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]
}

#[tokio::test]
async fn runs_a_command_to_completion() {
    let runner = FfmpegRunner::new(None, 2);
    runner
        .run(["-hide_banner".into(), "-version".into()])
        .await
        .unwrap();
    assert_eq!(runner.in_flight(), 0);
    assert_eq!(runner.max_concurrent(), 2);
    assert!(runner.binary().ends_with("ffmpeg"));
}

#[tokio::test]
async fn reports_a_failed_process_with_its_stderr() {
    let runner = FfmpegRunner::new(None, 2);
    let error = runner
        .run([
            "-hide_banner".into(),
            "-v".into(),
            "error".into(),
            "-i".into(),
            "/nonexistent/hearthsound-not-here.flac".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ])
        .await
        .unwrap_err();
    match error {
        MediaError::Process { message, args, .. } => {
            assert!(
                message.contains("hearthsound-not-here.flac"),
                "message was `{message}`"
            );
            assert!(args.contains("-f null -"), "args were `{args}`");
        }
        other => panic!("expected a process error, got {other:?}"),
    }
    assert_eq!(runner.in_flight(), 0);
}

#[tokio::test]
async fn waits_for_a_free_slot_before_spawning() {
    let runner = FfmpegRunner::new(None, 1);
    let first = runner.spawn(endless_args()).await.unwrap();
    assert_eq!(runner.in_flight(), 1);

    let blocked = tokio::time::timeout(Duration::from_millis(300), runner.spawn(endless_args()))
        .await
        .map(|result| result.unwrap());
    assert!(
        blocked.is_err(),
        "the second child started without a permit"
    );

    // Dropping a running child kills it and gives its permit back once it has
    // exited.
    drop(first);
    wait_for_idle(&runner).await;

    let second = tokio::time::timeout(Duration::from_secs(5), runner.spawn(endless_args()))
        .await
        .expect("a freed slot starts the next child")
        .unwrap();
    assert_eq!(runner.in_flight(), 1);
    drop(second);
    wait_for_idle(&runner).await;
}

#[tokio::test]
async fn grows_the_cap_when_the_setting_changes() {
    let runner = FfmpegRunner::new(None, 1);
    let first = runner.spawn(endless_args()).await.unwrap();
    assert_eq!(runner.in_flight(), 1);

    runner.set_max_concurrent(2);
    assert_eq!(runner.max_concurrent(), 2);
    let second = tokio::time::timeout(Duration::from_secs(5), runner.spawn(endless_args()))
        .await
        .expect("the raised cap admits a second child")
        .unwrap();
    assert_eq!(runner.in_flight(), 2);

    // Shrinking the cap takes budget away from callers that have not started.
    runner.set_max_concurrent(1);
    assert_eq!(runner.max_concurrent(), 1);
    let blocked =
        tokio::time::timeout(Duration::from_millis(300), runner.spawn(endless_args())).await;
    assert!(
        blocked.is_err(),
        "a third child started after the cap shrank"
    );

    drop(first);
    drop(second);
    wait_for_idle(&runner).await;
}

/// A dropped child keeps its slot until it has exited, so the budget never runs
/// a replacement beside the process it is tearing down.
#[tokio::test]
async fn dropping_a_child_keeps_its_permit_until_it_exits() {
    let runner = FfmpegRunner::new(None, 1);
    let first = runner.spawn(endless_args()).await.unwrap();
    assert_eq!(runner.in_flight(), 1);

    drop(first);
    assert_eq!(
        runner.in_flight(),
        1,
        "the permit must outlive the child it guards"
    );

    // The child is killed with `SIGKILL`, so the window between the drop and its
    // exit is short; what the test can assert without a child that ignores
    // signals is that the slot is not handed out while the old child holds it,
    // and that it comes back exactly once.
    wait_for_idle(&runner).await;
    let second = tokio::time::timeout(RELEASE_DEADLINE, runner.spawn(endless_args()))
        .await
        .expect("the freed slot starts the next child")
        .unwrap();
    assert_eq!(runner.in_flight(), 1);
    assert_eq!(
        runner.max_concurrent(),
        1,
        "the budget reports the cap in force"
    );
    drop(second);
    wait_for_idle(&runner).await;
}

#[tokio::test]
async fn kills_the_child_when_its_handle_is_dropped() {
    let runner = FfmpegRunner::new(None, 2);
    let mut child = runner
        .spawn([
            "-hide_banner".into(),
            "-v".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=440:sample_rate=44100".into(),
            "-f".into(),
            "s16le".into(),
            "-".into(),
        ])
        .await
        .unwrap();
    let mut stdout = child.take_stdout().expect("stdout is piped");
    // Wait for the child's first bytes before dropping it: the handle has to be
    // dropped while samples are in flight, and a fixed sleep cannot know that
    // they are.
    let mut buffer = [0u8; 4096];
    let first = tokio::time::timeout(FIRST_BYTES_DEADLINE, stdout.read(&mut buffer))
        .await
        .expect("the child wrote its first samples in time")
        .expect("the pipe read succeeds");
    assert_eq!(runner.in_flight(), 1, "the child lost its permit early");

    let reader = tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        let mut total = first;
        while let Ok(read) = stdout.read(&mut buffer).await {
            if read == 0 {
                break;
            }
            total += read;
        }
        total
    });

    let pid = child.id();
    drop(child);

    let total = tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("the pipe closed after the drop")
        .unwrap();
    assert!(total > 0, "the child produced no samples");
    wait_for_idle(&runner).await;
    assert!(pid.is_some());
}
