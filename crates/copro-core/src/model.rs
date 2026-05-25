use crate::request::GenerateRequest;
use crate::stream::ModelStream;

pub trait ChatModel {
    fn stream(&'_ self, request: GenerateRequest) -> ModelStream<'_>;
}
