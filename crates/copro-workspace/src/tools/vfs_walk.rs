use crate::tools::utils::{read_file_bytes, resolve_path};
use futures_util::StreamExt;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use vfs::async_vfs::AsyncVfsPath;
use vfs::{VfsFileType, VfsMetadata};

pub(crate) async fn directory_entries(
    path: &AsyncVfsPath,
) -> Result<Vec<(AsyncVfsPath, VfsMetadata)>, String> {
    let mut entries = Vec::new();
    let mut stream = path.read_dir().await.map_err(|error| error.to_string())?;
    while let Some(entry) = stream.next().await {
        let metadata = entry.metadata().await.map_err(|error| error.to_string())?;
        entries.push((entry, metadata));
    }
    Ok(entries)
}

pub(crate) async fn load_ancestor_gitignores(
    root: &AsyncVfsPath,
    path: &AsyncVfsPath,
) -> Result<Vec<Gitignore>, String> {
    let mut matchers = Vec::new();
    for directory in ancestor_directory_strings(path) {
        let dir = if directory.is_empty() {
            root.clone()
        } else {
            resolve_path(root, &directory)?
        };
        if let Some(matcher) = load_gitignore_in_dir(&dir).await? {
            matchers.push(matcher);
        }
    }
    Ok(matchers)
}

fn ancestor_directory_strings(path: &AsyncVfsPath) -> Vec<String> {
    let display = display_path(path);
    if display == "." {
        return Vec::new();
    }

    let Some(parent) = Path::new(&display).parent() else {
        return Vec::new();
    };

    let mut directories = vec![String::new()];
    let mut current = String::new();
    for component in parent.components() {
        let component = component.as_os_str().to_string_lossy();
        if component.is_empty() || component == "." {
            continue;
        }
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(&component);
        directories.push(current.clone());
    }
    directories
}

pub(crate) async fn load_gitignore_in_dir(dir: &AsyncVfsPath) -> Result<Option<Gitignore>, String> {
    let path = dir.join(".gitignore").map_err(|error| error.to_string())?;
    if !path.exists().await.map_err(|error| error.to_string())? {
        return Ok(None);
    }

    let metadata = path.metadata().await.map_err(|error| error.to_string())?;
    if metadata.file_type != VfsFileType::File {
        return Ok(None);
    }

    load_gitignore_file(&path, metadata.len).await
}

pub(crate) async fn load_gitignore_file(
    path: &AsyncVfsPath,
    byte_len: u64,
) -> Result<Option<Gitignore>, String> {
    let bytes = read_file_bytes(path, byte_len.try_into().unwrap_or_default()).await?;
    let text = String::from_utf8_lossy(&bytes);
    let source = PathBuf::from(display_path(path));
    let mut builder = GitignoreBuilder::new(gitignore_root(path));

    for (index, line) in text.lines().enumerate() {
        let line = if index == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        builder
            .add_line(Some(source.clone()), line)
            .map_err(|error| {
                format!(
                    "invalid .gitignore rule in {}:{}: {error}",
                    source.display(),
                    index + 1
                )
            })?;
    }

    let matcher = builder
        .build()
        .map_err(|error| format!("invalid .gitignore {}: {error}", source.display()))?;
    Ok((!matcher.is_empty()).then_some(matcher))
}

pub(crate) fn gitignore_is_ignored(
    matchers: &[Gitignore],
    path: &AsyncVfsPath,
    is_dir: bool,
) -> bool {
    let display = display_path(path);
    let candidate = Path::new(&display);
    let mut ignored = false;

    for matcher in matchers {
        if !path_is_under(candidate, matcher.path()) {
            continue;
        }

        match matcher.matched_path_or_any_parents(candidate, is_dir) {
            ignore::Match::None => {}
            ignore::Match::Ignore(_) => ignored = true,
            ignore::Match::Whitelist(_) => ignored = false,
        }
    }

    ignored
}

pub(crate) fn compare_modified_desc_then_path(
    left_modified: Option<&SystemTime>,
    left_path: &str,
    right_modified: Option<&SystemTime>,
    right_path: &str,
) -> Ordering {
    let left_key = modified_sort_key(left_modified);
    let right_key = modified_sort_key(right_modified);
    right_key
        .cmp(&left_key)
        .then_with(|| left_path.cmp(right_path))
}

fn modified_sort_key(modified: Option<&SystemTime>) -> Option<Duration> {
    modified.and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
}

fn gitignore_root(path: &AsyncVfsPath) -> PathBuf {
    let parent = display_path(&path.parent());
    if parent == "." {
        PathBuf::from(".")
    } else {
        PathBuf::from(parent)
    }
}

pub(crate) fn path_is_under(path: &Path, root: &Path) -> bool {
    root == Path::new(".") || root.as_os_str().is_empty() || path == root || path.starts_with(root)
}

const VCS_DIRECTORIES: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl", "_darcs"];

pub(crate) fn is_vcs_dir(path: &AsyncVfsPath) -> bool {
    VCS_DIRECTORIES.contains(&path.filename().as_str())
}

pub(crate) fn is_under_vcs_dir(path: &AsyncVfsPath) -> bool {
    Path::new(&display_path(path))
        .components()
        .any(|component| {
            VCS_DIRECTORIES
                .iter()
                .any(|dir| component.as_os_str() == *dir)
        })
}

pub(crate) fn display_path(path: &AsyncVfsPath) -> String {
    let path = path.as_str().trim_start_matches('/');
    if path.is_empty() {
        ".".to_string()
    } else {
        path.to_string()
    }
}
