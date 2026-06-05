use async_std::io::WriteExt;
use coox_workspace::WorkspaceToolRouter;
use copro_agent::{CancellationToken, ToolExecutionPolicy, ToolRouter};
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use serde_json::json;
use vfs::async_vfs::{AsyncMemoryFS, AsyncVfsPath};

#[tokio::test]
async fn exposes_standard_workspace_tools() {
    let router = WorkspaceToolRouter::new(memory_root().await);

    assert_eq!(router.root().as_str(), "");

    let definitions = router.definitions().await.unwrap();
    let names: Vec<_> = definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();

    assert_eq!(
        names,
        ["read", "write", "edit", "grep", "glob", "ls", "bash"]
    );
}

#[tokio::test]
async fn read_only_router_exposes_only_read_only_tools() {
    let router = WorkspaceToolRouter::read_only(memory_root().await);

    let definitions = router.definitions().await.unwrap();
    let names: Vec<_> = definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();

    assert_eq!(names, ["read", "grep", "glob", "ls"]);
}

#[tokio::test]
async fn read_only_router_does_not_execute_write_edit_or_bash() {
    let router = WorkspaceToolRouter::read_only(memory_root().await);

    for name in ["write", "edit", "bash"] {
        let result = router
            .execute(call(name, json!({})), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(tool_text(&result).contains("unknown tool"));
    }
}

#[tokio::test]
async fn router_root_can_be_workspace_cwd_inside_larger_filesystem() {
    let fs_root = memory_root().await;
    write_file(&fs_root, "workspace/src/main.rs", b"workspace\n").await;
    write_file(&fs_root, "outside.txt", b"outside\n").await;
    let workspace_cwd = fs_root.join("/workspace").unwrap();
    let router = WorkspaceToolRouter::read_only(workspace_cwd);

    let relative = router
        .execute(
            call("read", json!({ "path": "src/main.rs" })),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let absolute = router
        .execute(
            call("read", json!({ "path": "/outside.txt" })),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(relative.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&relative), "src/main.rs\n1: workspace");
    assert_eq!(absolute.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&absolute), "/outside.txt\n1: outside");
}

#[tokio::test]
async fn workspace_context_reports_current_workspace_inside_larger_filesystem() {
    let fs_root = memory_root().await;
    let workspace_cwd = fs_root.join("/workspace").unwrap();
    let router = WorkspaceToolRouter::read_only(workspace_cwd);

    let context = router.workspace_context();

    assert_eq!(context.current_workspace, "/workspace");
    assert_eq!(context.filesystem_root, "/");
}

#[tokio::test]
async fn shares_cache_between_read_write_and_edit_tools() {
    let root = memory_root().await;
    write_file(&root, "note.txt", b"hello\n").await;
    let router = WorkspaceToolRouter::new(root);

    let read_result = router
        .execute(
            call("read", json!({ "path": "note.txt" })),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(read_result.status, ToolResultStatus::Success);

    let edit_result = router
        .execute(
            call(
                "edit",
                json!({
                    "path": "note.txt",
                    "old_string": "hello",
                    "new_string": "hi"
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(edit_result.status, ToolResultStatus::Success);

    let write_without_reread = router
        .execute(
            call(
                "write",
                json!({
                    "path": "note.txt",
                    "content": "bye\n"
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(write_without_reread.status, ToolResultStatus::Error);
    assert!(tool_text(&write_without_reread).contains("must be read"));

    let read_again = router
        .execute(
            call("read", json!({ "path": "note.txt" })),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(read_again.status, ToolResultStatus::Success);

    let write_after_read = router
        .execute(
            call(
                "write",
                json!({
                    "path": "note.txt",
                    "content": "bye\n"
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(write_after_read.status, ToolResultStatus::Success);
}

#[tokio::test]
async fn reports_tool_execution_policies() {
    let router = WorkspaceToolRouter::new(memory_root().await);

    assert_eq!(
        router
            .execution_policy(&call("read", json!({})))
            .await
            .unwrap(),
        ToolExecutionPolicy::Parallel
    );
    assert_eq!(
        router
            .execution_policy(&call("write", json!({})))
            .await
            .unwrap(),
        ToolExecutionPolicy::Serial
    );
    assert_eq!(
        router
            .execution_policy(&call("bash", json!({})))
            .await
            .unwrap(),
        ToolExecutionPolicy::Serial
    );
}

#[tokio::test]
async fn clear_cache_resets_write_safety_state() {
    let root = memory_root().await;
    write_file(&root, "note.txt", b"hello\n").await;
    let router = WorkspaceToolRouter::new(root);

    let read_result = router
        .execute(
            call("read", json!({ "path": "note.txt" })),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(read_result.status, ToolResultStatus::Success);

    router.clear_cache();

    let write_result = router
        .execute(
            call(
                "write",
                json!({
                    "path": "note.txt",
                    "content": "bye\n"
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(write_result.status, ToolResultStatus::Error);
    assert!(tool_text(&write_result).contains("must be read"));
}

async fn memory_root() -> AsyncVfsPath {
    AsyncMemoryFS::new().into()
}

async fn write_file(root: &AsyncVfsPath, path: &str, bytes: &[u8]) {
    let path = root.join(path).unwrap();
    path.parent().create_dir_all().await.unwrap();
    path.create_file()
        .await
        .unwrap()
        .write_all(bytes)
        .await
        .unwrap();
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    let serde_json::Value::Object(arguments) = args else {
        panic!("tool args must be an object");
    };

    ToolCall {
        id: format!("call-{name}").into(),
        name: name.to_string(),
        arguments,
    }
}

fn tool_text(result: &ToolResult) -> &str {
    let Some(InputContent::Text(text)) = result.content.first() else {
        panic!("expected text output");
    };
    text
}
