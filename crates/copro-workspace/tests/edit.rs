use async_std::io::WriteExt;
use copro_agent::{CancellationToken, ToolRouter};
use copro_api::message::{InputContent, ToolCall, ToolResultStatus};
use copro_harness::tools::{ErasedTool, LocalToolRouter};
use copro_workspace::tools::{EditTool, FileCache};
use serde_json::json;
use std::sync::Arc;
use vfs::async_vfs::{AsyncMemoryFS, AsyncVfsPath};

#[tokio::test]
async fn edits_single_occurrence() {
    let root = memory_root_with("notes.txt", "hello\nworld\n").await;

    // Populate cache (simulate read-before-write)
    let cache: FileCache = Arc::default();
    cache.lock().unwrap().insert(
        "notes.txt".to_string(),
        copro_workspace::tools::CacheEntry {
            bytes: b"hello\nworld\n".to_vec(),
            offset: None,
            limit: None,
        },
    );

    let tool: Arc<dyn ErasedTool> = Arc::new(EditTool::with_cache(root, cache));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "old_string": "hello",
            "new_string": "hi",
            "replace_all": false
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(
        tool_text(&result).contains("1 replacement(s)") && tool_text(&result).contains(".txt:")
    );
}

#[tokio::test]
async fn replaces_all_occurrences() {
    let root = memory_root_with("notes.txt", "foo bar foo\n").await;

    let cache: FileCache = Arc::default();
    cache.lock().unwrap().insert(
        "notes.txt".to_string(),
        copro_workspace::tools::CacheEntry {
            bytes: b"foo bar foo\n".to_vec(),
            offset: None,
            limit: None,
        },
    );

    let tool: Arc<dyn ErasedTool> = Arc::new(EditTool::with_cache(root, cache));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "old_string": "foo",
            "new_string": "baz",
            "replace_all": true
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(
        tool_text(&result).contains("2 replacement(s)") && tool_text(&result).contains(".txt:")
    );
}

#[tokio::test]
async fn with_empty_new_string_deletes_text() {
    let root = memory_root_with("notes.txt", "keep this remove\n").await;

    let cache: FileCache = Arc::default();
    cache.lock().unwrap().insert(
        "notes.txt".to_string(),
        copro_workspace::tools::CacheEntry {
            bytes: b"keep this remove\n".to_vec(),
            offset: None,
            limit: None,
        },
    );

    let tool: Arc<dyn ErasedTool> = Arc::new(EditTool::with_cache(root, cache));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "old_string": "remove",
            "new_string": "",
            "replace_all": false
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(
        tool_text(&result).contains("1 replacement(s)") && tool_text(&result).contains(".txt:")
    );
}

#[tokio::test]
async fn rejects_edit_without_prior_read() {
    let root = memory_root_with("notes.txt", "hello\n").await;

    let tool: Arc<dyn ErasedTool> = Arc::new(EditTool::new(root));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "old_string": "hello",
            "new_string": "hi",
            "replace_all": false
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(tool_text(&result).contains("must be read"));
}

#[tokio::test]
async fn rejects_nonexistent_old_string() {
    let root = memory_root_with("notes.txt", "hello\n").await;

    let cache: FileCache = Arc::default();
    cache.lock().unwrap().insert(
        "notes.txt".to_string(),
        copro_workspace::tools::CacheEntry {
            bytes: b"hello\n".to_vec(),
            offset: None,
            limit: None,
        },
    );

    let tool: Arc<dyn ErasedTool> = Arc::new(EditTool::with_cache(root, cache));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "old_string": "missing",
            "new_string": "replacement",
            "replace_all": false
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(tool_text(&result).contains("not found"));
}

#[tokio::test]
async fn rejects_multiple_matches_without_replace_all() {
    let root = memory_root_with("notes.txt", "foo bar foo baz\n").await;

    let cache: FileCache = Arc::default();
    cache.lock().unwrap().insert(
        "notes.txt".to_string(),
        copro_workspace::tools::CacheEntry {
            bytes: b"foo bar foo baz\n".to_vec(),
            offset: None,
            limit: None,
        },
    );

    let tool: Arc<dyn ErasedTool> = Arc::new(EditTool::with_cache(root, cache));
    let router = LocalToolRouter::new(vec![tool]);

    let result = execute(
        &router,
        json!({
            "path": "notes.txt",
            "old_string": "foo",
            "new_string": "replaced",
            "replace_all": false
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(tool_text(&result).contains("include more surrounding context"));
}

async fn memory_root_with(path: &str, content: &str) -> AsyncVfsPath {
    let root: AsyncVfsPath = AsyncMemoryFS::new().into();
    root.join(path)
        .unwrap()
        .create_file()
        .await
        .unwrap()
        .write_all(content.as_bytes())
        .await
        .unwrap();
    root
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
                id: "call-edit".to_string(),
                name: "edit".to_string(),
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
