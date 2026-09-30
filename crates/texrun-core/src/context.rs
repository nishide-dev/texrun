//! Per-call runtime context passed to engines.
//!
//! Unlike [`CompileRequest`](crate::CompileRequest), which describes *what* to
//! compile and is serializable, [`CompileContext`] carries host-side runtime
//! state for one call: where the workspace lives, how to cancel, and where a
//! sandbox backend shows the workspace to the engine ([`PathMapping`]). New
//! runtime inputs are added as fields here rather than as new parameters of
//! [`TypesetEngine::compile`](crate::TypesetEngine::compile).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::path::WorkspacePath;
use crate::request::CompileOptions;

/// Reason a path was rejected as a [`WorkspaceRoot`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WorkspaceRootError {
    /// The path is not absolute.
    #[error("workspace root must be an absolute path: {0:?}")]
    NotAbsolute(PathBuf),
    /// The path has `.` or `..` components, or is the filesystem root
    /// (only checked for [`PathMapping::new`]).
    #[error("path must be a normalized absolute directory other than `/`: {0:?}")]
    NotNormalized(PathBuf),
}

/// The absolute host directory of a prepared compile workspace.
///
/// All [`WorkspacePath`]s in a request are resolved against it. This type only
/// checks that the path is absolute; creating, populating and cleaning up the
/// directory is the job of the workspace layer (#4), which is expected to be
/// the only producer of values of this type outside tests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// Wraps an absolute directory path.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, WorkspaceRootError> {
        let path = path.into();
        if path.is_absolute() {
            Ok(Self(path))
        } else {
            Err(WorkspaceRootError::NotAbsolute(path))
        }
    }

    /// The host directory.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Host path of a workspace-relative path.
    pub fn resolve(&self, path: &WorkspacePath) -> PathBuf {
        self.0.join(path.as_path())
    }

    /// Host path of the output directory selected by `options`.
    pub fn output_dir(&self, options: &CompileOptions) -> PathBuf {
        self.resolve(&options.output_dir)
    }
}

/// Where the engine's processes see the workspace when they do not run on
/// the host's filesystem view: a sandbox backend (#26) mounts the
/// [`WorkspaceRoot`] at [`PathMapping::guest_root`] inside the sandbox.
///
/// Requests and results never contain host or guest paths
/// ([`WorkspacePath`]s only), so they are the same with and without a
/// mapping. An engine uses the mapping for what it passes to its processes
/// (arguments, working directory, environment) and to interpret what they
/// report (e.g. absolute paths in the TeX log), while it keeps reading and
/// writing the workspace through the host path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathMapping {
    guest_root: PathBuf,
}

impl PathMapping {
    /// Maps the workspace root to `guest_root`, which must be absolute,
    /// without `.` / `..` components and not `/`.
    pub fn new(guest_root: impl Into<PathBuf>) -> Result<Self, WorkspaceRootError> {
        let guest_root = guest_root.into();
        if !guest_root.is_absolute() {
            return Err(WorkspaceRootError::NotAbsolute(guest_root));
        }
        let normal = guest_root
            .components()
            .skip(1)
            .all(|c| matches!(c, Component::Normal(_)));
        if !normal || guest_root.parent().is_none() {
            return Err(WorkspaceRootError::NotNormalized(guest_root));
        }
        Ok(Self { guest_root })
    }

    /// The workspace root as the engine's processes see it.
    pub fn guest_root(&self) -> &Path {
        &self.guest_root
    }

    /// Guest path of a workspace-relative path.
    pub fn to_guest(&self, path: &WorkspacePath) -> PathBuf {
        self.guest_root.join(path.as_path())
    }

    /// The workspace-relative path of `guest`, if it is below (or at) the
    /// guest root. Purely lexical.
    pub fn to_workspace_path(&self, guest: &Path) -> Option<WorkspacePath> {
        let rel = guest.strip_prefix(&self.guest_root).ok()?;
        WorkspacePath::from_path(rel).ok()
    }
}

/// A cooperative cancellation flag shared between a caller and an engine.
///
/// The caller (e.g. a Ctrl-C handler in the CLI) calls [`CancelToken::cancel`];
/// the engine polls [`CancelToken::is_cancelled`] while waiting on its process
/// and, when set, stops it (for TeX Live: kills the process group, #5) and
/// returns a [`CompileOutcome::Cancelled`](crate::CompileOutcome::Cancelled)
/// result. Clones share the same flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Creates a token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Runtime context for one [`TypesetEngine::compile`](crate::TypesetEngine::compile)
/// call. Not serialized.
///
/// `#[non_exhaustive]`: construct with [`CompileContext::new`] and the `with_*`
/// methods, so that later additions do not break engines or callers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CompileContext<'a> {
    /// The prepared workspace.
    pub workspace: &'a WorkspaceRoot,
    /// Cancellation flag; never cancelled unless the caller supplies one.
    pub cancel: CancelToken,
    /// Where the engine's processes see the workspace, if not at its host
    /// path. `None` (the default): at [`WorkspaceRoot::path`].
    ///
    /// A sandbox backend sets it for the engine it drives, or uses the one
    /// a caller set to choose the mount point. An engine that runs its
    /// processes on the host rejects a context with a mapping
    /// ([`EngineError::Unsupported`](crate::EngineError::Unsupported)).
    pub path_mapping: Option<PathMapping>,
}

impl<'a> CompileContext<'a> {
    /// Creates a context for `workspace` with a fresh, never-cancelled token.
    pub fn new(workspace: &'a WorkspaceRoot) -> Self {
        Self {
            workspace,
            cancel: CancelToken::new(),
            path_mapping: None,
        }
    }

    /// Uses `cancel` as the cancellation flag.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Sets [`CompileContext::path_mapping`].
    #[must_use]
    pub fn with_path_mapping(mut self, mapping: PathMapping) -> Self {
        self.path_mapping = Some(mapping);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(p: &str) -> PathBuf {
        std::env::temp_dir().join(p)
    }

    #[test]
    fn workspace_root_requires_absolute_path() {
        assert_eq!(
            WorkspaceRoot::new("relative/dir"),
            Err(WorkspaceRootError::NotAbsolute("relative/dir".into()))
        );
        let root = WorkspaceRoot::new(abs("ws")).unwrap();
        assert_eq!(root.path(), abs("ws"));
    }

    #[test]
    fn workspace_root_resolves_relative_paths() {
        let root = WorkspaceRoot::new(abs("ws")).unwrap();
        let p = WorkspacePath::new("chapters/intro.tex").unwrap();
        assert_eq!(
            root.resolve(&p),
            abs("ws").join("chapters").join("intro.tex")
        );
        assert_eq!(
            root.output_dir(&CompileOptions::default()),
            abs("ws").join(CompileOptions::DEFAULT_OUTPUT_DIR)
        );
    }

    #[test]
    fn path_mapping_maps_workspace_paths() {
        let mapping = PathMapping::new("/workspace").unwrap();
        let p = WorkspacePath::new("chapters/intro.tex").unwrap();
        assert_eq!(
            mapping.to_guest(&p),
            Path::new("/workspace/chapters/intro.tex")
        );
        assert_eq!(
            mapping.to_workspace_path(Path::new("/workspace/chapters/intro.tex")),
            Some(p)
        );
        assert_eq!(mapping.to_workspace_path(Path::new("/workspace2/x")), None);
        assert_eq!(mapping.to_workspace_path(Path::new("/etc/passwd")), None);
        for bad in ["relative", "/", "/a/../b"] {
            assert!(PathMapping::new(bad).is_err(), "{bad}");
        }
        let root = WorkspaceRoot::new(abs("ws")).unwrap();
        let ctx = CompileContext::new(&root);
        assert_eq!(ctx.path_mapping, None);
        let ctx = ctx.with_path_mapping(mapping.clone());
        assert_eq!(ctx.path_mapping, Some(mapping));
    }

    #[test]
    fn cancel_token_clones_share_state() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn context_defaults_to_uncancelled() {
        let root = WorkspaceRoot::new(abs("ws")).unwrap();
        let token = CancelToken::new();
        let ctx = CompileContext::new(&root);
        assert!(!ctx.cancel.is_cancelled());
        let ctx = ctx.with_cancel(token.clone());
        token.cancel();
        assert!(ctx.cancel.is_cancelled());
    }
}
