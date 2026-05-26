use async_openai::error::OpenAIError;
use copro_core::error::ModelError;

pub(crate) fn response_error(response: async_openai::types::responses::Response) -> ModelError {
    if let Some(error) = response.error {
        return ModelError::Server {
            message: error.message,
        };
    }

    ModelError::Server {
        message: format!("OpenAI response failed with status {:?}", response.status),
    }
}

pub(crate) fn map_openai_error(error: OpenAIError) -> ModelError {
    match error {
        OpenAIError::Reqwest(error) if error.is_timeout() => ModelError::Timeout,
        OpenAIError::Reqwest(error) => ModelError::Unknown {
            message: error.to_string(),
        },
        OpenAIError::ApiError(error) => map_api_error(&error),
        OpenAIError::JSONDeserialize(error, content) => ModelError::protocol(format!(
            "failed to deserialize OpenAI response: {error}; content: {content}"
        )),
        OpenAIError::FileSaveError(message) | OpenAIError::FileReadError(message) => {
            ModelError::client(message)
        }
        OpenAIError::InvalidArgument(message) => ModelError::client(message),
        OpenAIError::StreamError(error) => ModelError::Unknown {
            message: error.to_string(),
        },
    }
}

fn map_api_error(error: &async_openai::error::ApiErrorResponse) -> ModelError {
    let message = error.to_string();
    let error_type = error.api_error.r#type.as_deref().unwrap_or_default();
    let code = error.api_error.code.as_deref().unwrap_or_default();
    let status = error.status_code.as_u16();

    if status >= 500 || matches!(error_type, "server_error") || code.starts_with("server_") {
        return ModelError::Server { message };
    }

    if (400..500).contains(&status) {
        return ModelError::Client { message };
    }

    ModelError::Unknown { message }
}
