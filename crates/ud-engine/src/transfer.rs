//! Sending and receiving files.
//!
//! Received paths are rebuilt from scratch under the download directory, so a
//! malicious or buggy peer cannot write outside it. Every path component is
//! validated individually, which also covers the Windows device names and
//! trailing-dot tricks.

use std::path::{Component, Path, PathBuf};

use ud_core::protocol::{FileEntry, TransferId};

use crate::view::{TransferDirection, TransferState, TransferFileView, TransferView};
use ud_core::identity::DeviceId;

/// Bytes read from disk per chunk when sending.
pub const CHUNK_BYTES: usize = 48 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("path is not safe to write: {0}")]
    UnsafePath(String),

    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("the peer sent data for an unknown transfer")]
    UnknownTransfer,

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = TransferError> = std::result::Result<T, E>;

/// Names Windows reserves for devices; writing them would either fail or hit a
/// device instead of a file.
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validates a single path component.
pub fn sanitize_component(component: &str) -> Result<String> {
    let trimmed = component.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err(TransferError::UnsafePath(component.to_string()));
    }
    // Leading or trailing whitespace is stripped by Windows, which could turn
    // one name into a different one; refuse rather than guess.
    if trimmed != component {
        return Err(TransferError::UnsafePath(component.to_string()));
    }
    if trimmed.contains(['/', '\\', '\0']) {
        return Err(TransferError::UnsafePath(component.to_string()));
    }
    let stem = trimmed
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if WINDOWS_RESERVED.contains(&stem.as_str()) {
        return Err(TransferError::UnsafePath(component.to_string()));
    }
    // A trailing dot is dropped by Windows as well.
    if trimmed.ends_with('.') {
        return Err(TransferError::UnsafePath(component.to_string()));
    }
    if trimmed.chars().any(|c| (c as u32) < 0x20) {
        return Err(TransferError::UnsafePath(component.to_string()));
    }
    Ok(trimmed.to_string())
}

/// Turns a peer supplied relative path into a safe path below `root`.
pub fn sanitize_relative(relative: &str, root: &Path) -> Result<PathBuf> {
    let normalized = relative.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains(':') {
        return Err(TransferError::UnsafePath(relative.to_string()));
    }
    let mut out = root.to_path_buf();
    let mut components = 0;
    for raw in normalized.split('/') {
        if raw.is_empty() {
            continue;
        }
        out.push(sanitize_component(raw)?);
        components += 1;
    }
    if components == 0 {
        return Err(TransferError::UnsafePath(relative.to_string()));
    }
    // Belt and braces: the assembled path must stay below the root.
    if !out.starts_with(root) {
        return Err(TransferError::UnsafePath(relative.to_string()));
    }
    for component in out.components() {
        // RootDir is expected whenever the root itself is absolute; a ParentDir
        // component never is.
        if matches!(component, Component::ParentDir) {
            return Err(TransferError::UnsafePath(relative.to_string()));
        }
    }
    Ok(out)
}

/// Picks a path that does not exist yet, appending " (2)", " (3)" and so on.
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let extension = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for n in 2..10_000 {
        let candidate = parent.join(format!("{stem} ({n}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

/// A file queued for sending: what the peer will see, and where it lives here.
#[derive(Debug, Clone)]
pub struct OutgoingFile {
    pub entry: FileEntry,
    /// `None` for directories, which only ever create a folder on the far side.
    pub source: Option<PathBuf>,
}

/// Walks a list of user chosen paths into a flat offer list.
///
/// Directories keep their relative structure so the receiving side can rebuild
/// the tree. Symbolic links are not followed, which keeps a careless drag of a
/// home directory from turning into an accidental upload of an entire disk.
pub fn collect_entries(paths: &[PathBuf]) -> Result<Vec<OutgoingFile>> {
    let mut entries: Vec<OutgoingFile> = Vec::new();
    let mut next_index = 0u32;

    fn walk(
        path: &Path,
        prefix: &str,
        entries: &mut Vec<OutgoingFile>,
        next_index: &mut u32,
    ) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path).map_err(|source| TransferError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or_else(|| TransferError::Other(format!("{} has no file name", path.display())))?;
        let sanitized = sanitize_component(&name)?;
        let relative = if prefix.is_empty() {
            sanitized.clone()
        } else {
            format!("{prefix}/{sanitized}")
        };

        if metadata.is_dir() {
            entries.push(OutgoingFile {
                entry: FileEntry {
                    index: *next_index,
                    name: sanitized,
                    relative_path: relative.clone(),
                    size: 0,
                    is_dir: true,
                },
                source: None,
            });
            *next_index += 1;
            let mut children: Vec<PathBuf> = std::fs::read_dir(path)
                .map_err(|source| TransferError::Io {
                    path: path.to_path_buf(),
                    source,
                })?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .collect();
            children.sort();
            for child in children {
                walk(&child, &relative, entries, next_index)?;
            }
        } else {
            entries.push(OutgoingFile {
                entry: FileEntry {
                    index: *next_index,
                    name: sanitized,
                    relative_path: relative,
                    size: metadata.len(),
                    is_dir: false,
                },
                source: Some(path.to_path_buf()),
            });
            *next_index += 1;
        }
        Ok(())
    }

    for path in paths {
        // Top level entries keep just their name; nested ones keep the tree.
        walk(path, "", &mut entries, &mut next_index)?;
    }
    Ok(entries)
}

/// Aggregated bookkeeping for an outbound transfer.
#[derive(Debug, Clone)]
pub struct OutgoingTransfer {
    pub id: TransferId,
    pub peer: DeviceId,
    pub peer_name: String,
    pub files: Vec<OutgoingFile>,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub started_at: u64,
    /// Whether the peer has accepted and streaming may begin.
    pub accepted: bool,
    pub finished: bool,
}

/// Aggregated bookkeeping for an inbound transfer.
#[derive(Debug, Clone)]
pub struct IncomingTransfer {
    pub id: TransferId,
    pub peer: DeviceId,
    pub peer_name: String,
    pub entries: Vec<FileEntry>,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub started_at: u64,
    pub root: PathBuf,
    pub resolved: Vec<Option<PathBuf>>,
    pub written: Vec<u64>,
}

impl OutgoingTransfer {
    pub fn new(
        id: TransferId,
        peer: DeviceId,
        peer_name: String,
        files: Vec<OutgoingFile>,
    ) -> Self {
        let total_bytes = files.iter().map(|f| f.entry.size).sum();
        Self {
            id,
            peer,
            peer_name,
            files,
            total_bytes,
            done_bytes: 0,
            started_at: ud_core::now_unix(),
            accepted: false,
            finished: false,
        }
    }

    /// The wire representation of the offer.
    pub fn entries(&self) -> Vec<FileEntry> {
        self.files.iter().map(|file| file.entry.clone()).collect()
    }

    pub fn view(&self, state: TransferState, bytes_per_second: u64) -> TransferView {
        TransferView {
            id: self.id,
            peer: self.peer.clone(),
            peer_name: self.peer_name.clone(),
            direction: TransferDirection::Sending,
            state,
            files: self
                .files
                .iter()
                .map(|file| TransferFileView {
                    index: file.entry.index,
                    name: file.entry.name.clone(),
                    relative_path: file.entry.relative_path.clone(),
                    size: file.entry.size,
                    done_bytes: 0,
                    is_dir: file.entry.is_dir,
                })
                .collect(),
            total_bytes: self.total_bytes,
            done_bytes: self.done_bytes,
            bytes_per_second,
            started_at: self.started_at,
            destination: None,
            error: None,
            needs_acceptance: false,
        }
    }
}

impl IncomingTransfer {
    pub fn new(
        id: TransferId,
        peer: DeviceId,
        peer_name: String,
        entries: Vec<FileEntry>,
        root: PathBuf,
    ) -> Self {
        let total_bytes = entries.iter().map(|e| e.size).sum();
        let resolved = vec![None; entries.len()];
        let written = vec![0u64; entries.len()];
        Self {
            id,
            peer,
            peer_name,
            entries,
            total_bytes,
            done_bytes: 0,
            started_at: ud_core::now_unix(),
            root,
            resolved,
            written,
        }
    }

    pub fn view(&self, state: TransferState, bytes_per_second: u64, error: Option<String>) -> TransferView {
        TransferView {
            id: self.id,
            peer: self.peer.clone(),
            peer_name: self.peer_name.clone(),
            direction: TransferDirection::Receiving,
            state,
            files: self
                .entries
                .iter()
                .enumerate()
                .map(|(position, entry)| TransferFileView {
                    index: entry.index,
                    name: entry.name.clone(),
                    relative_path: entry.relative_path.clone(),
                    size: entry.size,
                    done_bytes: self.written.get(position).copied().unwrap_or(0),
                    is_dir: entry.is_dir,
                })
                .collect(),
            total_bytes: self.total_bytes,
            done_bytes: self.done_bytes,
            bytes_per_second,
            started_at: self.started_at,
            destination: Some(self.root.display().to_string()),
            error,
            needs_acceptance: state == TransferState::Pending,
        }
    }

    pub fn position_of(&self, index: u32) -> Option<usize> {
        self.entries.iter().position(|e| e.index == index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_attempts_are_refused() {
        let root = Path::new("/tmp/root");
        assert!(sanitize_relative("../../etc/passwd", root).is_err());
        assert!(sanitize_relative("/etc/passwd", root).is_err());
        assert!(sanitize_relative("C:/Windows/system32", root).is_err());
        assert!(sanitize_relative("a/../../b", root).is_err());
        assert!(sanitize_relative("..", root).is_err());
    }

    #[test]
    fn nested_paths_land_under_the_root() {
        let root = PathBuf::from("/tmp/root");
        let resolved = sanitize_relative("photos/2024/a.jpg", &root).unwrap();
        assert_eq!(resolved, root.join("photos").join("2024").join("a.jpg"));
        assert!(resolved.starts_with(&root));
    }

    #[test]
    fn windows_device_names_and_trailing_dots_are_refused() {
        assert!(sanitize_component("CON").is_err());
        assert!(sanitize_component("nul.txt").is_err());
        assert!(sanitize_component("report.").is_err());
        assert!(sanitize_component("report ").is_err());
        assert!(sanitize_component("report.txt").is_ok());
    }

    #[test]
    fn unique_path_never_steps_on_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("ud-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("notes.txt");
        std::fs::write(&first, b"one").unwrap();
        let second = unique_path(&first);
        assert_ne!(second, first);
        assert!(second.to_string_lossy().contains("notes (2)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
