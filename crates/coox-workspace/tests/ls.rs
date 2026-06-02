use async_std::io::WriteExt;
use coox_harness::tools::{ErasedTool, LocalToolRouter};
use coox_workspace::tools::LsTool;
use copro_agent::{CancellationToken, ToolRouter};
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use serde_json::json;
use std::sync::Arc;
use vfs::async_vfs::{AsyncMemoryFS, AsyncVfsPath};

#[tokio::test]
async fn lists_immediate_directory_entries() {
    let root = memory_root().await;
    write_file(&root, "src/main.rs", b"fn main() {}\n").await;
    write_file(&root, "README.md", b"hello\n").await;

    let result = execute_ls(root, json!({})).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "src/\nREADME.md\n2 entries");
}

#[tokio::test]
async fn path_limits_listed_directory() {
    let root = memory_root().await;
    write_file(&root, "src/app.ts", b"export {};\n").await;
    write_file(&root, "src/nested/mod.ts", b"export {};\n").await;
    write_file(&root, "tests/app.ts", b"export {};\n").await;

    let result = execute_ls(root, json!({ "path": "src" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "src/nested/\nsrc/app.ts\n2 entries");
}

#[tokio::test]
async fn file_path_outputs_file() {
    let root = memory_root().await;
    write_file(&root, "src/lib.rs", b"pub fn lib() {}\n").await;

    let result = execute_ls(root, json!({ "path": "src/lib.rs" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "src/lib.rs\n1 entries");
}

#[tokio::test]
async fn cwd_inside_larger_vfs_keeps_absolute_entry_paths() {
    let fs_root = memory_root().await;
    write_file(&fs_root, "workspace/src/main.rs", b"fn main() {}\n").await;
    write_file(&fs_root, "workspace/README.md", b"hello\n").await;
    let workspace_cwd = fs_root.join("/workspace").unwrap();

    let result = execute_ls(workspace_cwd, json!({})).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        "/workspace/src/\n/workspace/README.md\n2 entries"
    );
}

#[tokio::test]
async fn respects_gitignore_and_skips_vcs_dirs() {
    let root = memory_root().await;
    write_file(&root, ".gitignore", b"ignored/\n*.log\n").await;
    write_file(&root, "src/app.rs", b"fn app() {}\n").await;
    write_file(&root, "ignored/app.rs", b"fn ignored() {}\n").await;
    write_file(&root, "debug.log", b"debug\n").await;
    write_file(&root, ".git/config", b"[core]\n").await;
    write_file(&root, ".svn/entries", b"svn\n").await;
    write_file(&root, ".hg/store/data", b"hg\n").await;

    let result = execute_ls(root, json!({})).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "src/\n.gitignore\n2 entries");
}

#[tokio::test]
async fn supports_offset_and_head_limit() {
    let root = memory_root().await;
    write_file(&root, "a.txt", b"a\n").await;
    write_file(&root, "b.txt", b"b\n").await;
    write_file(&root, "c.txt", b"c\n").await;

    let result = execute_ls(root, json!({ "head_limit": 1, "offset": 1 })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        "b.txt\n1 of 3 entries (truncated, continue with offset=2)"
    );
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

async fn execute_ls(root: AsyncVfsPath, args: serde_json::Value) -> ToolResult {
    let tool: Arc<dyn ErasedTool> = Arc::new(LsTool::new(root));
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
        id: "call-ls".into(),
        name: "ls".to_string(),
        arguments,
    }
}

fn tool_text(result: &ToolResult) -> &str {
    let Some(InputContent::Text(text)) = result.content.first() else {
        panic!("expected text output");
    };
    text
}
