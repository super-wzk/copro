use copro_core::error::{Error, Result};
use copro_core::stream::{ModelStream, OutputStreamEvent};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOptions {
    pub timeout: Option<Duration>,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestDeadline {
    expires_at: Option<Instant>,
}

impl RequestDeadline {
    pub fn none() -> Self {
        Self { expires_at: None }
    }

    pub fn from_timeout(timeout: Option<Duration>) -> Self {
        Self {
            expires_at: timeout.map(|timeout| Instant::now() + timeout),
        }
    }

    pub fn from_options(options: &RuntimeOptions) -> Self {
        Self::from_timeout(options.timeout)
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.expires_at
            .map(|expires_at| expires_at.saturating_duration_since(Instant::now()))
    }

    pub async fn run<F, T>(&self, future: F) -> Result<T>
    where
        F: Future<Output = T>,
    {
        match self.expires_at {
            Some(expires_at) => tokio::time::timeout_at(expires_at, future)
                .await
                .map_err(|_| Error::Timeout),
            None => Ok(future.await),
        }
    }

    pub async fn next_model_event(
        &self,
        stream: &mut ModelStream<'_>,
    ) -> Result<Option<OutputStreamEvent>> {
        self.run(stream.next()).await?.transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copro_core::response::FinishReason;

    #[tokio::test]
    async fn deadline_without_timeout_allows_future_to_complete() {
        let deadline = RequestDeadline::none();

        let output = deadline.run(async { 42 }).await.unwrap();

        assert_eq!(output, 42);
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

    #[tokio::test]
    async fn deadline_reads_model_stream_event() {
        let deadline = RequestDeadline::none();
        let event = OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        };
        let mut stream: ModelStream<'_> =
            Box::pin(futures_util::stream::iter(vec![Ok(event.clone())]));

        assert_eq!(
            deadline.next_model_event(&mut stream).await.unwrap(),
            Some(event)
        );
        assert_eq!(deadline.next_model_event(&mut stream).await.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_times_out_pending_model_stream() {
        let deadline = RequestDeadline::from_timeout(Some(Duration::from_secs(5)));
        let mut stream: ModelStream<'_> =
            Box::pin(futures_util::stream::pending::<Result<OutputStreamEvent>>());

        let error = deadline.next_model_event(&mut stream).await.unwrap_err();

        assert_eq!(error, Error::Timeout);
    }
}
