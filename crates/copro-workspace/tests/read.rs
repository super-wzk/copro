use async_std::io::WriteExt;
use copro_agent::{CancellationToken, ToolRouter};
use copro_api::message::{ImageContent, InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_harness::tools::{ErasedTool, LocalToolRouter};
use copro_workspace::tools::{ReadTool, ReadToolConfig};
use serde_json::json;
use std::sync::Arc;
use vfs::async_vfs::{AsyncMemoryFS, AsyncVfsPath};

#[tokio::test]
async fn reads_text_file_with_line_numbers() {
    let root = memory_root().await;
    write_file(&root, "hello.txt", b"hello\nworld\n").await;

    let result = execute_read(root, json!({ "path": "hello.txt" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "hello.txt\n1\thello\n2\tworld");
}

#[tokio::test]
async fn reads_text_file_with_offset_and_limit() {
    let root = memory_root().await;
    write_file(&root, "hello.txt", b"one\ntwo\nthree\nfour\n").await;

    let result = execute_read(
        root,
        json!({ "path": "hello.txt", "offset": 2, "limit": 2 }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        "hello.txt\n2\ttwo\n3\tthree\n[truncated: reached line limit; continue with offset=4]"
    );
}

#[tokio::test]
async fn truncates_after_default_line_limit() {
    let root = memory_root().await;
    let mut text = String::new();
    for index in 1..=2001 {
        text.push_str(&format!("line {index}\n"));
    }
    write_file(&root, "large.txt", text.as_bytes()).await;

    let result = execute_read(root, json!({ "path": "large.txt" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    let text = tool_text(&result);
    assert!(text.starts_with("large.txt\n"));
    assert!(text.contains("   1\tline 1"));
    assert!(text.contains("2000\tline 2000"));
    assert!(text.ends_with("[truncated: reached line limit; continue with offset=2001]"));
}

#[tokio::test]
async fn respects_configured_line_limits() {
    let root = memory_root().await;
    write_file(&root, "hello.txt", b"a\nb\nc\n").await;

    let tool: Arc<dyn ErasedTool> = Arc::new(ReadTool::with_config(
        root,
        ReadToolConfig {
            max_line_limit: 1,
            line_numbers: true,
        },
    ));
    let router = LocalToolRouter::new(vec![tool]);

    let result = router
        .execute(
            call(json!({ "path": "hello.txt", "limit": 3 })),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        "hello.txt\n1\ta\n[truncated: reached line limit; continue with offset=2]"
    );
}

#[tokio::test]
async fn reads_image_file() {
    let root = memory_root().await;
    write_file(&root, "image.png", &[137, 80, 78, 71]).await;

    let result = execute_read(root, json!({ "path": "image.png" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    let Some(InputContent::Image(ImageContent::Data { mime_type, data })) = result.content.first()
    else {
        panic!("expected image data");
    };
    assert_eq!(mime_type, "image/png");
    assert_eq!(data, &vec![137, 80, 78, 71]);
}

#[tokio::test]
async fn rejects_non_utf8_text_file() {
    let root = memory_root().await;
    write_file(&root, "data.bin", &[0xff, 0xfe, 0xfd]).await;

    let result = execute_read(root, json!({ "path": "data.bin" })).await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert_eq!(tool_text(&result), "data.bin is not valid UTF-8");
}

#[tokio::test]
async fn rejects_directories() {
    let root = memory_root().await;
    root.join("src").unwrap().create_dir().await.unwrap();

    let result = execute_read(root, json!({ "path": "src" })).await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert_eq!(tool_text(&result), "cannot read directory: src");
}

#[tokio::test]
async fn rejects_paths_that_escape_root() {
    let root = memory_root().await;

    let result = execute_read(root, json!({ "path": "../secret.txt" })).await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(tool_text(&result).contains("Could not get metadata"));
}

#[tokio::test]
async fn dedups_unchanged_file_content() {
    let root = memory_root().await;
    write_file(&root, "notes.txt", b"hello\nworld\n").await;

    let tool: Arc<dyn ErasedTool> = Arc::new(ReadTool::new(root));
    let router = LocalToolRouter::new(vec![tool]);

    // First read: full content
    let first = router
        .execute(
            call(json!({ "path": "notes.txt" })),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first.status, ToolResultStatus::Success);
    assert!(tool_text(&first).contains("hello"));
    assert!(tool_text(&first).contains("world"));

    // Second read: same content → placeholder
    let second = router
        .execute(
            call(json!({ "path": "notes.txt" })),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(second.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&second), "notes.txt — unchanged since last read");
}

async fn memory_root() -> AsyncVfsPath {
    AsyncMemoryFS::new().into()
}

async fn write_file(root: &AsyncVfsPath, path: &str, bytes: &[u8]) {
    root.join(path)
        .unwrap()
        .create_file()
        .await
        .unwrap()
        .write_all(bytes)
        .await
        .unwrap();
}

async fn execute_read(root: AsyncVfsPath, args: serde_json::Value) -> ToolResult {
    let tool: Arc<dyn ErasedTool> = Arc::new(ReadTool::new(root));
    let router = LocalToolRouter::new(vec![tool]);
    router
        .execute(call(args), CancellationToken::new())
        .await
        .unwrap()
}

fn call(args: serde_json::Value) -> ToolCall {
    let serde_json::Value::Object(arguments) = args else {
        panic!("tool args must be an object");
    };

    ToolCall {
        id: "call-read".to_string(),
        name: "read".to_string(),
        arguments,
    }
}

fn tool_text(result: &ToolResult) -> &str {
    let Some(InputContent::Text(text)) = result.content.first() else {
        panic!("expected text output");
    };
    text
}
