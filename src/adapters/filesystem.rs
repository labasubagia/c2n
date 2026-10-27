//! Filesystem and output-path helpers.

use std::path::{Component, Path, PathBuf};

use crate::shared::constants::DB_FILE_NAME;

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("absolute paths are not allowed here: {0}")]
    AbsoluteNotAllowed(String),
    #[error("path attempts to escape the output directory: {0}")]
    Escape(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolves the data directory and account-specific image directories.
#[derive(Debug, Clone)]
pub struct Filesystem {
    data_dir: PathBuf,
}

impl Default for Filesystem {
    fn default() -> Self {
        Self::new()
    }
}

impl Filesystem {
    pub fn new() -> Self {
        Self {
            data_dir: crate::shared::constants::data_dir(),
        }
    }

    pub fn with_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join(DB_FILE_NAME)
    }

    /// Return the image folder: `<data_dir>/images`, creating it.
    pub fn image_folder(&self) -> Result<PathBuf, FsError> {
        let folder = self.data_dir.join("images");
        std::fs::create_dir_all(&folder)?;
        Ok(folder)
    }

    /// Resolve a relative path beneath the output directory, rejecting escapes.
    pub fn resolve_under_output(&self, relative: &Path) -> Result<PathBuf, FsError> {
        if relative.is_absolute() {
            return Err(FsError::AbsoluteNotAllowed(relative.display().to_string()));
        }

        let mut resolved = self.data_dir.clone();
        for component in relative.components() {
            match component {
                Component::Normal(part) => resolved.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                    if !resolved.starts_with(&self.data_dir) {
                        return Err(FsError::Escape(relative.display().to_string()));
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(FsError::AbsoluteNotAllowed(relative.display().to_string()));
                }
            }
        }

        if !resolved.starts_with(&self.data_dir) {
            return Err(FsError::Escape(relative.display().to_string()));
        }
        self.ensure_existing_ancestor_within_output(&resolved)?;
        Ok(resolved)
    }

    /// Reject paths whose existing ancestors resolve outside the output root,
    /// including paths that traverse a symlink inside `output/`.
    fn ensure_existing_ancestor_within_output(&self, path: &Path) -> Result<(), FsError> {
        let root = std::fs::canonicalize(&self.data_dir).unwrap_or_else(|_| self.data_dir.clone());
        let mut current = path;
        loop {
            if current == self.data_dir {
                // The data dir itself is the trust boundary; it may be
                // created later and may itself be a symlink.
                return Ok(());
            }
            match std::fs::symlink_metadata(current) {
                Ok(_) => {
                    // `canonicalize` follows symlinks. Using `symlink_metadata`
                    // first means even a dangling symlink is checked instead of
                    // being treated as an absent path and silently allowed.
                    let canonical = std::fs::canonicalize(current).map_err(|error| {
                        if error.kind() == std::io::ErrorKind::NotFound {
                            FsError::Escape(path.display().to_string())
                        } else {
                            FsError::Io(error)
                        }
                    })?;
                    if canonical.starts_with(&root) {
                        return Ok(());
                    }
                    return Err(FsError::Escape(path.display().to_string()));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(FsError::Io(error)),
            }
            match current.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => current = parent,
                _ => return Ok(()),
            }
        }
    }

    /// Remove everything in `output/` except the database and `.gitkeep`.
    pub fn clean_output(&self) -> Result<(), FsError> {
        if !self.data_dir.exists() {
            return Ok(());
        }

        let db_path = self.db_path();
        for entry in std::fs::read_dir(&self.data_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if path == db_path {
                continue;
            }
            if file_type.is_file()
                && matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(".gitkeep" | "chatgpt.db-wal" | "chatgpt.db-shm" | "chatgpt.db-journal")
                )
            {
                continue;
            }
            if file_type.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_escape() {
        let filesystem = Filesystem::with_data_dir("/tmp/c2n-output");
        let error = filesystem
            .resolve_under_output(Path::new("../outside"))
            .expect_err("escape should be rejected");
        assert!(matches!(error, FsError::Escape(_)));
    }

    #[test]
    fn resolves_relative_paths_under_output() {
        let filesystem = Filesystem::with_data_dir("/tmp/c2n-output");
        let resolved = filesystem
            .resolve_under_output(Path::new("images/sub"))
            .unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/c2n-output/images/sub"));
    }

    #[test]
    fn defaults_to_images_under_output() {
        let temp = tempfile::tempdir().unwrap();
        let filesystem = Filesystem::with_data_dir(temp.path().join("output"));
        let resolved = filesystem.image_folder().unwrap();
        assert_eq!(resolved, temp.path().join("output/images"));
        assert!(resolved.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let filesystem = Filesystem::with_data_dir(&root);
        let error = filesystem
            .resolve_under_output(Path::new("link/image.png"))
            .expect_err("symlink escape should be rejected");
        assert!(matches!(error, FsError::Escape(_)));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        let outside = temp.path().join("does-not-exist-yet");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let filesystem = Filesystem::with_data_dir(&root);
        let error = filesystem
            .resolve_under_output(Path::new("link/image.png"))
            .expect_err("dangling symlink escape should be rejected");
        assert!(matches!(error, FsError::Escape(_)));
    }

    #[cfg(unix)]
    #[test]
    fn clean_output_removes_symlinks_without_following_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.png"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let filesystem = Filesystem::with_data_dir(&root);
        filesystem.clean_output().unwrap();

        assert!(outside.join("keep.png").exists());
        assert!(!root.join("link").exists());
    }

    #[test]
    fn cleans_output_but_preserves_database_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        std::fs::create_dir_all(root.join("images")).unwrap();
        for name in [
            "chatgpt.db",
            "chatgpt.db-wal",
            "chatgpt.db-shm",
            "chatgpt.db-journal",
            ".gitkeep",
        ] {
            std::fs::write(root.join(name), b"keep").unwrap();
        }
        std::fs::write(root.join("images").join("old.png"), b"remove").unwrap();

        let filesystem = Filesystem::with_data_dir(&root);
        filesystem.clean_output().unwrap();

        for name in [
            "chatgpt.db",
            "chatgpt.db-wal",
            "chatgpt.db-shm",
            "chatgpt.db-journal",
            ".gitkeep",
        ] {
            assert!(root.join(name).exists(), "{name} should be preserved");
        }
        assert!(!root.join("images").exists());
    }
}
