use crate::request::GenerateRequest;
use crate::stream::ModelStream;

pub trait ChatModel: Send + Sync {
    fn stream(&self, request: GenerateRequest) -> ModelStream<'_>;
}
