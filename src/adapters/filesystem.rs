//! Filesystem and output-path helpers.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

pub(crate) struct SafeFileTarget {
    directory: File,
    directory_path: PathBuf,
    file_name: OsString,
}

pub(crate) struct AnchoredTempFile {
    directory: File,
    directory_path: PathBuf,
    file: File,
    name: OsString,
    persisted: bool,
}

impl AnchoredTempFile {
    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

impl Drop for AnchoredTempFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = remove_relative(&self.directory, &self.directory_path, &self.name);
        }
    }
}

impl SafeFileTarget {
    fn from_parts(directory_path: PathBuf, file_name: OsString) -> Result<Self, FsError> {
        let directory = open_directory_secure(&directory_path)?;
        Ok(Self {
            directory,
            directory_path,
            file_name,
        })
    }

    pub(crate) fn open(path: &Path) -> Result<Self, FsError> {
        let original_parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let directory_path = std::fs::canonicalize(original_parent)?;
        let file_name = path
            .file_name()
            .ok_or_else(|| FsError::Escape(path.display().to_string()))?
            .to_os_string();
        Self::from_parts(directory_path, file_name)
    }

    pub(crate) fn directory_path(&self) -> &Path {
        &self.directory_path
    }

    pub(crate) fn open_read(&self) -> Result<File, FsError> {
        open_relative(&self.directory, &self.directory_path, &self.file_name).map_err(FsError::Io)
    }

    pub(crate) fn create_temp(&self) -> Result<AnchoredTempFile, FsError> {
        static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
        for _ in 0..128 {
            let name = OsString::from(format!(
                ".c2n-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            match create_relative(&self.directory, &self.directory_path, &name) {
                Ok(file) => {
                    let directory = match self.directory.try_clone() {
                        Ok(directory) => directory,
                        Err(error) => {
                            let _ = remove_relative(&self.directory, &self.directory_path, &name);
                            return Err(FsError::Io(error));
                        }
                    };
                    return Ok(AnchoredTempFile {
                        directory,
                        directory_path: self.directory_path.clone(),
                        file,
                        name,
                        persisted: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(FsError::Io(error)),
            }
        }
        Err(FsError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate unique temporary image file",
        )))
    }

    pub(crate) fn replace(&self, mut temporary: AnchoredTempFile) -> Result<(), FsError> {
        // Durability: fsync the temp contents before the rename so a crash
        // between rename and the directory sync cannot leave a partial file
        // at the destination.
        temporary.file_mut().sync_all()?;
        rename_relative(
            &self.directory,
            &self.directory_path,
            &temporary.name,
            &self.file_name,
        )?;
        temporary.persisted = true;
        self.directory.sync_all()?;
        Ok(())
    }

    pub(crate) fn remove(&self) -> Result<(), FsError> {
        remove_relative(&self.directory, &self.directory_path, &self.file_name).map_err(FsError::Io)
    }
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

    /// Return the image folder: `<data_dir>/images`, creating it only when
    /// its path remains within the data directory (including symlink checks).
    pub fn image_folder(&self) -> Result<PathBuf, FsError> {
        let folder = self.resolve_under_output(Path::new("images"))?;
        std::fs::create_dir_all(&folder)?;
        self.ensure_existing_ancestor_within_output(&folder)?;
        Ok(folder)
    }

    /// Verify that a path under the images directory does not traverse a
    /// symlink outside the data directory.
    pub fn validate_image_path(&self, path: &Path) -> Result<(), FsError> {
        let folder = self.data_dir.join("images");
        let relative = path
            .strip_prefix(&folder)
            .map_err(|_| FsError::Escape(path.display().to_string()))?;
        let relative = Path::new("images").join(relative);
        self.resolve_under_output(&relative).map(|_| ())
    }

    /// Return an image path whose parent is canonicalized beneath the data
    /// root. File operations can use this stable parent instead of resolving a
    /// potentially replaced `images` symlink again.
    pub fn safe_image_path(&self, path: &Path) -> Result<PathBuf, FsError> {
        self.validate_image_path(path)?;
        let file_name = path
            .file_name()
            .ok_or_else(|| FsError::Escape(path.display().to_string()))?;
        let parent = path
            .parent()
            .ok_or_else(|| FsError::Escape(path.display().to_string()))?;
        let canonical_root = std::fs::canonicalize(&self.data_dir)?;
        let canonical_parent = std::fs::canonicalize(parent)?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(FsError::Escape(path.display().to_string()));
        }
        Ok(canonical_parent.join(file_name))
    }

    /// Open a validated image path through a pinned parent directory.
    pub(crate) fn safe_image_target(&self, path: &Path) -> Result<SafeFileTarget, FsError> {
        let canonical = self.safe_image_path(path)?;
        let parent = canonical
            .parent()
            .ok_or_else(|| FsError::Escape(canonical.display().to_string()))?;
        let file_name = canonical
            .file_name()
            .ok_or_else(|| FsError::Escape(canonical.display().to_string()))?;
        SafeFileTarget::from_parts(parent.to_path_buf(), file_name.to_os_string())
    }

    /// Remove an image without following a replacement symlink at its filename.
    pub(crate) fn remove_image_file(&self, path: &Path) -> Result<(), FsError> {
        self.safe_image_target(path)?.remove()
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

    /// List entries `clean_output` would remove, without changing the filesystem.
    pub fn list_cleanable_entries(&self) -> Result<Vec<PathBuf>, FsError> {
        if !self.data_dir.exists() {
            return Ok(Vec::new());
        }
        let db_path = self.db_path();
        let mut removable = Vec::new();
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
            removable.push(path);
        }
        Ok(removable)
    }

    /// Remove everything in `output/` except the database and `.gitkeep`.
    pub fn clean_output(&self) -> Result<(), FsError> {
        if !self.data_dir.exists() {
            return Ok(());
        }

        let root = std::fs::canonicalize(&self.data_dir)?;
        let db_path = self.db_path();
        for entry in std::fs::read_dir(&self.data_dir)? {
            let entry = entry?;
            let path = entry.path();
            let canonical_root = std::fs::canonicalize(&self.data_dir)?;
            if canonical_root != root {
                return Err(FsError::Escape(self.data_dir.display().to_string()));
            }
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
            // A root-level symlink is unlinked, never traversed by
            // `remove_dir_all`; its target is outside this cleanup boundary.
            if file_type.is_dir() {
                let canonical = std::fs::canonicalize(&path)?;
                if !canonical.starts_with(&root) {
                    return Err(FsError::Escape(path.display().to_string()));
                }
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn open_directory_secure(path: &Path) -> Result<File, std::io::Error> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let root = c"/";
    // SAFETY: the literal is NUL-terminated and open returns an owned descriptor.
    let root_fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: root_fd is newly opened and its ownership is transferred.
    let mut current = unsafe { File::from_raw_fd(root_fd) };
    for component in path.components() {
        let Component::Normal(part) = component else {
            if matches!(component, Component::ParentDir) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "parent traversal",
                ));
            }
            continue;
        };
        let part = std::ffi::CString::new(part.as_bytes())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        // SAFETY: component is NUL-terminated; current is an open directory.
        let fd = unsafe {
            libc::openat(
                current.as_raw_fd(),
                part.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd is newly opened and its ownership is transferred.
        current = unsafe { File::from_raw_fd(fd) };
    }
    Ok(current)
}

#[cfg(not(unix))]
fn open_directory_secure(path: &Path) -> Result<File, std::io::Error> {
    File::open(path)
}

#[cfg(unix)]
fn open_relative(
    directory: &File,
    _path: &Path,
    name: &std::ffi::OsStr,
) -> Result<File, std::io::Error> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    // SAFETY: name is NUL-terminated and directory is a valid open directory.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd is newly opened and its ownership is transferred.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
fn open_relative(
    _directory: &File,
    path: &Path,
    name: &std::ffi::OsStr,
) -> Result<File, std::io::Error> {
    File::open(path.join(name))
}

#[cfg(unix)]
fn create_relative(
    directory: &File,
    _path: &Path,
    name: &std::ffi::OsStr,
) -> Result<File, std::io::Error> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    // SAFETY: name is NUL-terminated and directory is a valid open directory.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd is newly opened and its ownership is transferred.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
fn create_relative(
    _directory: &File,
    path: &Path,
    name: &std::ffi::OsStr,
) -> Result<File, std::io::Error> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join(name))
}

#[cfg(unix)]
fn rename_relative(
    directory: &File,
    _path: &Path,
    source: &std::ffi::OsStr,
    target: &std::ffi::OsStr,
) -> Result<(), FsError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source.as_bytes()).map_err(|error| {
        FsError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
    })?;
    let target = std::ffi::CString::new(target.as_bytes()).map_err(|error| {
        FsError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
    })?;
    // SAFETY: names are NUL-terminated; the directory descriptor pins both operations.
    let result = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            target.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(FsError::Io(std::io::Error::last_os_error()))
    }
}

#[cfg(not(unix))]
fn rename_relative(
    _directory: &File,
    path: &Path,
    source: &std::ffi::OsStr,
    target: &std::ffi::OsStr,
) -> Result<(), FsError> {
    std::fs::rename(path.join(source), path.join(target)).map_err(FsError::Io)
}

#[cfg(unix)]
fn remove_relative(
    directory: &File,
    _path: &Path,
    name: &std::ffi::OsStr,
) -> Result<(), std::io::Error> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    // SAFETY: name is NUL-terminated and the directory descriptor is valid.
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn remove_relative(
    _directory: &File,
    path: &Path,
    name: &std::ffi::OsStr,
) -> Result<(), std::io::Error> {
    std::fs::remove_file(path.join(name))
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

    #[cfg(unix)]
    #[test]
    fn pinned_image_operations_survive_directory_and_file_symlink_swaps() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        let images = root.join("images");
        let moved = root.join("images-moved");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("image.png"), b"do not touch").unwrap();
        let filesystem = Filesystem::with_data_dir(&root);
        let target = filesystem
            .safe_image_target(&images.join("image.png"))
            .unwrap();

        std::fs::rename(&images, &moved).unwrap();
        std::os::unix::fs::symlink(&outside, &images).unwrap();
        let mut temporary = target.create_temp().unwrap();
        temporary.file_mut().write_all(b"anchored data").unwrap();
        temporary.file_mut().sync_all().unwrap();
        target.replace(temporary).unwrap();

        assert_eq!(
            std::fs::read(moved.join("image.png")).unwrap(),
            b"anchored data"
        );
        assert_eq!(
            std::fs::read(outside.join("image.png")).unwrap(),
            b"do not touch"
        );

        std::fs::remove_file(moved.join("image.png")).unwrap();
        std::os::unix::fs::symlink(outside.join("image.png"), moved.join("image.png")).unwrap();
        target.remove().unwrap();
        assert!(!moved.join("image.png").exists());
        assert_eq!(
            std::fs::read(outside.join("image.png")).unwrap(),
            b"do not touch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn image_folder_rejects_symlink_to_external_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("images")).unwrap();

        let error = Filesystem::with_data_dir(&root)
            .image_folder()
            .expect_err("images symlink should not escape the data directory");
        assert!(matches!(error, FsError::Escape(_)));
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
