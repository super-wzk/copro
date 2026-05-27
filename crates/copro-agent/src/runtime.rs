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
