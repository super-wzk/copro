use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ModelError {
    #[error("provider not found: {provider_id}")]
    ProviderNotFound { provider_id: String },
    #[error("provider factory not found: {factory_kind}")]
    ProviderFactoryNotFound { factory_kind: String },
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

impl ModelError {
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

pub type ModelResult<T> = Result<T, ModelError>;
