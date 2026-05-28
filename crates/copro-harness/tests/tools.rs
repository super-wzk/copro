use copro_agent::ToolRouter;
use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_api::tool::ToolDefinition;
use copro_harness::tools::{CompositeToolRouter, ErasedTool, FnTool, LocalToolRouter, tool_fn};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn fn_tool_wraps_async_functions() {
    let router = LocalToolRouter::new(vec![tool_fn("echo", "Echo a message.", echo)]);

    let definitions = router.definitions().await.unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "echo");
    assert_eq!(definitions[0].description, "Echo a message.");

    let result = router
        .execute(ToolCall {
            id: "call-echo".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::Map::from_iter([("message".to_string(), json!("hello"))]),
        })
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
        .call_content(json!({ "message": "direct" }))
        .await
        .unwrap();

    assert_eq!(content_text(&content), "echo: direct");
}

#[tokio::test]
async fn fn_tool_wraps_async_closures() {
    let router = LocalToolRouter::new(vec![tool_fn(
        "length",
        "Return the message length.",
        |input: EchoInput| async move { Ok::<_, String>(input.message.len()) },
    )]);

    let result = router
        .execute(ToolCall {
            id: "call-length".to_string(),
            name: "length".to_string(),
            arguments: serde_json::Map::from_iter([("message".to_string(), json!("hello"))]),
        })
        .await
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "5");
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

async fn echo(input: EchoInput) -> std::result::Result<String, String> {
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

    let result = router.execute(call("second")).await.unwrap();
    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(result.name, "second");
    assert_eq!(tool_text(&result), "second result");
}

#[tokio::test]
async fn composite_tool_router_rejects_unknown_tools() {
    let router =
        CompositeToolRouter::new(vec![Arc::new(StaticRouter::new("known", "known result"))]);

    let result = router.execute(call("missing")).await.unwrap();

    assert_eq!(result.status, ToolResultStatus::Error);
    assert_eq!(result.name, "missing");
    assert_eq!(tool_text(&result), "unknown tool: missing");
}

struct StaticRouter {
    name: &'static str,
    output: &'static str,
}

impl StaticRouter {
    fn new(name: &'static str, output: &'static str) -> Self {
        Self { name, output }
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

    async fn execute(&self, call: ToolCall) -> Result<ToolResult> {
        Ok(ToolResult {
            call_id: call.id,
            name: call.name,
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text(self.output.to_string())],
        })
    }
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
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
