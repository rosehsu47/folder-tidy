//! Scan a directory for regular files.

use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use walkdir::WalkDir;

use crate::classify::FileInfo;

#[derive(Debug)]
pub enum ScanError {
    NotADirectory(PathBuf),
    Walk(walkdir::Error),
    Metadata {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::NotADirectory(p) => write!(f, "not a directory: {}", p.display()),
            ScanError::Walk(e) => write!(f, "scan failed: {e}"),
            ScanError::Metadata { path, source } => {
                write!(f, "cannot read metadata of {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ScanError {}

/// Lists regular files under `root` (top level only unless `recursive`),
/// sorted by path. Symlinks are not followed and not listed.
pub fn scan(root: &Path, recursive: bool) -> Result<Vec<FileInfo>, ScanError> {
    if !root.is_dir() {
        return Err(ScanError::NotADirectory(root.to_path_buf()));
    }
    let mut walker = WalkDir::new(root).min_depth(1).sort_by_file_name();
    if !recursive {
        walker = walker.max_depth(1);
    }
    let mut files = Vec::new();
    for entry in walker {
        let entry = entry.map_err(ScanError::Walk)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        let modified = entry
            .metadata()
            .map_err(|e| ScanError::Metadata {
                path: path.clone(),
                source: e.into(),
            })?
            .modified()
            .map_err(|e| ScanError::Metadata {
                path: path.clone(),
                source: e,
            })?;
        files.push(FileInfo {
            path,
            modified: DateTime::<Local>::from(modified).date_naive(),
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names(files: &[FileInfo], root: &Path) -> Vec<String> {
        files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn non_recursive_by_default_and_recursive_by_flag() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.txt"), "b").unwrap();

        assert_eq!(
            names(&scan(dir.path(), false).unwrap(), dir.path()),
            vec!["a.txt"]
        );
        assert_eq!(
            names(&scan(dir.path(), true).unwrap(), dir.path()),
            vec!["a.txt", "sub/b.txt"]
        );
    }

    #[test]
    fn reads_modified_date() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("old.bin");
        fs::write(&p, "x").unwrap();
        let when = Local::now() - chrono::Duration::days(400);
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(when.into())
            .unwrap();
        let files = scan(dir.path(), false).unwrap();
        assert_eq!(files[0].modified, when.date_naive());
    }

    #[test]
    fn missing_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = scan(&dir.path().join("nope"), false).unwrap_err();
        assert!(matches!(err, ScanError::NotADirectory(_)));
    }
}
