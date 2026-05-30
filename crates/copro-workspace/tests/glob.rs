use async_std::io::WriteExt;
use copro_agent::{CancellationToken, ToolRouter};
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_harness::tools::{ErasedTool, LocalToolRouter};
use copro_workspace::tools::GlobTool;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use vfs::async_vfs::{AsyncMemoryFS, AsyncPhysicalFS, AsyncVfsPath};

#[tokio::test]
async fn finds_files_by_glob_pattern() {
    let root = memory_root().await;
    write_file(&root, "src/main.rs", b"fn main() {}\n").await;
    write_file(&root, "src/lib.rs", b"pub fn lib() {}\n").await;
    write_file(&root, "README.md", b"hello\n").await;

    let result = execute_glob(root, json!({ "pattern": "**/*.rs" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        "src/lib.rs\nsrc/main.rs\n[sort: path order; modification time unavailable from VFS for matched files]\n2 files"
    );
}

#[tokio::test]
async fn path_limits_search_root() {
    let root = memory_root().await;
    write_file(&root, "src/app.ts", b"export {};\n").await;
    write_file(&root, "tests/app.ts", b"export {};\n").await;

    let result = execute_glob(
        root,
        json!({
            "pattern": "*.ts",
            "path": "src"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "src/app.ts\n1 files");
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

    let result = execute_glob(root, json!({ "pattern": "**/*" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        ".gitignore\nsrc/app.rs\n[sort: path order; modification time unavailable from VFS for matched files]\n2 files"
    );
}

#[tokio::test]
async fn include_ignored_finds_gitignored_files_but_still_skips_vcs_dirs() {
    let root = memory_root().await;
    write_file(&root, ".gitignore", b"ignored/\n*.log\n").await;
    write_file(&root, "src/app.rs", b"fn app() {}\n").await;
    write_file(&root, "ignored/app.rs", b"fn ignored() {}\n").await;
    write_file(&root, "debug.log", b"debug\n").await;
    write_file(&root, ".git/config", b"[core]\n").await;
    write_file(&root, ".svn/entries", b"svn\n").await;
    write_file(&root, ".hg/store/data", b"hg\n").await;

    let result = execute_glob(
        root,
        json!({
            "pattern": "**/*",
            "include_ignored": true
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        ".gitignore\ndebug.log\nignored/app.rs\nsrc/app.rs\n[sort: path order; modification time unavailable from VFS for matched files]\n4 files"
    );
}

#[tokio::test]
async fn sorts_matches_by_modified_time_descending() {
    let (root, temp_dir) = physical_root("glob-sort");
    write_file_at(&root, "older.txt", b"old\n", 1).await;
    write_file_at(&root, "newer.txt", b"new\n", 2).await;

    let result = execute_glob(root, json!({ "pattern": "*.txt" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "newer.txt\nolder.txt\n2 files");
    std::fs::remove_dir_all(temp_dir).ok();
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

fn physical_root(prefix: &str) -> (AsyncVfsPath, PathBuf) {
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("copro-workspace-{prefix}-{unique}"));
    std::fs::create_dir_all(&path).unwrap();
    (AsyncPhysicalFS::new(&path).into(), path)
}

async fn write_file_at(root: &AsyncVfsPath, path: &str, bytes: &[u8], modified_secs: u64) {
    let path = root.join(path).unwrap();
    path.parent().create_dir_all().await.unwrap();
    path.create_file()
        .await
        .unwrap()
        .write_all(bytes)
        .await
        .unwrap();
    path.set_modification_time(SystemTime::UNIX_EPOCH + Duration::from_secs(modified_secs))
        .await
        .unwrap();
}

async fn execute_glob(root: AsyncVfsPath, args: serde_json::Value) -> ToolResult {
    let tool: Arc<dyn ErasedTool> = Arc::new(GlobTool::new(root));
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
        id: "call-glob".into(),
        name: "glob".to_string(),
        arguments,
    }
}

fn tool_text(result: &ToolResult) -> &str {
    let Some(InputContent::Text(text)) = result.content.first() else {
        panic!("expected text output");
    };
    text
}
