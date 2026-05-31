use async_std::io::WriteExt;
use copro_agent::{CancellationToken, ToolRouter};
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_harness::tools::{ErasedTool, LocalToolRouter, ToolSlots, ToolUpdate, ToolUpdateSlot};
use copro_workspace::tools::{CacheEntry, EditTool, FileCache, FileSnapshot};
use serde_json::json;
use std::sync::Arc;
use vfs::async_vfs::{AsyncMemoryFS, AsyncVfsPath};

#[tokio::test]
async fn edits_single_occurrence() {
    let root = memory_root_with("notes.txt", "hello\nworld\n").await;

    // Populate cache (simulate read-before-write)
    let cache: FileCache = Arc::default();
    cache
        .lock()
        .unwrap()
        .insert("notes.txt".to_string(), cache_entry(b"hello\nworld\n"));

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
    cache
        .lock()
        .unwrap()
        .insert("notes.txt".to_string(), cache_entry(b"foo bar foo\n"));

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
async fn emits_structured_match_updates() {
    let root = memory_root_with("notes.txt", "foo\nbar foo\n").await;
    let cache = cache_with("notes.txt", b"foo\nbar foo\n");

    let (result, updates) = execute_edit_with_updates(
        root,
        cache,
        json!({
            "path": "notes.txt",
            "old_string": "foo",
            "new_string": "baz",
            "replace_all": true
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].kind, "edit.match_found");
    assert_eq!(
        updates[0].payload,
        json!({
            "path": "notes.txt",
            "line_number": 1
        })
    );
    assert_eq!(updates[1].kind, "edit.match_found");
    assert_eq!(
        updates[1].payload,
        json!({
            "path": "notes.txt",
            "line_number": 2
        })
    );
}

#[tokio::test]
async fn with_empty_new_string_deletes_text() {
    let root = memory_root_with("notes.txt", "keep this remove\n").await;

    let cache: FileCache = Arc::default();
    cache
        .lock()
        .unwrap()
        .insert("notes.txt".to_string(), cache_entry(b"keep this remove\n"));

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
    cache
        .lock()
        .unwrap()
        .insert("notes.txt".to_string(), cache_entry(b"hello\n"));

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
async fn emits_no_updates_when_edit_fails_without_matches() {
    let root = memory_root_with("notes.txt", "hello\n").await;
    let cache = cache_with("notes.txt", b"hello\n");

    let (result, updates) = execute_edit_with_updates(
        root,
        cache,
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
    assert!(updates.is_empty());
}

#[tokio::test]
async fn rejects_multiple_matches_without_replace_all() {
    let root = memory_root_with("notes.txt", "foo bar foo baz\n").await;

    let cache: FileCache = Arc::default();
    cache
        .lock()
        .unwrap()
        .insert("notes.txt".to_string(), cache_entry(b"foo bar foo baz\n"));

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

#[tokio::test]
async fn emits_match_updates_before_rejecting_ambiguous_edit() {
    let root = memory_root_with("notes.txt", "foo bar foo baz\n").await;
    let cache = cache_with("notes.txt", b"foo bar foo baz\n");

    let (result, updates) = execute_edit_with_updates(
        root,
        cache,
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
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].kind, "edit.match_found");
    assert_eq!(
        updates[0].payload,
        json!({
            "path": "notes.txt",
            "line_number": 1
        })
    );
    assert_eq!(updates[1].kind, "edit.match_found");
    assert_eq!(
        updates[1].payload,
        json!({
            "path": "notes.txt",
            "line_number": 1
        })
    );
}

fn cache_with(path: &str, bytes: &[u8]) -> FileCache {
    let cache: FileCache = Arc::default();
    cache
        .lock()
        .unwrap()
        .insert(path.to_string(), cache_entry(bytes));
    cache
}

fn cache_entry(bytes: &[u8]) -> CacheEntry {
    CacheEntry {
        offset: None,
        limit: None,
        snapshot: FileSnapshot::from_bytes(bytes),
    }
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

async fn execute(router: &LocalToolRouter, args: serde_json::Value) -> ToolResult {
    let serde_json::Value::Object(arguments) = args else {
        panic!("tool args must be an object");
    };
    router
        .execute(
            ToolCall {
                id: "call-edit".into(),
                name: "edit".to_string(),
                arguments,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

async fn execute_edit_with_updates(
    root: AsyncVfsPath,
    cache: FileCache,
    args: serde_json::Value,
) -> (ToolResult, Vec<ToolUpdate>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let slots = ToolSlots::new().with(ToolUpdateSlot::new(move |update| {
        let tx = tx.clone();
        async move {
            tx.send(update).await.unwrap();
        }
    }));
    let tool: Arc<dyn ErasedTool> = Arc::new(EditTool::with_cache(root, cache));
    let router = LocalToolRouter::new(vec![tool]).with_slots(slots);
    let result = execute(&router, args).await;
    let mut updates = Vec::new();
    while let Ok(update) = rx.try_recv() {
        updates.push(update);
    }
    (result, updates)
}

fn tool_text(result: &ToolResult) -> &str {
    let Some(InputContent::Text(text)) = result.content.first() else {
        panic!("expected text output");
    };
    text
}
