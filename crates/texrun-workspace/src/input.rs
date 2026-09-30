//! The host-side description of what to compile.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use texrun_core::WorkspacePath;

use crate::error::WorkspaceError;
use crate::fsutil::FileId;

/// A project on the host: a root directory and an entrypoint inside it.
///
/// Everything below [`ProjectInput::root`] (minus exclusions) is copied into
/// the workspace; nothing outside it is. Constructors check that the
/// entrypoint exists, is a regular file and — after resolving symlinks —
/// lies inside the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInput {
    root: PathBuf,
    /// Identity of the root when this value was created; re-checked when
    /// the workspace is created so a swapped root is detected.
    root_id: FileId,
    entrypoint: WorkspacePath,
}

impl ProjectInput {
    /// A project at `root` with `entrypoint` given relative to it (e.g. as
    /// received from an API). The entrypoint is validated lexically first, so
    /// absolute paths and `..` are rejected with
    /// [`WorkspaceError::InvalidEntrypoint`].
    pub fn new(root: impl AsRef<Path>, entrypoint: &str) -> Result<Self, WorkspaceError> {
        let entrypoint =
            WorkspacePath::new(entrypoint).map_err(|source| WorkspaceError::InvalidEntrypoint {
                input: entrypoint.to_owned(),
                source,
            })?;
        let (root, root_id) = canonical_root(root.as_ref())?;
        check_entrypoint(&root, &entrypoint)?;
        Ok(Self {
            root,
            root_id,
            entrypoint,
        })
    }

    /// A project for a host entrypoint path such as the CLI's `main.tex`
    /// argument (relative to the current directory, or absolute).
    ///
    /// The root defaults to the directory containing the entrypoint. An
    /// explicit `root` (e.g. `--root`) must contain the entrypoint, otherwise
    /// [`WorkspaceError::EntrypointOutsideRoot`] is returned. The entrypoint's
    /// own name is kept even if it is a symlink (it must still resolve inside
    /// the root).
    pub fn from_host_entrypoint(
        entrypoint: &Path,
        root: Option<&Path>,
    ) -> Result<Self, WorkspaceError> {
        let not_found = || WorkspaceError::EntrypointNotFound(entrypoint.to_path_buf());
        let name = entrypoint.file_name().ok_or_else(not_found)?;
        let parent = match entrypoint.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let parent = fs::canonicalize(parent).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => not_found(),
            _ => WorkspaceError::io("resolving", parent)(e),
        })?;
        let (root, root_id) = canonical_root(root.unwrap_or(parent.as_path()))?;
        let rel_dir =
            parent
                .strip_prefix(&root)
                .map_err(|_| WorkspaceError::EntrypointOutsideRoot {
                    entrypoint: entrypoint.to_path_buf(),
                    root: root.clone(),
                })?;
        let rel = rel_dir.join(name);
        let entry =
            WorkspacePath::from_path(&rel).map_err(|source| WorkspaceError::InvalidEntrypoint {
                input: rel.to_string_lossy().into_owned(),
                source,
            })?;
        check_entrypoint(&root, &entry)?;
        Ok(Self {
            root,
            root_id,
            entrypoint: entry,
        })
    }

    /// The canonical (absolute, symlink-free) project root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The entrypoint, relative to the root.
    pub fn entrypoint(&self) -> &WorkspacePath {
        &self.entrypoint
    }

    pub(crate) fn root_id(&self) -> FileId {
        self.root_id
    }
}

fn canonical_root(root: &Path) -> Result<(PathBuf, FileId), WorkspaceError> {
    match fs::canonicalize(root) {
        Ok(p) => match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => Ok((p, FileId::of_metadata(&m))),
            Ok(_) => Err(WorkspaceError::RootNotDirectory(root.to_path_buf())),
            Err(e) => Err(WorkspaceError::io("inspecting", &p)(e)),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(WorkspaceError::RootNotDirectory(root.to_path_buf()))
        }
        Err(e) => Err(WorkspaceError::io("resolving", root)(e)),
    }
}

fn check_entrypoint(root: &Path, entry: &WorkspacePath) -> Result<(), WorkspaceError> {
    let host = root.join(entry.as_path());
    let resolved = fs::canonicalize(&host).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => WorkspaceError::EntrypointNotFound(host.clone()),
        _ => WorkspaceError::io("resolving", &host)(e),
    })?;
    if !resolved.starts_with(root) {
        return Err(WorkspaceError::EntrypointOutsideRoot {
            entrypoint: host,
            root: root.to_path_buf(),
        });
    }
    if !resolved.is_file() {
        return Err(WorkspaceError::EntrypointNotFile(host));
    }
    Ok(())
}
