use crate::cancel::RunCancellation;
use crate::event::AgentEvent;
use crate::run::{AgentControlSignal, AgentRun, AgentRunId};
use crate::tools::ToolRouter;
use copro_api::error::Result;
use copro_api::message::Message;
use copro_api::request::GenerateRequestOptions;
use copro_api::stream::Model;
use copro_api::tool::{HostedToolSpec, ToolChoice};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentContext {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    messages: Vec<Message>,
    #[serde(default)]
    options: GenerateRequestOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hosted_tools: Vec<HostedToolSpec>,
}

impl AgentContext {
    pub fn new(
        messages: Vec<Message>,
        options: GenerateRequestOptions,
        tool_choice: Option<ToolChoice>,
        hosted_tools: Vec<HostedToolSpec>,
    ) -> Self {
        Self {
            messages,
            options,
            tool_choice,
            hosted_tools,
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn options(&self) -> &GenerateRequestOptions {
        &self.options
    }

    pub fn tool_choice(&self) -> Option<&ToolChoice> {
        self.tool_choice.as_ref()
    }

    pub fn hosted_tools(&self) -> &[HostedToolSpec] {
        &self.hosted_tools
    }

    pub(crate) fn messages_mut(&mut self) -> &mut Vec<Message> {
        &mut self.messages
    }

    pub(crate) fn push_message(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub(crate) fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    pub(crate) fn clear_messages(&mut self) {
        self.messages.clear();
    }

    pub(crate) fn set_options(&mut self, options: GenerateRequestOptions) {
        self.options = options;
    }

    pub(crate) fn set_tool_choice(&mut self, tool_choice: Option<ToolChoice>) {
        self.tool_choice = tool_choice;
    }

    pub(crate) fn set_hosted_tools(&mut self, hosted_tools: Vec<HostedToolSpec>) {
        self.hosted_tools = hosted_tools;
    }
}

pub(crate) struct AgentState {
    pub(crate) model: Arc<dyn Model>,
    pub(crate) tools: Arc<dyn ToolRouter>,
    pub(crate) context: AgentContext,
    next_run_id: AgentRunId,
}

impl AgentState {
    pub(crate) fn new(
        context: AgentContext,
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolRouter>,
    ) -> Self {
        Self {
            model,
            tools,
            context,
            next_run_id: AgentRunId(0),
        }
    }

    pub(crate) fn allocate_run_id(&mut self) -> AgentRunId {
        let run_id = self.next_run_id;
        self.next_run_id = AgentRunId(self.next_run_id.0 + 1);
        run_id
    }

    pub(crate) fn spawn(state: Self, rx: mpsc::Receiver<AgentCommand>) {
        tokio::spawn(state.run(rx));
    }

    async fn run(mut self, mut rx: mpsc::Receiver<AgentCommand>) {
        while let Some(command) = rx.recv().await {
            match command {
                AgentCommand::RunTurn {
                    events,
                    cancellation,
                } => {
                    AgentRun::new(&mut self, cancellation)
                        .run_turn(events)
                        .await;
                }
                AgentCommand::PushMessage { message, reply } => {
                    self.context.push_message(message);
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::ReplaceMessages { messages, reply } => {
                    self.context.replace_messages(messages);
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::ClearMessages { reply } => {
                    self.context.clear_messages();
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::Messages { reply } => {
                    let _ = reply.send(Ok(self.context.messages().to_vec()));
                }
                AgentCommand::Context { reply } => {
                    let _ = reply.send(Ok(self.context.clone()));
                }
                AgentCommand::SetOptions { options, reply } => {
                    self.context.set_options(options);
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::SetToolChoice { tool_choice, reply } => {
                    self.context.set_tool_choice(tool_choice);
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::SetHostedTools {
                    hosted_tools,
                    reply,
                } => {
                    self.context.set_hosted_tools(hosted_tools);
                    let _ = reply.send(Ok(()));
                }
            }
        }
    }
}

pub(crate) enum AgentCommand {
    RunTurn {
        events: mpsc::Sender<AgentStreamItem>,
        cancellation: RunCancellation,
    },
    PushMessage {
        message: Message,
        reply: oneshot::Sender<Result<()>>,
    },
    ReplaceMessages {
        messages: Vec<Message>,
        reply: oneshot::Sender<Result<()>>,
    },
    ClearMessages {
        reply: oneshot::Sender<Result<()>>,
    },
    Messages {
        reply: oneshot::Sender<Result<Vec<Message>>>,
    },
    Context {
        reply: oneshot::Sender<Result<AgentContext>>,
    },
    SetOptions {
        options: GenerateRequestOptions,
        reply: oneshot::Sender<Result<()>>,
    },
    SetToolChoice {
        tool_choice: Option<ToolChoice>,
        reply: oneshot::Sender<Result<()>>,
    },
    SetHostedTools {
        hosted_tools: Vec<HostedToolSpec>,
        reply: oneshot::Sender<Result<()>>,
    },
}

pub(crate) enum AgentStreamItem {
    Event(Box<AgentEvent>, oneshot::Sender<AgentControlSignal>),
    Error(copro_api::error::Error),
}
