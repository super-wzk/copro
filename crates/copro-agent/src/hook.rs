use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{Message, OutputContent, ToolCall, ToolResult};
use copro_api::request::GenerateRequest;
use copro_api::stream::OutputContentDelta;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// Collection of agent hooks with centralized lifecycle dispatch.
#[derive(Default, Clone)]
pub struct AgentHooks {
    hooks: Vec<Arc<dyn AgentHook>>,
}

impl AgentHooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn before_turn(&self, messages: &mut Vec<Message>) -> Result<()> {
        for hook in &self.hooks {
            hook.before_turn(messages).await?;
        }
        Ok(())
    }

    pub(crate) async fn before_request(&self, request: &mut GenerateRequest) -> Result<()> {
        for hook in &self.hooks {
            hook.before_request(request).await?;
        }
        Ok(())
    }

    pub(crate) async fn before_output_delta(
        &self,
        content_index: usize,
        delta: &mut OutputContentDelta,
    ) -> Result<()> {
        for hook in &self.hooks {
            hook.before_output_delta(content_index, delta).await?;
        }
        Ok(())
    }

    pub(crate) async fn before_tool_plan(&self, tool_calls: &mut Vec<ToolCall>) -> Result<()> {
        for hook in &self.hooks {
            hook.before_tool_plan(tool_calls).await?;
        }
        Ok(())
    }

    pub(crate) async fn before_tool_call(&self, tool: &mut ToolCall) -> Result<ToolCallDecision> {
        for hook in &self.hooks {
            match hook.before_tool_call(tool).await? {
                ToolCallDecision::Allow => {}
                decision => return Ok(decision),
            }
        }
        Ok(ToolCallDecision::Allow)
    }

    pub(crate) async fn before_tool_result_commit(
        &self,
        tool: &ToolCall,
        result: &mut ToolResult,
    ) -> Result<()> {
        for hook in &self.hooks {
            hook.before_tool_result_commit(tool, result).await?;
        }
        Ok(())
    }

    pub(crate) async fn before_output_commit(
        &self,
        content: &mut Vec<OutputContent>,
    ) -> Result<()> {
        for hook in &self.hooks {
            hook.before_output_commit(content).await?;
        }
        Ok(())
    }

    pub(crate) async fn after_output_commit(&self, content: &[OutputContent]) -> Result<()> {
        for hook in &self.hooks {
            hook.after_output_commit(content).await?;
        }
        Ok(())
    }

    pub(crate) async fn after_tool_result_commit(
        &self,
        tool: &ToolCall,
        result: &ToolResult,
    ) -> Result<()> {
        for hook in &self.hooks {
            hook.after_tool_result_commit(tool, result).await?;
        }
        Ok(())
    }

    pub(crate) async fn after_turn(&self, messages: &[Message]) -> Result<()> {
        for hook in &self.hooks {
            hook.after_turn(messages).await?;
        }
        Ok(())
    }
}

impl Deref for AgentHooks {
    type Target = Vec<Arc<dyn AgentHook>>;

    fn deref(&self) -> &Self::Target {
        &self.hooks
    }
}

impl DerefMut for AgentHooks {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.hooks
    }
}

impl From<Vec<Arc<dyn AgentHook>>> for AgentHooks {
    fn from(hooks: Vec<Arc<dyn AgentHook>>) -> Self {
        Self { hooks }
    }
}

impl FromIterator<Arc<dyn AgentHook>> for AgentHooks {
    fn from_iter<T: IntoIterator<Item = Arc<dyn AgentHook>>>(iter: T) -> Self {
        Self {
            hooks: iter.into_iter().collect(),
        }
    }
}

/// Hook points that can inspect or modify agent execution.
#[async_trait]
pub trait AgentHook: Send + Sync {
    async fn before_turn(&self, _messages: &mut Vec<Message>) -> Result<()> {
        Ok(())
    }

    async fn before_request(&self, _request: &mut GenerateRequest) -> Result<()> {
        Ok(())
    }

    async fn before_output_delta(
        &self,
        _content_index: usize,
        _delta: &mut OutputContentDelta,
    ) -> Result<()> {
        Ok(())
    }

    async fn before_output_commit(&self, _content: &mut Vec<OutputContent>) -> Result<()> {
        Ok(())
    }

    async fn after_output_commit(&self, _content: &[OutputContent]) -> Result<()> {
        Ok(())
    }

    /// Inspect or rewrite the full batch of tool calls during planning, before
    /// any of them are authorized or executed.
    async fn before_tool_plan(&self, _tool_calls: &mut Vec<ToolCall>) -> Result<()> {
        Ok(())
    }

    /// Authorize a single tool call during the planning stage.
    ///
    /// This is an approval gate, not an execution-time hook: it runs while the
    /// turn is building its execution plan, before any tool runs and possibly
    /// well before this specific tool is dispatched. Returning
    /// [`ToolCallDecision::Reject`] short-circuits the tool with an error
    /// result instead of executing it. The actual start of execution is
    /// observable via the `ToolStarted` agent event.
    async fn before_tool_call(&self, _tool: &mut ToolCall) -> Result<ToolCallDecision> {
        Ok(ToolCallDecision::Allow)
    }

    /// Rewrite a tool result just before it is committed to conversation
    /// history.
    ///
    /// This runs during the commit stage in deterministic plan order, after the
    /// tool has finished executing but before the result is pushed onto the
    /// message history. For parallel batches it does not fire the instant an
    /// individual tool resolves; results are committed together once the batch
    /// completes. Mirror of [`AgentHook::before_output_commit`] for tool
    /// results. The read-only counterpart is
    /// [`AgentHook::after_tool_result_commit`].
    async fn before_tool_result_commit(
        &self,
        _tool: &ToolCall,
        _result: &mut ToolResult,
    ) -> Result<()> {
        Ok(())
    }

    /// Observe a tool result after it has been committed to conversation
    /// history. Read-only counterpart of
    /// [`AgentHook::before_tool_result_commit`].
    async fn after_tool_result_commit(&self, _tool: &ToolCall, _result: &ToolResult) -> Result<()> {
        Ok(())
    }

    async fn after_turn(&self, _messages: &[Message]) -> Result<()> {
        Ok(())
    }
}

/// Decision returned by [`AgentHook::before_tool_call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallDecision {
    Allow,
    Reject { reason: String },
}
