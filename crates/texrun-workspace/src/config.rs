//! Workspace configuration and input limits.

use std::path::PathBuf;

/// Upper bounds applied while copying a project into a workspace.
///
/// Exceeding any of them aborts materialization with
/// [`WorkspaceError::LimitExceeded`](crate::WorkspaceError::LimitExceeded).
/// The defaults are provisional and meant to be aligned with the execution
/// limits policy (#9, `docs/security.md`).
///
/// `#[non_exhaustive]`: construct with [`WorkspaceLimits::default`] and the
/// `with_*` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkspaceLimits {
    /// Maximum total size in bytes of all copied regular files.
    pub max_total_bytes: u64,
    /// Maximum number of copied entries (regular files, directories and
    /// symlinks together; excluded entries are not counted).
    pub max_entries: u64,
    /// Maximum path depth in components below the project root
    /// (`main.tex` has depth 1, `a/b/c.tex` depth 3).
    pub max_depth: usize,
}

impl WorkspaceLimits {
    /// Default [`WorkspaceLimits::max_total_bytes`]: 256 MiB.
    pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
    /// Default [`WorkspaceLimits::max_entries`]: 10 000.
    pub const DEFAULT_MAX_ENTRIES: u64 = 10_000;
    /// Default [`WorkspaceLimits::max_depth`]: 32.
    pub const DEFAULT_MAX_DEPTH: usize = 32;

    /// Sets [`WorkspaceLimits::max_total_bytes`].
    #[must_use]
    pub fn with_max_total_bytes(mut self, bytes: u64) -> Self {
        self.max_total_bytes = bytes;
        self
    }

    /// Sets [`WorkspaceLimits::max_entries`].
    #[must_use]
    pub fn with_max_entries(mut self, entries: u64) -> Self {
        self.max_entries = entries;
        self
    }

    /// Sets [`WorkspaceLimits::max_depth`].
    #[must_use]
    pub fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }
}

impl Default for WorkspaceLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: Self::DEFAULT_MAX_TOTAL_BYTES,
            max_entries: Self::DEFAULT_MAX_ENTRIES,
            max_depth: Self::DEFAULT_MAX_DEPTH,
        }
    }
}

/// How a [`Workspace`](crate::Workspace) is created and cleaned up.
///
/// `#[non_exhaustive]`: construct with [`WorkspaceConfig::default`] and the
/// `with_*` methods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkspaceConfig {
    /// Input size limits.
    pub limits: WorkspaceLimits,
    /// File or directory names that are never copied, at any depth. Matched
    /// against the whole name, ignoring ASCII case (so the result does not
    /// depend on whether the host filesystem is case-sensitive). Defaults to
    /// [`WorkspaceConfig::DEFAULT_EXCLUDED_NAMES`].
    ///
    /// `latexmkrc` / `.latexmkrc` are excluded unconditionally, in addition
    /// to this list (see [`LATEXMK_RC_NAMES`](crate::LATEXMK_RC_NAMES)).
    pub excluded_names: Vec<String>,
    /// Keep the workspace directory on drop instead of deleting it (for
    /// debugging). Can also be changed later with
    /// [`Workspace::set_keep`](crate::Workspace::set_keep).
    pub keep: bool,
    /// Directory in which workspaces are created. `None` uses
    /// [`std::env::temp_dir`].
    pub temp_parent: Option<PathBuf>,
}

impl WorkspaceConfig {
    /// Default [`WorkspaceConfig::excluded_names`]: version control metadata,
    /// texrun's own state / output directory and Cargo-style build output.
    pub const DEFAULT_EXCLUDED_NAMES: &'static [&'static str] =
        &[".git", ".hg", ".svn", ".texrun", "target"];

    /// Sets the input limits.
    #[must_use]
    pub fn with_limits(mut self, limits: WorkspaceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Replaces the excluded names.
    #[must_use]
    pub fn with_excluded_names<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.excluded_names = names.into_iter().map(Into::into).collect();
        self
    }

    /// Sets whether the workspace is kept on drop.
    #[must_use]
    pub fn with_keep(mut self, keep: bool) -> Self {
        self.keep = keep;
        self
    }

    /// Creates workspaces below `parent` instead of the system temp dir.
    #[must_use]
    pub fn with_temp_parent(mut self, parent: impl Into<PathBuf>) -> Self {
        self.temp_parent = Some(parent.into());
        self
    }
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            limits: WorkspaceLimits::default(),
            excluded_names: Self::DEFAULT_EXCLUDED_NAMES
                .iter()
                .map(|&s| s.to_owned())
                .collect(),
            keep: false,
            temp_parent: None,
        }
    }
}
