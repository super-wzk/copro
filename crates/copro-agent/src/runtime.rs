use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Cloneable stop signal checked by agent runtimes at safe transition points.
#[derive(Debug, Clone)]
pub struct StopSignal {
    token: Arc<Mutex<CancellationToken>>,
}

impl StopSignal {
    pub fn new() -> Self {
        Self {
            token: Arc::new(Mutex::new(CancellationToken::new())),
        }
    }

    pub fn request_stop(&self) {
        self.token().cancel();
    }

    pub fn clear(&self) {
        *self.token.lock().expect("stop signal mutex poisoned") = CancellationToken::new();
    }

    pub fn is_requested(&self) -> bool {
        self.token().is_cancelled()
    }

    pub(crate) fn token(&self) -> CancellationToken {
        self.token
            .lock()
            .expect("stop signal mutex poisoned")
            .clone()
    }
}

impl Default for StopSignal {
    fn default() -> Self {
        Self::new()
    }
}
