use copro_core::error::Result;
use copro_core::message::{OutputContent, ToolResult};
use copro_core::response::{FinishReason, Usage};
use copro_core::stream::OutputContentDelta;
use std::pin::Pin;

/// Events emitted during one agent turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// A streaming model output delta before the output is committed.
    OutputDelta(OutputContentDelta),
    /// A complete model output committed as an assistant message.
    OutputFinished {
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    /// A tool execution result committed as a tool message.
    ToolResult(ToolResult),
}

/// A stream of [`AgentEvent`]s produced by an agent turn.
pub type AgentStream<'a> =
    Pin<Box<dyn futures_util::Stream<Item = Result<AgentEvent>> + Send + 'a>>;
