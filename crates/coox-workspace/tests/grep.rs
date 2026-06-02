use async_std::io::WriteExt;
use coox_harness::tools::{ErasedTool, LocalToolRouter, ToolSlots, ToolUpdate, ToolUpdateSlot};
use coox_workspace::tools::GrepTool;
use copro_agent::{CancellationToken, ToolRouter};
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use vfs::async_vfs::{AsyncMemoryFS, AsyncPhysicalFS, AsyncVfsPath};

#[tokio::test]
async fn defaults_to_files_with_matches() {
    let root = memory_root().await;
    write_file(&root, "src/main.rs", b"fn main() { println!(\"hi\"); }\n").await;
    write_file(&root, "README.md", b"hello\n").await;

    let result = execute_grep(root, json!({ "pattern": "println" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "src/main.rs");
}

#[tokio::test]
async fn cwd_inside_larger_vfs_keeps_absolute_match_paths() {
    let fs_root = memory_root().await;
    write_file(
        &fs_root,
        "workspace/src/main.rs",
        b"fn main() { println!(\"hi\"); }\n",
    )
    .await;
    write_file(&fs_root, "workspace/README.md", b"hello\n").await;
    let workspace_cwd = fs_root.join("/workspace").unwrap();

    let result = execute_grep(
        workspace_cwd,
        json!({
            "pattern": "println",
            "glob": "*.rs"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "/workspace/src/main.rs");
}

#[tokio::test]
async fn content_mode_supports_line_numbers_and_case_insensitive_search() {
    let root = memory_root().await;
    write_file(&root, "notes.txt", b"Alpha\nbeta\nALPINE\n").await;

    let result = execute_grep(
        root,
        json!({
            "pattern": "alp",
            "output_mode": "content",
            "-i": true
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "notes.txt:1:Alpha\nnotes.txt:3:ALPINE");
}

#[tokio::test]
async fn filters_by_glob() {
    let root = memory_root().await;
    write_file(&root, "src/app.ts", b"const needle = 1;\n").await;
    write_file(&root, "src/app.py", b"needle = 1\n").await;

    let result = execute_grep(
        root,
        json!({
            "pattern": "needle",
            "glob": "*.ts"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "src/app.ts");
}

#[tokio::test]
async fn filters_by_type() {
    let root = memory_root().await;
    write_file(&root, "src/app.js", b"const needle = 1;\n").await;
    write_file(&root, "src/app.py", b"needle = 1\n").await;

    let result = execute_grep(
        root,
        json!({
            "pattern": "needle",
            "type": "js"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "src/app.js");
}

#[tokio::test]
async fn respects_root_gitignore_rules() {
    let root = memory_root().await;
    write_file(&root, ".gitignore", b"ignored.txt\ntarget/\n").await;
    write_file(&root, "visible.txt", b"needle\n").await;
    write_file(&root, "ignored.txt", b"needle\n").await;
    write_file(&root, "target/output.txt", b"needle\n").await;

    let result = execute_grep(root, json!({ "pattern": "needle" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "visible.txt");
}

#[tokio::test]
async fn respects_nested_gitignore_rules() {
    let root = memory_root().await;
    write_file(&root, "src/.gitignore", b"generated/\n").await;
    write_file(&root, "src/lib.rs", b"let needle = true;\n").await;
    write_file(&root, "src/generated/lib.rs", b"let needle = true;\n").await;

    let result = execute_grep(root, json!({ "pattern": "needle" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "src/lib.rs");
}

#[tokio::test]
async fn include_ignored_searches_gitignored_files() {
    let root = memory_root().await;
    write_file(&root, ".gitignore", b"ignored.txt\ntarget/\n").await;
    write_file(&root, "visible.txt", b"needle\n").await;
    write_file(&root, "ignored.txt", b"needle\n").await;
    write_file(&root, "target/output.txt", b"needle\n").await;

    let result = execute_grep(
        root,
        json!({
            "pattern": "needle",
            "include_ignored": true
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        "ignored.txt\ntarget/output.txt\nvisible.txt\n[sort: path order; modification time unavailable from VFS for matched files]"
    );
}

#[tokio::test]
async fn excludes_vcs_directories_by_default() {
    let root = memory_root().await;
    write_file(&root, ".git/config", b"needle\n").await;
    write_file(&root, ".git/objects/pack/data", b"needle\n").await;
    write_file(&root, ".svn/entries", b"needle\n").await;
    write_file(&root, ".hg/store/data", b"needle\n").await;
    write_file(&root, "visible.txt", b"needle\n").await;

    let result = execute_grep(root, json!({ "pattern": "needle" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "visible.txt");
}

#[tokio::test]
async fn count_mode_counts_matching_lines_per_file() {
    let root = memory_root().await;
    write_file(&root, "a.txt", b"needle\nneedle again\nnope\n").await;
    write_file(&root, "b.txt", b"needle\n").await;

    let result = execute_grep(
        root,
        json!({
            "pattern": "needle",
            "output_mode": "count"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        "a.txt:2\nb.txt:1\n[sort: path order; modification time unavailable from VFS for matched files]"
    );
}

#[tokio::test]
async fn sorts_results_by_modified_time_descending() {
    let (root, temp_dir) = physical_root("grep-sort");
    write_file_at(&root, "older.txt", b"needle\n", 1).await;
    write_file_at(&root, "newer.txt", b"needle\n", 2).await;

    let result = execute_grep(root, json!({ "pattern": "needle" })).await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "newer.txt\nolder.txt");
    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn context_and_head_limit_are_applied_to_content_output() {
    let root = memory_root().await;
    write_file(&root, "notes.txt", b"before\nneedle\nafter\nneedle two\n").await;

    let result = execute_grep(
        root,
        json!({
            "pattern": "needle",
            "output_mode": "content",
            "context": 1,
            "head_limit": 3
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        "notes.txt-1-before\nnotes.txt:2:needle\nnotes.txt-3-after\n[truncated: reached head_limit; continue with offset=3]"
    );
}

#[tokio::test]
async fn multiline_search_crosses_line_boundaries() {
    let root = memory_root().await;
    write_file(&root, "notes.txt", b"begin\nmiddle\nend\n").await;

    let result = execute_grep(
        root,
        json!({
            "pattern": "begin.*end",
            "output_mode": "content",
            "multiline": true
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        tool_text(&result),
        "notes.txt:1:begin\nnotes.txt:2:middle\nnotes.txt:3:end"
    );
}

#[tokio::test]
async fn emits_structured_progress_and_match_updates() {
    let root = memory_root().await;
    write_file(&root, "notes.txt", b"first\nneedle here\nlast\n").await;

    let (result, updates) = execute_grep_with_updates(
        root,
        json!({
            "pattern": "needle",
            "output_mode": "content"
        }),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(tool_text(&result), "notes.txt:2:needle here");
    assert_eq!(updates.len(), 3);
    assert_eq!(updates[0].kind, "grep.progress");
    assert_eq!(
        updates[0].payload,
        json!({
            "searched_files": 1,
            "matched_files": 0,
            "current_path": "notes.txt",
        })
    );
    assert_eq!(updates[1].kind, "grep.match_found");
    assert_eq!(
        updates[1].payload,
        json!({
            "path": "notes.txt",
            "line_number": 2,
            "byte_offset": 6,
            "line_count": 1,
        })
    );
    assert_eq!(updates[2].kind, "grep.progress");
    assert_eq!(
        updates[2].payload,
        json!({
            "searched_files": 1,
            "matched_files": 1,
            "current_path": null,
        })
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

fn physical_root(prefix: &str) -> (AsyncVfsPath, PathBuf) {
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("coox-workspace-{prefix}-{unique}"));
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

async fn execute_grep(root: AsyncVfsPath, args: serde_json::Value) -> ToolResult {
    let tool: Arc<dyn ErasedTool> = Arc::new(GrepTool::new(root));
    let router = LocalToolRouter::new(vec![tool]);
    router
        .execute(call(args), CancellationToken::new())
        .await
        .unwrap()
}

async fn execute_grep_with_updates(
    root: AsyncVfsPath,
    args: serde_json::Value,
) -> (ToolResult, Vec<ToolUpdate>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let slots = ToolSlots::new().with(ToolUpdateSlot::new(move |update| {
        let tx = tx.clone();
        async move {
            tx.send(update).await.unwrap();
        }
    }));
    let tool: Arc<dyn ErasedTool> = Arc::new(GrepTool::new(root));
    let router = LocalToolRouter::new(vec![tool]).with_slots(slots);
    let result = router
        .execute(call(args), CancellationToken::new())
        .await
        .unwrap();
    let mut updates = Vec::new();
    while let Ok(update) = rx.try_recv() {
        updates.push(update);
    }
    (result, updates)
}

fn call(args: serde_json::Value) -> ToolCall {
    let serde_json::Value::Object(arguments) = args else {
        panic!("tool args must be an object");
    };

    ToolCall {
        id: "call-grep".into(),
        name: "grep".to_string(),
        arguments,
    }
}

fn tool_text(result: &ToolResult) -> &str {
    let Some(InputContent::Text(text)) = result.content.first() else {
        panic!("expected text output");
    };
    text
}
