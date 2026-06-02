use coox_harness::tools::{ErasedTool, LocalToolRouter};
use coox_workspace::tools::BashTool;
use copro_agent::{CancellationToken, ToolRouter};
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

#[tokio::test]
async fn runs_command_in_workspace_directory() {
    let cwd = physical_dir("cwd");
    std::fs::write(cwd.join("note.txt"), "hello\n").unwrap();

    let result = execute_bash(
        cwd,
        json!({
            "command": "cat note.txt",
            "timeout": 5
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    let text = tool_text(&result);
    assert!(text.contains("exit code: 0"));
    assert!(text.contains("stdout:\nhello\n"));
}

#[tokio::test]
async fn captures_stderr_and_nonzero_exit_code() {
    let result = execute_bash(
        physical_dir("nonzero"),
        json!({
            "command": "printf out; printf err >&2; exit 7",
            "timeout": 5
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    let text = tool_text(&result);
    assert!(text.contains("exit code: 7"));
    assert!(text.contains("stdout:\nout"));
    assert!(text.contains("stderr:\nerr"));
}

#[tokio::test]
async fn rejects_empty_command() {
    let result = execute_bash(
        physical_dir("empty"),
        json!({
            "command": "   ",
            "timeout": 5
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(tool_text(&result).contains("command cannot be empty"));
}

#[tokio::test]
async fn times_out_long_running_command() {
    let result = execute_bash(
        physical_dir("timeout"),
        json!({
            "command": "sleep 2",
            "timeout": 1
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(tool_text(&result).contains("timed out after 1s"));
}

#[tokio::test]
async fn tail_truncates_large_stdout() {
    let result = execute_bash(
        physical_dir("tail"),
        json!({
            "command": "for i in $(seq 1 2105); do echo line-$i; done",
            "timeout": 5
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    let text = tool_text(&result);
    assert!(text.contains("stdout:\nline-106\n"));
    assert!(text.contains("line-2105\n"));
    assert!(!text.contains("line-105\n"));
    assert!(text.contains("[stdout truncated: showing tail"));
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_terminates_process_group_children() {
    let cwd = physical_dir("tree-timeout");
    let marker = cwd.join("child-term.txt");
    let result = execute_bash(
        cwd.clone(),
        json!({
            "command": "bash -c 'trap \"echo child_term > child-term.txt; exit 0\" TERM; while true; do sleep 1; done' & wait",
            "timeout": 1
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(tool_text(&result).contains("timed out after 1s"));

    for _ in 0..20 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(marker.exists(), "background child did not receive SIGTERM");
}

async fn execute_bash(cwd: PathBuf, args: serde_json::Value) -> ToolResult {
    let tool: Arc<dyn ErasedTool> = Arc::new(BashTool::new(cwd));
    let router = LocalToolRouter::new(vec![tool]);
    router
        .execute(call(args), CancellationToken::new())
        .await
        .unwrap()
}

fn physical_dir(prefix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("coox-workspace-bash-{prefix}-{unique}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn call(args: serde_json::Value) -> ToolCall {
    let serde_json::Value::Object(arguments) = args else {
        panic!("tool args must be an object");
    };

    ToolCall {
        id: "call-bash".into(),
        name: "bash".to_string(),
        arguments,
    }
}

fn tool_text(result: &ToolResult) -> &str {
    let Some(InputContent::Text(text)) = result.content.first() else {
        panic!("expected text output");
    };
    text
}
