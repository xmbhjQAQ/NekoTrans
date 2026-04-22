use crate::models::{FileFingerprint, TransferItem, now_epoch_ms};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug)]
pub enum InventoryBuildError {
    MissingPath(PathBuf),
    Io(String),
    InvalidSourceRoot(PathBuf),
}

impl std::fmt::Display for InventoryBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPath(path) => write!(f, "missing path {}", path.display()),
            Self::Io(message) => write!(f, "{message}"),
            Self::InvalidSourceRoot(path) => write!(f, "invalid source root {}", path.display()),
        }
    }
}

impl std::error::Error for InventoryBuildError {}

pub fn expand_sources(
    source_root: &Path,
    selected_paths: &[PathBuf],
) -> Result<Vec<TransferItem>, InventoryBuildError> {
    if !source_root.exists() {
        return Err(InventoryBuildError::InvalidSourceRoot(
            source_root.to_path_buf(),
        ));
    }

    let canonical_root = source_root
        .canonicalize()
        .map_err(|err| InventoryBuildError::Io(err.to_string()))?;
    let mut items = Vec::new();

    for selected in selected_paths {
        let resolved = if selected.is_absolute() {
            selected.clone()
        } else {
            canonical_root.join(selected)
        };

        if !resolved.exists() {
            return Err(InventoryBuildError::MissingPath(resolved));
        }

        collect_items(&canonical_root, &resolved, &mut items)?;
    }

    items.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    items.dedup_by(|left, right| left.relative_path == right.relative_path);
    Ok(items)
}

fn collect_items(
    source_root: &Path,
    current_path: &Path,
    items: &mut Vec<TransferItem>,
) -> Result<(), InventoryBuildError> {
    let metadata =
        fs::metadata(current_path).map_err(|err| InventoryBuildError::Io(err.to_string()))?;
    if metadata.is_dir() {
        for entry in
            fs::read_dir(current_path).map_err(|err| InventoryBuildError::Io(err.to_string()))?
        {
            let path = entry
                .map_err(|err| InventoryBuildError::Io(err.to_string()))?
                .path();
            collect_items(source_root, &path, items)?;
        }
        return Ok(());
    }

    let canonical = current_path
        .canonicalize()
        .map_err(|err| InventoryBuildError::Io(err.to_string()))?;
    let relative_path = canonical
        .strip_prefix(source_root)
        .map_err(|_| InventoryBuildError::InvalidSourceRoot(source_root.to_path_buf()))?
        .to_path_buf();
    let modified_at_epoch_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis())
        .unwrap_or_else(now_epoch_ms);

    items.push(TransferItem {
        relative_path,
        size_bytes: metadata.len(),
        modified_at_epoch_ms,
        fingerprint: Some(fast_fingerprint(&metadata)),
    });
    Ok(())
}

fn fast_fingerprint(metadata: &fs::Metadata) -> FileFingerprint {
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis())
        .unwrap_or(0);

    FileFingerprint {
        algorithm: "size-mtime",
        hex_digest: format!("{:x}-{:x}", metadata.len(), modified_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::expand_sources;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_temp_dir() -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "nekotrans-inventory-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("nested")).expect("create nested directory");
        fs::write(path.join("nested").join("a.txt"), b"hello").expect("write test file");
        fs::write(path.join("b.txt"), b"world").expect("write test file");
        path
    }

    #[test]
    fn inventory_expands_files_from_root() {
        let root = unique_temp_dir();
        let items = expand_sources(&root, &[PathBuf::from("nested"), PathBuf::from("b.txt")])
            .expect("inventory should succeed");

        let relative_paths = items
            .iter()
            .map(|item| item.relative_path.to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(relative_paths.len(), 2);
        assert!(
            relative_paths.contains(&"nested\\a.txt".to_string())
                || relative_paths.contains(&"nested/a.txt".to_string())
        );
        assert!(relative_paths.contains(&"b.txt".to_string()));
    }
}
