use coox_harness::request::{RequestInjector, RequestPipeline};
use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{InputContent, InputMessage, Message};
use copro_api::request::{GenerateRequest, GenerateRequestOptions};
use std::sync::Arc;

#[tokio::test]
async fn request_pipeline_runs_injectors_in_order() {
    let pipeline = RequestPipeline::new(vec![
        Arc::new(PushDeveloper("first")),
        Arc::new(PushDeveloper("second")),
    ]);
    let mut request = request_with_messages(vec![Message::user(vec![InputContent::Text(
        "hello".to_string(),
    )])]);

    pipeline.prepare_request(&mut request).await.unwrap();

    assert_eq!(message_text(&request.messages[1]), "first");
    assert_eq!(message_text(&request.messages[2]), "second");
}

#[tokio::test]
async fn request_pipeline_builder_adds_injectors() {
    let pipeline = RequestPipeline::default()
        .with_injector(Arc::new(PushDeveloper("first")))
        .with_injector(Arc::new(PushDeveloper("second")));

    assert_eq!(pipeline.len(), 2);
    assert!(!pipeline.is_empty());
}

struct PushDeveloper(&'static str);

#[async_trait]
impl RequestInjector for PushDeveloper {
    async fn prepare_request(&self, request: &mut GenerateRequest) -> Result<()> {
        request
            .messages
            .push(Message::developer(vec![InputContent::Text(
                self.0.to_string(),
            )]));
        Ok(())
    }
}

fn request_with_messages(messages: Vec<Message>) -> GenerateRequest {
    GenerateRequest {
        messages,
        tools: Vec::new(),
        tool_choice: None,
        hosted_tools: Vec::new(),
        options: GenerateRequestOptions::default(),
    }
}

fn message_text(message: &Message) -> &str {
    let Message::Input(InputMessage::Developer(content)) = message else {
        panic!("expected developer message");
    };
    let Some(InputContent::Text(text)) = content.first() else {
        panic!("expected text content");
    };
    text
}
