//! Workspace errors.

use std::fmt;
use std::io;
use std::path::PathBuf;

use texrun_core::{EngineError, WorkspacePath, WorkspacePathError};

/// Which [`WorkspaceLimits`](crate::WorkspaceLimits) bound was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Limit {
    /// [`WorkspaceLimits::max_total_bytes`](crate::WorkspaceLimits::max_total_bytes).
    TotalBytes,
    /// [`WorkspaceLimits::max_entries`](crate::WorkspaceLimits::max_entries).
    Entries,
    /// [`WorkspaceLimits::max_depth`](crate::WorkspaceLimits::max_depth).
    Depth,
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TotalBytes => "total size (bytes)",
            Self::Entries => "entry count",
            Self::Depth => "path depth",
        })
    }
}

/// Failure to prepare a workspace or to collect its outputs.
///
/// Paths of project entries are reported relative to the project root.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// The project root does not exist or is not a directory.
    #[error("project root {0:?} is not a directory")]
    RootNotDirectory(PathBuf),
    /// The entrypoint is not a valid workspace-relative path (absolute,
    /// contains `..`, forbidden characters, ...).
    #[error("invalid entrypoint {input:?}")]
    InvalidEntrypoint {
        /// The rejected input.
        input: String,
        /// Why it was rejected.
        #[source]
        source: WorkspacePathError,
    },
    /// The entrypoint does not exist.
    #[error("entrypoint {0:?} does not exist")]
    EntrypointNotFound(PathBuf),
    /// The entrypoint (or what it resolves to) is outside the project root.
    #[error("entrypoint {entrypoint:?} is outside the project root {root:?}")]
    EntrypointOutsideRoot {
        /// The entrypoint as given (or resolved).
        entrypoint: PathBuf,
        /// The project root.
        root: PathBuf,
    },
    /// The entrypoint is not a regular file.
    #[error("entrypoint {0:?} is not a regular file")]
    EntrypointNotFile(PathBuf),
    /// The entrypoint lies in a location that is not copied into the
    /// workspace (an excluded name, `latexmkrc`, the output directory).
    #[error("entrypoint `{0}` is excluded from the workspace")]
    EntrypointExcluded(WorkspacePath),
    /// The compile request built for the workspace is invalid
    /// (see [`CompileRequest::validate`](texrun_core::CompileRequest::validate)).
    #[error(transparent)]
    InvalidRequest(EngineError),
    /// A symlink in the project points outside the project root.
    #[error("symlink {link:?} points outside the project root (target {target:?})")]
    SymlinkOutsideRoot {
        /// The symlink, relative to the project root.
        link: PathBuf,
        /// Its target as stored in the link.
        target: PathBuf,
    },
    /// The project contains a symlink, which is not supported on this
    /// platform.
    #[error("symlinks are not supported on this platform: {0:?}")]
    SymlinkUnsupported(PathBuf),
    /// The project exceeds an input limit.
    #[error("project exceeds the {limit} limit of {max}")]
    LimitExceeded {
        /// The exceeded bound.
        limit: Limit,
        /// Its configured value.
        max: u64,
    },
    /// A file changed (e.g. was replaced by a symlink) while it was being
    /// copied.
    #[error("input {0:?} changed while it was being copied")]
    InputChanged(PathBuf),
    /// An artifact reported by the engine is not in the output directory.
    #[error("artifact `{0}` was not found in the output directory")]
    ArtifactMissing(WorkspacePath),
    /// An artifact is not a regular file inside the output directory (e.g. a
    /// symlink or directory).
    #[error("artifact `{0}` is not a regular file in the output directory")]
    ArtifactNotFile(WorkspacePath),
    /// The destination already has a file of that name and
    /// [`OverwritePolicy::Refuse`](crate::OverwritePolicy::Refuse) is in
    /// effect.
    #[error("output {0:?} already exists")]
    OutputExists(PathBuf),
    /// A destination path is a symlink or other non-regular entry, which
    /// texrun refuses to write through or replace.
    #[error("refusing to write to {0:?}: it is a symlink or not a regular file / directory")]
    UnsafeOutputPath(PathBuf),
    /// An I/O error.
    #[error("I/O error while {context} {path:?}")]
    Io {
        /// What was being done, e.g. `"copying"`.
        context: &'static str,
        /// The path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

impl WorkspaceError {
    pub(crate) fn io(
        context: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io {
            context,
            path,
            source,
        }
    }
}
