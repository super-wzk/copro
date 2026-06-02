use crate::tools::{
    BashTool, EditTool, FileCache, GlobTool, GrepTool, LsTool, ReadTool, ReadToolConfig, WriteTool,
};
use coox_harness::tools::{ErasedTool, LocalToolRouter, ToolSlots};
use copro_agent::{CancellationToken, ToolExecutionPolicy, ToolRouter};
use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{ToolCall, ToolResult};
use copro_api::tool::ToolDefinition;
use std::path::PathBuf;
use std::sync::Arc;
use vfs::async_vfs::AsyncVfsPath;

/// Tool router for workspace tools rooted at a VFS path.
///
/// The root may be the filesystem root itself or a workspace cwd inside a
/// larger VFS. Tool-relative paths resolve against this root, while absolute
/// paths resolve from the same VFS root.
///
/// Use [`WorkspaceToolRouter::new`] for the standard workspace tool set, or
/// [`WorkspaceToolRouter::read_only`] for tools that cannot write files or run
/// shell commands.
#[derive(Clone)]
pub struct WorkspaceToolRouter {
    inner: LocalToolRouter,
    root: AsyncVfsPath,
    cache: FileCache,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceContext {
    pub current_workspace: String,
    pub filesystem_root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceToolSet {
    Standard,
    ReadOnly,
}

impl WorkspaceToolRouter {
    /// Construct a router that exposes the standard workspace tools.
    ///
    /// The router wires `read`, `write`, and `edit` together with a shared file
    /// snapshot cache so write/edit safety checks see files read through the
    /// same router. It also exposes the read-only discovery tools: `ls`,
    /// `glob`, and `grep`, plus a process-local `bash` tool.
    pub fn new(root: AsyncVfsPath) -> Self {
        Self::build(root, WorkspaceToolSet::Standard)
    }

    /// Construct a router that exposes only read-only workspace tools.
    ///
    /// The router exposes `read`, `grep`, `glob`, and `ls`. It omits `write`,
    /// `edit`, and `bash`, so callers cannot write files or run shell commands
    /// through this router.
    pub fn read_only(root: AsyncVfsPath) -> Self {
        Self::build(root, WorkspaceToolSet::ReadOnly)
    }

    fn build(root: AsyncVfsPath, tool_set: WorkspaceToolSet) -> Self {
        let cache = FileCache::default();
        let mut tools: Vec<Arc<dyn ErasedTool>> = vec![
            Arc::new(ReadTool::with_cache(
                root.clone(),
                ReadToolConfig::default(),
                Arc::clone(&cache),
            )),
            Arc::new(GrepTool::new(root.clone())),
            Arc::new(GlobTool::new(root.clone())),
            Arc::new(LsTool::new(root.clone())),
        ];

        if tool_set == WorkspaceToolSet::Standard {
            tools.insert(
                1,
                Arc::new(WriteTool::with_cache(root.clone(), Arc::clone(&cache))),
            );
            tools.insert(
                2,
                Arc::new(EditTool::with_cache(root.clone(), Arc::clone(&cache))),
            );
            tools.push(Arc::new(BashTool::new(bash_working_directory())));
        }

        Self {
            inner: LocalToolRouter::new(tools),
            root,
            cache,
        }
    }

    /// Attach host-provided tool slots to the workspace tools.
    pub fn with_slots(mut self, slots: ToolSlots) -> Self {
        self.inner = self.inner.with_slots(slots);
        self
    }

    /// Return the VFS root used by all workspace tools.
    pub fn root(&self) -> &AsyncVfsPath {
        &self.root
    }

    /// Return the workspace path context exposed to models and UI callers.
    pub fn workspace_context(&self) -> WorkspaceContext {
        let filesystem_root = self.root.root();
        WorkspaceContext {
            current_workspace: if self.root.is_root() {
                "/".to_string()
            } else {
                self.root.as_str().to_string()
            },
            filesystem_root: if filesystem_root.is_root() {
                "/".to_string()
            } else {
                filesystem_root.as_str().to_string()
            },
        }
    }

    /// Return the shared read/write/edit snapshot cache.
    pub fn cache(&self) -> &FileCache {
        &self.cache
    }

    /// Clear the shared read/write/edit snapshot cache.
    pub fn clear_cache(&self) {
        self.cache.lock().unwrap().clear();
    }
}

fn bash_working_directory() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[async_trait]
impl ToolRouter for WorkspaceToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        self.inner.definitions().await
    }

    async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolResult> {
        self.inner.execute(call, cancel).await
    }

    async fn execution_policy(&self, call: &ToolCall) -> Result<ToolExecutionPolicy> {
        self.inner.execution_policy(call).await
    }
}
