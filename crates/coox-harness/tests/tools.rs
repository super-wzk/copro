use coox_harness::tool;
use coox_harness::tools::{
    CompositeToolRouter, ErasedTool, FnTool, LocalToolRouter, Tool, ToolContext,
    ToolExecutionPolicy, ToolSlots, ToolUpdateParts, ToolUpdatePayload, ToolUpdateSlot,
};
use copro_agent::{CancellationToken, ToolRouter};
use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_api::tool::ToolDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::result::Result as StdResult;
use std::sync::Arc;

#[tokio::test]
async fn fn_tool_wraps_async_functions() {
    let router = LocalToolRouter::new(vec![tool!("echo", "Echo a message.", echo)]);

    let definitions = router.definitions().await.unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "echo");
    assert_eq!(definitions[0].description, "Echo a message.");

    let result = router
        .execute(
            ToolCall {
                id: "call-echo".into(),
                name: "echo".to_string(),
                arguments: serde_json::Map::from_iter([("message".to_string(), json!("hello"))]),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(result.name, "echo");
    assert_eq!(tool_text(&result), "echo: hello");
}

#[tokio::test]
async fn fn_tool_can_be_used_directly() {
    let tool = FnTool::new("echo", "Echo a message.", echo);

    let definition = tool.definition();
    assert_eq!(definition.name, "echo");
    assert_eq!(definition.description, "Echo a message.");

    let content = tool
        .call_content(
            json!({ "message": "direct" }),
            ToolContext::without_slots("call-direct", "echo", CancellationToken::new()),
        )
        .await
        .unwrap();

    assert_eq!(content_text(&content), "echo: direct");
}

#[tokio::test]
async fn fn_tool_wraps_async_closures() {
    let router = LocalToolRouter::new(vec![tool!(
        "length",
        "Return the message length.",
        |input: EchoInput, _context: ToolContext| async move { Ok::<_, String>(input.message.len()) },
    )]);

    let result = router
        .execute(
            ToolCall {
                id: "call-length".into(),
                name: "length".to_string(),
                arguments: serde_json::Map::from_iter([("message".to_string(), json!("hello"))]),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "5");
}

#[tokio::test]
async fn fn_tool_can_return_no_output() {
    let router = LocalToolRouter::new(vec![tool!(
        "noop",
        "Return no output.",
        |_input: EmptyInput, _context: ToolContext| async move { Ok::<_, String>(()) },
    )]);

    let result = router
        .execute(
            ToolCall {
                id: "call-noop".into(),
                name: "noop".to_string(),
                arguments: serde_json::Map::new(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(result.content.is_empty());
}

#[tokio::test]
async fn tool_context_emits_typed_updates_through_slots() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let slots = ToolSlots::new().with(ToolUpdateSlot::new(move |update| {
        let tx = tx.clone();
        async move {
            tx.send(update).await.unwrap();
        }
    }));
    let router = LocalToolRouter::new(vec![Arc::new(UpdateTool)]).with_slots(slots);

    let result = router
        .execute(call("updater"), CancellationToken::new())
        .await
        .unwrap();
    let update = rx.recv().await.unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "done");
    assert_eq!(update.call_id.as_str(), "call-updater");
    assert_eq!(update.tool_name, "updater");
    assert_eq!(update.sequence, 0);
    assert_eq!(update.kind, StatusUpdate::KIND);
    assert_eq!(update.payload, json!({ "message": "running" }));
}

#[tokio::test]
async fn tool_context_updates_are_noop_without_slot() {
    let router = LocalToolRouter::new(vec![Arc::new(UpdateTool)]);

    let result = router
        .execute(call("updater"), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "done");
}

#[tokio::test]
async fn tool_context_emits_raw_update_parts_through_slots() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let slots = ToolSlots::new().with(ToolUpdateSlot::new(move |update| {
        let tx = tx.clone();
        async move {
            tx.send(update).await.unwrap();
        }
    }));
    let router = LocalToolRouter::new(vec![Arc::new(RawUpdateTool)]).with_slots(slots);

    let result = router
        .execute(call("raw_updater"), CancellationToken::new())
        .await
        .unwrap();
    let update = rx.recv().await.unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "done");
    assert_eq!(update.call_id.as_str(), "call-raw_updater");
    assert_eq!(update.tool_name, "raw_updater");
    assert_eq!(update.sequence, 0);
    assert_eq!(update.kind, "raw.status");
    assert_eq!(update.payload, json!({ "message": "raw running" }));
}

#[tokio::test]
async fn local_tool_router_reports_tool_execution_policy() {
    let router = LocalToolRouter::new(vec![tool!(
        "echo",
        "Echo a message.",
        echo,
        policy = ToolExecutionPolicy::Parallel,
    )]);

    assert_eq!(
        router.execution_policy(&call("echo")).await.unwrap(),
        ToolExecutionPolicy::Parallel
    );
    assert_eq!(
        router.execution_policy(&call("missing")).await.unwrap(),
        ToolExecutionPolicy::Serial
    );
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EmptyInput {}

struct UpdateTool;

struct RawUpdateTool;

#[derive(Debug, Serialize)]
struct StatusUpdate {
    message: String,
}

impl ToolUpdatePayload for StatusUpdate {
    const KIND: &'static str = "status";
}

#[async_trait]
impl Tool for UpdateTool {
    type Input = EmptyInput;
    type Output = String;

    fn name(&self) -> &str {
        "updater"
    }

    fn description(&self) -> &str {
        "Emit a typed update."
    }

    async fn call(
        &self,
        _input: Self::Input,
        context: ToolContext,
    ) -> StdResult<Self::Output, String> {
        context
            .emit(StatusUpdate {
                message: "running".to_string(),
            })
            .await?;
        Ok("done".to_string())
    }
}

#[async_trait]
impl Tool for RawUpdateTool {
    type Input = EmptyInput;
    type Output = String;

    fn name(&self) -> &str {
        "raw_updater"
    }

    fn description(&self) -> &str {
        "Emit a raw update."
    }

    async fn call(
        &self,
        _input: Self::Input,
        context: ToolContext,
    ) -> StdResult<Self::Output, String> {
        context
            .emit(ToolUpdateParts::new(
                "raw.status",
                json!({ "message": "raw running" }),
            ))
            .await?;
        Ok("done".to_string())
    }
}

async fn echo(input: EchoInput, _context: ToolContext) -> StdResult<String, String> {
    Ok(format!("echo: {}", input.message))
}

#[tokio::test]
async fn composite_tool_router_merges_definitions_and_routes_calls() {
    let router = CompositeToolRouter::new(vec![
        Arc::new(StaticRouter::new("first", "first result")),
        Arc::new(StaticRouter::new("second", "second result")),
    ]);

    let definitions = router.definitions().await.unwrap();
    assert_eq!(definitions.len(), 2);
    assert_eq!(definitions[0].name, "first");
    assert_eq!(definitions[1].name, "second");

    let result = router
        .execute(call("second"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(result.name, "second");
    assert_eq!(tool_text(&result), "second result");
}

#[tokio::test]
async fn composite_tool_router_rejects_unknown_tools() {
    let router =
        CompositeToolRouter::new(vec![Arc::new(StaticRouter::new("known", "known result"))]);

    let result = router
        .execute(call("missing"), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Error);
    assert_eq!(result.name, "missing");
    assert_eq!(tool_text(&result), "unknown tool: missing");
}

#[tokio::test]
async fn composite_tool_router_delegates_execution_policy() {
    let router = CompositeToolRouter::new(vec![Arc::new(
        StaticRouter::new("parallel", "parallel result").with_policy(ToolExecutionPolicy::Parallel),
    )]);

    assert_eq!(
        router.execution_policy(&call("parallel")).await.unwrap(),
        ToolExecutionPolicy::Parallel
    );
    assert_eq!(
        router.execution_policy(&call("missing")).await.unwrap(),
        ToolExecutionPolicy::Serial
    );
}

struct StaticRouter {
    name: &'static str,
    output: &'static str,
    policy: ToolExecutionPolicy,
}

impl StaticRouter {
    fn new(name: &'static str, output: &'static str) -> Self {
        Self {
            name,
            output,
            policy: ToolExecutionPolicy::Serial,
        }
    }

    fn with_policy(mut self, policy: ToolExecutionPolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[async_trait]
impl ToolRouter for StaticRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        Ok(vec![ToolDefinition {
            name: self.name.to_string(),
            description: format!("{} test tool", self.name),
            parameters: json!({ "type": "object" }),
        }])
    }

    async fn execute(&self, call: ToolCall, _cancel: CancellationToken) -> Result<ToolResult> {
        Ok(ToolResult {
            call_id: call.id,
            name: call.name,
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text(self.output.to_string())],
        })
    }

    async fn execution_policy(&self, _call: &ToolCall) -> Result<ToolExecutionPolicy> {
        Ok(self.policy)
    }
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: format!("call-{name}").into(),
        name: name.to_string(),
        arguments: serde_json::Map::new(),
    }
}

fn tool_text(result: &ToolResult) -> &str {
    content_text(&result.content)
}

fn content_text(content: &[InputContent]) -> &str {
    let Some(InputContent::Text(text)) = content.first() else {
        panic!("expected text tool result");
    };
    text
}
