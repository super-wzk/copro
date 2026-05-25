use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ModelError {
    #[error("rate limited: {message}")]
    RateLimit {
        retry_after: Option<Duration>,
        message: String,
    },
    #[error("authentication failed: {message}")]
    Auth { message: String },
    #[error("request timed out")]
    Timeout,
    #[error("server error: {message}")]
    Server { message: String },
    #[error("model protocol error: {message}")]
    Protocol { message: String },
    #[error("unknown model error: {message}")]
    Unknown { message: String },
}

impl ModelError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }
}

pub type ModelResult<T> = Result<T, ModelError>;
