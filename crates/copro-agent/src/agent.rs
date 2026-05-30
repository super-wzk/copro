use crate::context::{AgentCommand, AgentContext};
use crate::event::AgentStream;
use crate::run::AgentRunHandle;
use crate::runtime::StopSignal;
use crate::tools::ToolRouter;
use copro_api::error::{Error, Result};
use copro_api::message::Message;
use copro_api::request::GenerateRequestOptions;
use copro_api::stream::Model;
use copro_api::tool::{HostedToolSpec, ToolChoice};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

const COMMAND_BUFFER: usize = 16;
const EVENT_BUFFER: usize = 16;

/// Cloneable handle to an agent context.
#[derive(Clone)]
pub struct Agent {
    tx: mpsc::Sender<AgentCommand>,
    stop_signal: StopSignal,
}

impl Agent {
    pub fn new(model: Arc<dyn Model>, tools: Arc<dyn ToolRouter>) -> Self {
        Self::with_stop_signal(model, tools, StopSignal::new())
    }

    pub fn with_stop_signal(
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolRouter>,
        stop_signal: StopSignal,
    ) -> Self {
        let (tx, rx) = mpsc::channel(COMMAND_BUFFER);
        let context = AgentContext::new(model, tools, stop_signal.clone());
        AgentContext::spawn(context, rx);

        Self { tx, stop_signal }
    }

    pub async fn start_run(&self) -> Result<AgentRunHandle> {
        let (events, rx) = mpsc::channel(EVENT_BUFFER);
        self.tx
            .send(AgentCommand::RunTurn { events })
            .await
            .map_err(|_| agent_closed())?;
        Ok(AgentRunHandle::new(rx, self.stop_signal.clone()))
    }

    /// Run one turn and stream core agent events.
    pub fn run_stream(&self) -> AgentStream {
        let agent = self.clone();

        Box::pin(async_stream::try_stream! {
            let handle = agent.start_run().await?;
            let mut stream = handle.events();
            while let Some(event) = stream.next().await {
                yield event?;
            }
        })
    }

    pub fn stop_signal(&self) -> StopSignal {
        self.stop_signal.clone()
    }

    pub fn request_stop(&self) {
        self.stop_signal.request_stop();
    }

    pub fn clear_stop(&self) {
        self.stop_signal.clear();
    }

    pub async fn push_message(&self, message: Message) -> Result<()> {
        self.call(|reply| AgentCommand::PushMessage { message, reply })
            .await
    }

    pub async fn replace_messages(&self, messages: Vec<Message>) -> Result<()> {
        self.call(|reply| AgentCommand::ReplaceMessages { messages, reply })
            .await
    }

    pub async fn clear_messages(&self) -> Result<()> {
        self.call(|reply| AgentCommand::ClearMessages { reply })
            .await
    }

    pub async fn messages(&self) -> Result<Vec<Message>> {
        self.call(|reply| AgentCommand::Messages { reply }).await
    }

    pub async fn set_options(&self, options: GenerateRequestOptions) -> Result<()> {
        self.call(|reply| AgentCommand::SetOptions { options, reply })
            .await
    }

    pub async fn set_tool_choice(&self, tool_choice: Option<ToolChoice>) -> Result<()> {
        self.call(|reply| AgentCommand::SetToolChoice { tool_choice, reply })
            .await
    }

    pub async fn set_hosted_tools(&self, hosted_tools: Vec<HostedToolSpec>) -> Result<()> {
        self.call(|reply| AgentCommand::SetHostedTools {
            hosted_tools,
            reply,
        })
        .await
    }

    async fn call<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T>>) -> AgentCommand,
    ) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(command(reply))
            .await
            .map_err(|_| agent_closed())?;
        rx.await.map_err(|_| agent_closed())?
    }
}

fn agent_closed() -> Error {
    Error::client("agent context stopped")
}
