use crate::event::AgentEvent;
use crate::hook::{AgentHook, AgentHooks};
use crate::run::{AgentControl, AgentRun, AgentRunId, AgentTurnId};
use crate::runtime::StopSignal;
use crate::tools::ToolRouter;
use copro_api::error::Result;
use copro_api::message::Message;
use copro_api::request::GenerateRequestOptions;
use copro_api::stream::Model;
use copro_api::tool::{HostedToolSpec, ToolChoice};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub(crate) struct AgentContext {
    pub(crate) model: Arc<dyn Model>,
    pub(crate) tools: Arc<dyn ToolRouter>,
    pub(crate) hooks: AgentHooks,
    pub(crate) stop_signal: StopSignal,
    pub(crate) messages: Vec<Message>,
    pub(crate) tool_choice: Option<ToolChoice>,
    pub(crate) hosted_tools: Vec<HostedToolSpec>,
    pub(crate) options: GenerateRequestOptions,
    next_run_id: AgentRunId,
    next_turn_id: AgentTurnId,
}

impl AgentContext {
    pub(crate) fn new(
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolRouter>,
        stop_signal: StopSignal,
    ) -> Self {
        Self {
            model,
            tools,
            hooks: AgentHooks::new(),
            stop_signal,
            messages: Vec::new(),
            tool_choice: None,
            hosted_tools: Vec::new(),
            options: GenerateRequestOptions::default(),
            next_run_id: AgentRunId(0),
            next_turn_id: AgentTurnId(0),
        }
    }

    pub(crate) fn allocate_run_ids(&mut self) -> (AgentRunId, AgentTurnId) {
        let run_id = self.next_run_id;
        let turn_id = self.next_turn_id;
        self.next_run_id = AgentRunId(self.next_run_id.0 + 1);
        self.next_turn_id = AgentTurnId(self.next_turn_id.0 + 1);
        (run_id, turn_id)
    }

    pub(crate) fn spawn(context: Self, rx: mpsc::Receiver<AgentCommand>) {
        tokio::spawn(context.run(rx));
    }

    async fn run(mut self, mut rx: mpsc::Receiver<AgentCommand>) {
        while let Some(command) = rx.recv().await {
            match command {
                AgentCommand::RunTurn { events } => {
                    AgentRun::new(&mut self).run_turn(events).await;
                }
                AgentCommand::PushMessage { message, reply } => {
                    self.messages.push(message);
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::ReplaceMessages { messages, reply } => {
                    self.messages = messages;
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::ClearMessages { reply } => {
                    self.messages.clear();
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::Messages { reply } => {
                    let _ = reply.send(Ok(self.messages.clone()));
                }
                AgentCommand::AddHook { hook, reply } => {
                    self.hooks.push(hook);
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::SetOptions { options, reply } => {
                    self.options = options;
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::SetToolChoice { tool_choice, reply } => {
                    self.tool_choice = tool_choice;
                    let _ = reply.send(Ok(()));
                }
                AgentCommand::SetHostedTools {
                    hosted_tools,
                    reply,
                } => {
                    self.hosted_tools = hosted_tools;
                    let _ = reply.send(Ok(()));
                }
            }
        }
    }
}

pub(crate) enum AgentCommand {
    RunTurn {
        events: mpsc::Sender<AgentStreamItem>,
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
    AddHook {
        hook: Arc<dyn AgentHook>,
        reply: oneshot::Sender<Result<()>>,
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
    Event(AgentEvent, oneshot::Sender<AgentControl>),
    Error(copro_api::error::Error),
}
