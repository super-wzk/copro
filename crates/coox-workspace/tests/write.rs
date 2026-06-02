use async_std::io::{ReadExt, WriteExt};
use coox_harness::tools::{ErasedTool, LocalToolRouter};
use coox_workspace::tools::{CacheEntry, FileCache, FileSnapshot, WriteTool};
use copro_agent::{CancellationToken, ToolRouter};
use copro_api::message::{InputContent, ToolCall, ToolResultStatus};
use serde_json::json;
use std::sync::Arc;
use vfs::async_vfs::{AsyncMemoryFS, AsyncVfsPath};

#[tokio::test]
async fn creates_new_file_without_prior_read() {
    let root = memory_root().await;

    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::new(root.clone()));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "hello\nworld\n"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(tool_text(&result).contains("created"));
    assert_eq!(read_file(&root, "notes.txt").await, "hello\nworld\n");
}

#[tokio::test]
async fn overwrites_existing_file_after_prior_read() {
    let root = memory_root_with("notes.txt", "old\n").await;
    let cache = cache_with("notes.txt", b"old\n");

    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::with_cache(root.clone(), cache.clone()));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "new\n"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(tool_text(&result).contains("overwritten"));
    assert_eq!(read_file(&root, "notes.txt").await, "new\n");
    // Cache now holds the write entry (offset/limit = None)
    let entry = cache.lock().unwrap().get("notes.txt").cloned().unwrap();
    assert!(entry.offset.is_none());
    assert_eq!(entry.snapshot, FileSnapshot::from_bytes(b"new\n"));
}

#[tokio::test]
async fn allows_multiple_writes_to_new_file_in_sequence() {
    let root = memory_root().await;

    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::new(root.clone()));
    let router = LocalToolRouter::new(vec![tool]);

    let first = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "first\n"
        }),
    )
    .await;
    let second = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "second\n"
        }),
    )
    .await;

    assert_eq!(first.status, ToolResultStatus::Success);
    assert_eq!(second.status, ToolResultStatus::Success);
    assert_eq!(read_file(&root, "notes.txt").await, "second\n");
}

#[tokio::test]
async fn allows_multiple_overwrites_after_prior_read() {
    let root = memory_root_with("notes.txt", "old\n").await;
    let cache = cache_with("notes.txt", b"old\n");

    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::with_cache(root.clone(), cache));
    let router = LocalToolRouter::new(vec![tool]);

    let first = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "first\n"
        }),
    )
    .await;
    let second = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "second\n"
        }),
    )
    .await;

    assert_eq!(first.status, ToolResultStatus::Success);
    assert_eq!(second.status, ToolResultStatus::Success);
    assert_eq!(read_file(&root, "notes.txt").await, "second\n");
}

#[tokio::test]
async fn rejects_file_changed_after_previous_write() {
    let root = memory_root().await;

    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::new(root.clone()));
    let router = LocalToolRouter::new(vec![tool]);

    let first = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "first\n"
        }),
    )
    .await;
    write_file(&root, "notes.txt", b"external\n").await;
    let second = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "second\n"
        }),
    )
    .await;

    assert_eq!(first.status, ToolResultStatus::Success);
    assert_eq!(second.status, ToolResultStatus::Error);
    assert!(tool_text(&second).contains("changed since it was last read or written"));
    assert_eq!(read_file(&root, "notes.txt").await, "external\n");
}

#[tokio::test]
async fn rejects_existing_file_without_prior_read() {
    let root = memory_root_with("notes.txt", "old\n").await;

    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::new(root));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "new\n"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(tool_text(&result).contains("must be read"));
}

#[tokio::test]
async fn rejects_file_changed_after_read() {
    let root = memory_root_with("notes.txt", "old\n").await;
    let cache = cache_with("notes.txt", b"old\n");
    write_file(&root, "notes.txt", b"external\n").await;

    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::with_cache(root.clone(), cache));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "content": "new\n"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(tool_text(&result).contains("changed since it was last read"));
    assert_eq!(read_file(&root, "notes.txt").await, "external\n");
}

#[tokio::test]
async fn rejects_directories() {
    let root = memory_root().await;
    root.join("src").unwrap().create_dir().await.unwrap();

    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::new(root));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "src",
            "content": "not a file\n"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert_eq!(tool_text(&result), "cannot write directory: src");
}

#[tokio::test]
async fn creates_nested_file_with_create_dirs() {
    let root = memory_root().await;
    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::new(root.clone()));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "sub/dir/notes.txt",
            "content": "nested\n",
            "create_dirs": true
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(tool_text(&result).contains("created"));
    assert_eq!(read_file(&root, "sub/dir/notes.txt").await, "nested\n");
}

#[tokio::test]
async fn rejects_nested_file_without_create_dirs() {
    let root = memory_root().await;
    let tool: Arc<dyn ErasedTool> = Arc::new(WriteTool::new(root));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "sub/dir/notes.txt",
            "content": "nested\n"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
}

async fn memory_root() -> AsyncVfsPath {
    AsyncMemoryFS::new().into()
}

async fn memory_root_with(path: &str, content: &str) -> AsyncVfsPath {
    let root = memory_root().await;
    write_file(&root, path, content.as_bytes()).await;
    root
}

fn cache_with(path: &str, bytes: &[u8]) -> FileCache {
    let cache: FileCache = Arc::default();
    cache.lock().unwrap().insert(
        path.to_string(),
        CacheEntry {
            offset: None,
            limit: None,
            snapshot: FileSnapshot::from_bytes(bytes),
        },
    );
    cache
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

async fn read_file(root: &AsyncVfsPath, path: &str) -> String {
    let mut file = root.join(path).unwrap().open_file().await.unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).await.unwrap();
    text
}

async fn execute(
    router: &LocalToolRouter,
    args: serde_json::Value,
) -> copro_api::message::ToolResult {
    let serde_json::Value::Object(arguments) = args else {
        panic!("tool args must be an object");
    };
    router
        .execute(
            ToolCall {
                id: "call-write".into(),
                name: "write".to_string(),
                arguments,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

fn tool_text(result: &copro_api::message::ToolResult) -> &str {
    let Some(InputContent::Text(text)) = result.content.first() else {
        panic!("expected text output");
    };
    text
}
