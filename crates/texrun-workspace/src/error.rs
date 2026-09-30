//! Workspace errors.

use std::fmt;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use texrun_core::{EngineError, WorkspacePath, WorkspacePathError};

/// Which [`WorkspaceLimits`](crate::WorkspaceLimits) bound was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Limit {
    /// [`WorkspaceLimits::max_total_bytes`](crate::WorkspaceLimits::max_total_bytes).
    TotalBytes,
    /// [`WorkspaceLimits::max_entries`](crate::WorkspaceLimits::max_entries).
    Entries,
    /// [`WorkspaceLimits::max_scanned_entries`](crate::WorkspaceLimits::max_scanned_entries).
    ScannedEntries,
    /// [`WorkspaceLimits::max_depth`](crate::WorkspaceLimits::max_depth).
    Depth,
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TotalBytes => "total size (bytes)",
            Self::Entries => "entry count",
            Self::ScannedEntries => "scanned entry count",
            Self::Depth => "path depth",
        })
    }
}

/// Failure to prepare a workspace or to collect its outputs.
///
/// Paths of project entries are reported relative to the project root. Use
/// [`WorkspaceError::kind`] for a stable, serializable classification.
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
    /// The project exceeds an input limit.
    #[error("project exceeds the {limit} limit of {max}")]
    LimitExceeded {
        /// The exceeded bound.
        limit: Limit,
        /// Its configured value.
        max: u64,
    },
    /// An entry changed (e.g. a file or directory was replaced by a symlink)
    /// while the project was being copied.
    #[error("input {0:?} changed while it was being copied")]
    InputChanged(PathBuf),
    /// An artifact reported by the engine is not in the output directory.
    #[error("artifact `{0}` was not found in the output directory")]
    ArtifactMissing(WorkspacePath),
    /// An artifact is not a regular file inside the output directory (e.g. a
    /// symlink or directory, or a path through a symlinked directory).
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
    /// Creating the workspace failed with `source`, and the partial
    /// workspace was kept at `path` because
    /// [`WorkspaceConfig::keep`](crate::WorkspaceConfig::keep) is set.
    /// [`WorkspaceError::kind`] is that of `source`.
    #[error("{source} (partial workspace kept at {path:?})")]
    KeptAfterFailure {
        /// The kept workspace directory.
        path: PathBuf,
        /// The actual failure.
        source: Box<WorkspaceError>,
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

    /// The error's classification (for exit codes and JSON error objects).
    pub fn kind(&self) -> WorkspaceErrorKind {
        use WorkspaceErrorKind as K;
        match self {
            Self::RootNotDirectory(_) => K::RootNotDirectory,
            Self::InvalidEntrypoint { .. } => K::InvalidEntrypoint,
            Self::EntrypointNotFound(_) => K::EntrypointNotFound,
            Self::EntrypointOutsideRoot { .. } => K::EntrypointOutsideRoot,
            Self::EntrypointNotFile(_) => K::EntrypointNotFile,
            Self::EntrypointExcluded(_) => K::EntrypointExcluded,
            Self::InvalidRequest(_) => K::InvalidRequest,
            Self::SymlinkOutsideRoot { .. } => K::SymlinkOutsideRoot,
            Self::LimitExceeded { .. } => K::LimitExceeded,
            Self::InputChanged(_) => K::InputChanged,
            Self::ArtifactMissing(_) => K::ArtifactMissing,
            Self::ArtifactNotFile(_) => K::ArtifactNotFile,
            Self::OutputExists(_) => K::OutputExists,
            Self::UnsafeOutputPath(_) => K::UnsafeOutputPath,
            Self::Io { .. } => K::Io,
            Self::KeptAfterFailure { source, .. } => source.kind(),
        }
    }
}

/// Serializable classification of a [`WorkspaceError`] (`snake_case` in JSON).
/// `WorkspaceError` itself is not serializable because it carries
/// `io::Error` sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
#[allow(missing_docs)] // Each variant mirrors the `WorkspaceError` variant of the same name.
pub enum WorkspaceErrorKind {
    RootNotDirectory,
    InvalidEntrypoint,
    EntrypointNotFound,
    EntrypointOutsideRoot,
    EntrypointNotFile,
    EntrypointExcluded,
    InvalidRequest,
    SymlinkOutsideRoot,
    LimitExceeded,
    InputChanged,
    ArtifactMissing,
    ArtifactNotFile,
    OutputExists,
    UnsafeOutputPath,
    Io,
}

impl WorkspaceErrorKind {
    /// Whether the error is caused by the given project, entrypoint or
    /// request (something the user can fix in their input), as opposed to
    /// the environment, concurrent modification, the engine's output or the
    /// destination directory.
    pub fn is_input_error(self) -> bool {
        matches!(
            self,
            Self::RootNotDirectory
                | Self::InvalidEntrypoint
                | Self::EntrypointNotFound
                | Self::EntrypointOutsideRoot
                | Self::EntrypointNotFile
                | Self::EntrypointExcluded
                | Self::InvalidRequest
                | Self::SymlinkOutsideRoot
                | Self::LimitExceeded
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_value(WorkspaceErrorKind::SymlinkOutsideRoot).unwrap(),
            serde_json::json!("symlink_outside_root")
        );
        assert_eq!(
            serde_json::to_value(Limit::TotalBytes).unwrap(),
            serde_json::json!("total_bytes")
        );
    }

    #[test]
    fn kept_after_failure_reports_the_inner_kind() {
        let inner = WorkspaceError::LimitExceeded {
            limit: Limit::Entries,
            max: 1,
        };
        assert!(inner.kind().is_input_error());
        let kept = WorkspaceError::KeptAfterFailure {
            path: "/tmp/x".into(),
            source: Box::new(inner),
        };
        assert_eq!(kept.kind(), WorkspaceErrorKind::LimitExceeded);
        assert!(kept.to_string().contains("/tmp/x"));
        let io = WorkspaceError::io("copying", "a")(io::Error::other("x"));
        assert!(!io.kind().is_input_error());
    }
}
