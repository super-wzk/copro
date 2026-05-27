use copro_agent::{Agent, AgentEvent, AgentHook};
use copro_core::error::Result;
use copro_core::message::{InputContent, Message, OutputContent};
use copro_core::provider::Chat;
use copro_core::request::GenerateRequest;
use copro_core::response::FinishReason;
use copro_core::stream::{ModelStream, OutputContentDelta, OutputStreamEvent};
use futures_util::StreamExt;
use std::sync::Arc;

#[tokio::test]
async fn run_stream_commits_assistant_message() {
    let mut agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text {
                text: "Hello".to_string(),
            },
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    agent.messages = vec![Message::User {
        content: vec![InputContent::Text {
            text: "hi".to_string(),
        }],
    }];

    let events = agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    let assistant = Message::Assistant {
        content: vec![OutputContent::Text {
            text: "Hello".to_string(),
        }],
    };
    assert_eq!(
        events,
        vec![
            AgentEvent::OutputDelta {
                delta: OutputContentDelta::Text {
                    text: "Hello".to_string(),
                },
            },
            AgentEvent::Output {
                content: vec![OutputContent::Text {
                    text: "Hello".to_string(),
                }],
                finish_reason: FinishReason::Stop,
                usage: None,
            },
            AgentEvent::TurnFinish,
        ]
    );
    assert_eq!(
        agent.messages,
        vec![
            Message::User {
                content: vec![InputContent::Text {
                    text: "hi".to_string(),
                }],
            },
            assistant,
        ]
    );
}

#[tokio::test]
async fn on_output_finished_hook_can_modify_output() {
    let mut agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text {
                text: "secret".to_string(),
            },
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    agent.hooks.push(Arc::new(RedactHook));

    let events = agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    let redacted = vec![OutputContent::Text {
        text: "redacted".to_string(),
    }];
    assert_eq!(
        events,
        vec![
            AgentEvent::OutputDelta {
                delta: OutputContentDelta::Text {
                    text: "secret".to_string(),
                },
            },
            AgentEvent::Output {
                content: redacted.clone(),
                finish_reason: FinishReason::Stop,
                usage: None,
            },
            AgentEvent::TurnFinish,
        ]
    );
    assert_eq!(
        agent.messages,
        vec![Message::Assistant { content: redacted }]
    );
}

fn test_agent(events: Vec<OutputStreamEvent>) -> Agent {
    Agent::new(Arc::new(TestChat { events }))
}

struct RedactHook;

impl AgentHook for RedactHook {
    fn on_output_finished(&self, message: &mut Message) -> Result<()> {
        if let Message::Assistant { content } = message {
            *content = vec![OutputContent::Text {
                text: "redacted".to_string(),
            }];
        }
        Ok(())
    }
}

struct TestChat {
    events: Vec<OutputStreamEvent>,
}

impl Chat for TestChat {
    fn stream(&self, _request: GenerateRequest) -> ModelStream<'_> {
        Box::pin(futures_util::stream::iter(
            self.events.clone().into_iter().map(Ok),
        ))
    }
}
