use async_openai::error::OpenAIError;
use copro_api::error::Error;

pub(crate) fn map_openai_error(error: OpenAIError) -> Error {
    match error {
        OpenAIError::Reqwest(error) if error.is_timeout() => Error::Timeout,
        OpenAIError::Reqwest(error) => Error::Unknown {
            message: error.to_string(),
        },
        OpenAIError::ApiError(error) => map_api_error(&error),
        OpenAIError::JSONDeserialize(error, content) => Error::protocol(format!(
            "failed to deserialize OpenAI response: {error}; content: {content}"
        )),
        OpenAIError::FileSaveError(message) | OpenAIError::FileReadError(message) => {
            Error::client(message)
        }
        OpenAIError::InvalidArgument(message) => Error::client(message),
        OpenAIError::StreamError(error) => Error::Unknown {
            message: error.to_string(),
        },
    }
}

fn map_api_error(error: &async_openai::error::ApiErrorResponse) -> Error {
    let message = error.to_string();
    let error_type = error.api_error.r#type.as_deref().unwrap_or_default();
    let code = error.api_error.code.as_deref().unwrap_or_default();
    let status = error.status_code.as_u16();

    if status >= 500 || matches!(error_type, "server_error") || code.starts_with("server_") {
        return Error::Server { message };
    }

    if (400..500).contains(&status) {
        return Error::Client { message };
    }

    Error::Unknown { message }
}
