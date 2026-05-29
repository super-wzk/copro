use async_std::io::ReadExt;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use vfs::VfsMetadata;
use vfs::async_vfs::AsyncVfsPath;

// ---- file snapshot & cache types ------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub content_hash: u64,
}

impl FileSnapshot {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            len: bytes.len() as u64,
            modified: None,
            content_hash: content_hash(bytes),
        }
    }

    pub(crate) fn from_metadata_and_bytes(metadata: &VfsMetadata, bytes: &[u8]) -> Self {
        Self {
            len: metadata.len,
            modified: metadata.modified,
            content_hash: content_hash(bytes),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    pub snapshot: FileSnapshot,
}

pub type FileCache = Arc<Mutex<HashMap<String, CacheEntry>>>;

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

// ---- utility functions ----------------------------------------------------

/// Join `input_path` onto `root`, mapping VFS errors to strings.
pub(crate) fn resolve_path(root: &AsyncVfsPath, input_path: &str) -> Result<AsyncVfsPath, String> {
    root.join(input_path).map_err(|error| error.to_string())
}

/// Read the entire contents of a file into a `Vec<u8>`. Pass 0 for `capacity`
/// when file size is unknown.
pub(crate) async fn read_file_bytes(
    path: &AsyncVfsPath,
    capacity: usize,
) -> Result<Vec<u8>, String> {
    let mut file = path.open_file().await.map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(capacity);
    file.read_to_end(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

/// Convert UTF-8 bytes into a `String`, mapping the error to a human-readable message.
pub(crate) fn validate_utf8(bytes: Vec<u8>, path: &str) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| format!("{path} is not valid UTF-8"))
}
