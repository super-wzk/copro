use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderError {
    RateLimit {
        retry_after: Option<Duration>,
        message: String,
    },
    Auth {
        message: String,
    },
    Timeout,
    ServerError {
        message: String,
    },
    Unknown {
        message: String,
    },
}

pub type ModelResult<T> = Result<T, ProviderError>;
