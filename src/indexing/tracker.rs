use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

/// Classification of a detected file change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// A single file change detected by the tracker.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
}

/// File metadata snapshot used to detect staleness.
#[derive(Debug, Clone)]
pub struct FileStat {
    pub path: String,
    pub mtime: i64,
    pub size: i64,
}

/// Detect stale files by comparing on-disk mtime/size against stored metadata.
///
/// Returns a list of `FileChange` entries for files that are:
/// - new (in `current_files` but not in `indexed_meta`)
/// - modified (mtime or size differs)
/// - deleted (in `indexed_meta` but no longer on disk / not in `current_files`)
pub fn detect_changes(
    current_files: &[String],
    indexed_meta: &std::collections::HashMap<String, (i64, i64)>, // path → (mtime, size)
) -> Vec<FileChange> {
    let mut changes = Vec::new();

    // Check for added/modified files.
    for path in current_files {
        let stat = match stat_file(path) {
            Some(s) => s,
            None => continue, // file disappeared between walk and stat
        };

        match indexed_meta.get(path) {
            None => {
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Added,
                });
            }
            Some(&(indexed_mtime, indexed_size)) => {
                if stat.mtime != indexed_mtime || stat.size != indexed_size {
                    changes.push(FileChange {
                        path: path.clone(),
                        kind: ChangeKind::Modified,
                    });
                }
            }
        }
    }

    // Check for deleted files.
    let current_set: std::collections::HashSet<&str> =
        current_files.iter().map(|s| s.as_str()).collect();
    for path in indexed_meta.keys() {
        if !current_set.contains(path.as_str()) {
            changes.push(FileChange {
                path: path.clone(),
                kind: ChangeKind::Deleted,
            });
        }
    }

    changes
}

/// Read mtime and size from the filesystem.
pub fn stat_file(path: &str) -> Option<FileStat> {
    let meta = std::fs::metadata(Path::new(path)).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    let size = meta.len() as i64;
    Some(FileStat {
        path: path.to_string(),
        mtime,
        size,
    })
}
