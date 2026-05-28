use copro_api::error::{Error, Result};
use copro_api::stream::{ModelStream, OutputStreamEvent};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOptions {
    pub timeout: Option<Duration>,
}

/// Cloneable stop signal checked by agent runtimes at safe transition points.
#[derive(Debug, Clone)]
pub struct StopSignal {
    requested: watch::Sender<bool>,
}

impl StopSignal {
    pub fn new() -> Self {
        let (requested, _receiver) = watch::channel(false);
        Self { requested }
    }

    pub fn request_stop(&self) {
        self.requested.send_replace(true);
    }

    pub fn clear(&self) {
        self.requested.send_replace(false);
    }

    pub fn is_requested(&self) -> bool {
        *self.requested.borrow()
    }

    pub(crate) async fn wait_requested(&self) {
        let mut requested = self.requested.subscribe();
        loop {
            if *requested.borrow_and_update() {
                return;
            }
            if requested.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Default for StopSignal {
    fn default() -> Self {
        Self::new()
    }
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
