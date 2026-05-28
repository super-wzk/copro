use copro_agent::runtime::{RequestDeadline, RuntimeOptions, StopSignal};
use copro_api::error::{Error, Result};
use copro_api::stream::{ModelStream, OutputStreamEvent};
use std::time::Duration;

#[test]
fn stop_signal_tracks_requested_state_across_clones() {
    let signal = StopSignal::new();
    let clone = signal.clone();

    assert!(!signal.is_requested());
    clone.request_stop();
    assert!(signal.is_requested());
    signal.clear();
    assert!(!clone.is_requested());
}

#[tokio::test(start_paused = true)]
async fn deadline_times_out_future() {
    let options = RuntimeOptions {
        timeout: Some(Duration::from_secs(5)),
    };
    let deadline = RequestDeadline::from_options(&options);

    let error = deadline
        .run(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            42
        })
        .await
        .unwrap_err();

    assert_eq!(error, Error::Timeout);
}

#[tokio::test(start_paused = true)]
async fn deadline_uses_remaining_time_across_runs() {
    let deadline = RequestDeadline::from_timeout(Some(Duration::from_secs(5)));

    deadline
        .run(async {
            tokio::time::sleep(Duration::from_secs(3)).await;
        })
        .await
        .unwrap();

    let error = deadline
        .run(async {
            tokio::time::sleep(Duration::from_secs(3)).await;
        })
        .await
        .unwrap_err();

    assert_eq!(error, Error::Timeout);
}

#[tokio::test(start_paused = true)]
async fn deadline_times_out_pending_model_stream() {
    let deadline = RequestDeadline::from_timeout(Some(Duration::from_secs(5)));
    let mut stream: ModelStream<'_> =
        Box::pin(futures_util::stream::pending::<Result<OutputStreamEvent>>());

    let error = deadline.next_model_event(&mut stream).await.unwrap_err();

    assert_eq!(error, Error::Timeout);
}
