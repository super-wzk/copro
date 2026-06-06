use std::io;
use std::path::{Component, Path};
use std::sync::Arc;

use coox_cli::agent::config::{RuntimeConfig, build_model};
use coox_cli::agent::runtime::AgentRuntime;
use coox_cli::tui::app::{App, run};
use coox_cli::tui::terminal::Tui;
use coox_workspace::WorkspaceToolRouter;
use copro_agent::{AgentHistory, AgentTurnConfig};
use copro_api::message::{InputContent, Message};
use vfs::async_vfs::{AsyncPhysicalFS, AsyncVfsPath};

#[tokio::main]
async fn main() -> io::Result<()> {
    let runtime_config = RuntimeConfig::from_env();
    let model =
        build_model(&runtime_config).map_err(|error| io::Error::other(error.to_string()))?;

    let current_dir = std::env::current_dir()?;
    let workspace = workspace_name(&current_dir);
    let fs_root_path = filesystem_root(&current_dir);
    let fs_root = AsyncPhysicalFS::new(fs_root_path).into();
    let workspace_root = workspace_vfs_root(&fs_root, &current_dir)?;
    let tools = WorkspaceToolRouter::new(workspace_root);
    let workspace_context = tools.workspace_context();
    let history = AgentHistory::from_messages(vec![Message::developer(vec![InputContent::Text(
        format!(
            "Current workspace: {}\nFilesystem root: {}\nRelative tool paths resolve from the current workspace. Absolute tool paths resolve from the filesystem root.",
            workspace_context.current_workspace, workspace_context.filesystem_root,
        ),
    )])]);
    let runtime = AgentRuntime::new_with_history(
        AgentTurnConfig::default(),
        model,
        Arc::new(tools),
        history.clone(),
    );

    let mut tui = Tui::new()?;
    let mut app = App::new_with_runtime_config(
        runtime,
        workspace,
        runtime_config,
        tui.image_renderer.clone(),
    );

    run(&mut tui, &mut app).await
}

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string())
}

fn filesystem_root(path: &Path) -> &Path {
    path.ancestors().last().unwrap_or(path)
}

fn workspace_vfs_root(fs_root: &AsyncVfsPath, current_dir: &Path) -> io::Result<AsyncVfsPath> {
    fs_root
        .join(path_to_vfs(current_dir))
        .map_err(|error| io::Error::other(error.to_string()))
}

fn path_to_vfs(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_string()),
            Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_name_uses_last_path_component() {
        assert_eq!(workspace_name(Path::new("/tmp/copro")), "copro");
    }

    #[test]
    fn workspace_name_falls_back_for_root_path() {
        assert_eq!(workspace_name(Path::new("/")), ".");
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_root_uses_system_root() {
        assert_eq!(filesystem_root(Path::new("/tmp/copro")), Path::new("/"));
    }

    #[test]
    fn workspace_vfs_root_keeps_relative_and_absolute_tool_paths_working() {
        let fs_root: AsyncVfsPath = vfs::async_vfs::AsyncMemoryFS::new().into();

        let workspace_root =
            workspace_vfs_root(&fs_root, Path::new("/tmp/copro")).expect("workspace root");

        assert_eq!(workspace_root.as_str(), "/tmp/copro");
        assert_eq!(
            workspace_root.join("src/main.rs").unwrap().as_str(),
            "/tmp/copro/src/main.rs"
        );
        assert_eq!(
            workspace_root.join("/Users/wzk/file.txt").unwrap().as_str(),
            "/Users/wzk/file.txt"
        );
    }
}
