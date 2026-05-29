use crate::tools::{
    EditTool, FileCache, GlobTool, GrepTool, LsTool, ReadTool, ReadToolConfig, WriteTool,
};
use copro_agent::{CancellationToken, ToolExecutionPolicy, ToolRouter};
use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{ToolCall, ToolResult};
use copro_api::tool::ToolDefinition;
use copro_harness::tools::{ErasedTool, LocalToolRouter};
use std::sync::Arc;
use vfs::async_vfs::AsyncVfsPath;

/// Tool router that exposes the standard workspace tools for a VFS root.
///
/// The router wires `read`, `write`, and `edit` together with a shared file
/// snapshot cache so write/edit safety checks see files read through the same
/// router. It also exposes the read-only discovery tools: `ls`, `glob`, and
/// `grep`.
#[derive(Clone)]
pub struct WorkspaceToolRouter {
    inner: LocalToolRouter,
    root: AsyncVfsPath,
    cache: FileCache,
}

impl WorkspaceToolRouter {
    pub fn new(root: AsyncVfsPath) -> Self {
        let cache = FileCache::default();
        let tools: Vec<Arc<dyn ErasedTool>> = vec![
            Arc::new(ReadTool::with_cache(
                root.clone(),
                ReadToolConfig::default(),
                Arc::clone(&cache),
            )),
            Arc::new(WriteTool::with_cache(root.clone(), Arc::clone(&cache))),
            Arc::new(EditTool::with_cache(root.clone(), Arc::clone(&cache))),
            Arc::new(GrepTool::new(root.clone())),
            Arc::new(GlobTool::new(root.clone())),
            Arc::new(LsTool::new(root.clone())),
        ];

        Self {
            inner: LocalToolRouter::new(tools),
            root,
            cache,
        }
    }

    /// Return the VFS root used by all workspace tools.
    pub fn root(&self) -> &AsyncVfsPath {
        &self.root
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
