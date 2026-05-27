use copro_core::error::Result;
use copro_core::message::{InputContent, OutputContent, ToolResultStatus};
use copro_core::response::{FinishReason, Usage};
use copro_core::stream::OutputContentDelta;
use std::pin::Pin;

/// Events emitted during one agent turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// A streaming model output delta before the output is committed.
    OutputDelta { delta: OutputContentDelta },
    /// A complete model output committed as an assistant message.
    Output {
        content: Vec<OutputContent>,
        finish_reason: FinishReason,
        usage: Option<Usage>,
    },
    /// A tool execution result committed as a tool message.
    ToolResult {
        call_id: String,
        name: String,
        status: ToolResultStatus,
        content: Vec<InputContent>,
    },
    /// The whole agent turn has completed after all tool rounds.
    TurnFinish,
}

/// A stream of [`AgentEvent`]s produced by an agent turn.
pub type AgentStream<'a> =
    Pin<Box<dyn futures_util::Stream<Item = Result<AgentEvent>> + Send + 'a>>;
