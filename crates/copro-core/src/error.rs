use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum Error {
    #[error("model not found: {model_id}")]
    ModelNotFound { model_id: String },
    #[error("provider not found: {provider_id}")]
    ProviderNotFound { provider_id: String },
    #[error("request timed out")]
    Timeout,
    #[error("client error: {message}")]
    Client { message: String },
    #[error("server error: {message}")]
    Server { message: String },
    #[error("model protocol error: {message}")]
    Protocol { message: String },
    #[error("unknown model error: {message}")]
    Unknown { message: String },
}

impl Error {
    pub fn client(message: impl Into<String>) -> Self {
        Self::Client {
            message: message.into(),
        }
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
