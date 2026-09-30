//! Workspace configuration and input limits.

use std::path::PathBuf;

use texrun_core::WorkspacePath;

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
    /// symlinks together; excluded entries are not counted here, but in
    /// [`WorkspaceLimits::max_scanned_entries`]).
    pub max_entries: u64,
    /// Maximum number of directory entries *examined*, including excluded
    /// ones (an excluded directory counts once; its contents are not read).
    /// Counted while a directory is being listed, so a huge directory is
    /// rejected before it is fully read into memory.
    pub max_scanned_entries: u64,
    /// Maximum path depth in components below the project root
    /// (`main.tex` has depth 1, `a/b/c.tex` depth 3).
    pub max_depth: usize,
}

impl WorkspaceLimits {
    /// Default [`WorkspaceLimits::max_total_bytes`]: 256 MiB.
    pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
    /// Default [`WorkspaceLimits::max_entries`]: 10 000.
    pub const DEFAULT_MAX_ENTRIES: u64 = 10_000;
    /// Default [`WorkspaceLimits::max_scanned_entries`]: 40 000
    /// (4 × [`WorkspaceLimits::DEFAULT_MAX_ENTRIES`]).
    pub const DEFAULT_MAX_SCANNED_ENTRIES: u64 = 4 * Self::DEFAULT_MAX_ENTRIES;
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

    /// Sets [`WorkspaceLimits::max_scanned_entries`].
    #[must_use]
    pub fn with_max_scanned_entries(mut self, entries: u64) -> Self {
        self.max_scanned_entries = entries;
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
            max_scanned_entries: Self::DEFAULT_MAX_SCANNED_ENTRIES,
            max_depth: Self::DEFAULT_MAX_DEPTH,
        }
    }
}

/// How a [`Workspace`](crate::Workspace) is created and cleaned up.
///
/// Name, extension and path-component matching is done on a folded form of
/// the name (Unicode NFKC + lowercase), so `LATEXM\u{212A}RC` (with a Kelvin sign)
/// matches `latexmkrc`. In addition, after each entry is created in the
/// workspace, the workspace filesystem itself is asked whether the new entry
/// is reachable under one of the protected names or path components (e.g.
/// because the
/// filesystem is case- or normalization-insensitive); such entries are
/// removed again and reported as excluded.
///
/// `#[non_exhaustive]`: construct with [`WorkspaceConfig::default`] and the
/// `with_*` methods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkspaceConfig {
    /// Input size limits.
    pub limits: WorkspaceLimits,
    /// File or directory names that are never copied, at any depth.
    /// Defaults to [`WorkspaceConfig::DEFAULT_EXCLUDED_NAMES`].
    ///
    /// Tool configuration files ([`TOOL_CONFIG_NAMES`](crate::TOOL_CONFIG_NAMES),
    /// e.g. `latexmkrc`) are excluded unconditionally, in addition to this
    /// list.
    pub excluded_names: Vec<String>,
    /// Names that are not copied when they appear directly in the project
    /// root. Defaults to [`WorkspaceConfig::DEFAULT_EXCLUDED_ROOT_NAMES`].
    pub excluded_root_names: Vec<String>,
    /// File extensions (without the dot) of files and symlinks that are
    /// never copied, at any depth. Directories are not matched (a
    /// `data.base/` directory is copied). Defaults to
    /// [`WorkspaceConfig::DEFAULT_EXCLUDED_EXTENSIONS`].
    pub excluded_extensions: Vec<String>,
    /// Paths relative to the project root that are never copied (with
    /// everything below them), e.g. a nested output directory such as
    /// `build/pdf`. Empty by default.
    ///
    /// Each component is matched like a name in
    /// [`WorkspaceConfig::excluded_names`] (folded, and checked again on the
    /// workspace filesystem), so `Build/PDF` also matches `build/pdf`. Only
    /// the path itself is excluded: its ancestors (`build/`) and siblings
    /// (`build/figures/`) are copied, and a file named like one of its
    /// ancestors is not affected. Excluded entries are reported as
    /// [`ExclusionReason::ExcludedPath`](crate::ExclusionReason::ExcludedPath);
    /// an entrypoint at or below one of these paths is rejected.
    pub excluded_paths: Vec<WorkspacePath>,
    /// Keep the workspace directory on drop instead of deleting it (for
    /// debugging), and also when creating it fails. Can also be changed
    /// later with [`Workspace::set_keep`](crate::Workspace::set_keep).
    pub keep: bool,
    /// Directory in which workspaces are created. `None` uses
    /// [`std::env::temp_dir`].
    pub temp_parent: Option<PathBuf>,
}

impl WorkspaceConfig {
    /// Default [`WorkspaceConfig::excluded_names`]: version control metadata
    /// and texrun's own state / output directory.
    pub const DEFAULT_EXCLUDED_NAMES: &'static [&'static str] = &[".git", ".hg", ".svn", ".texrun"];
    /// Default [`WorkspaceConfig::excluded_root_names`]: Cargo-style build
    /// output. Only at the root, so e.g. `figures/target/` is kept.
    pub const DEFAULT_EXCLUDED_ROOT_NAMES: &'static [&'static str] = &["target"];
    /// Default [`WorkspaceConfig::excluded_extensions`]: precompiled formats
    /// (TeX `.fmt`, Metafont `.base`, `MetaPost` `.mem`). A format in the
    /// working directory takes precedence over the installed one (e.g. via a
    /// `%&name` first line) and may carry engine-specific code such as Lua
    /// bytecode.
    ///
    /// `.base` and `.mem` are also used by unrelated files; such files are
    /// left out too (listed in the [`MaterializeReport`](crate::MaterializeReport)
    /// as [`ExclusionReason::ExcludedExtension`](crate::ExclusionReason::ExcludedExtension)).
    /// Replace the list with [`WorkspaceConfig::with_excluded_extensions`]
    /// if a project needs them.
    pub const DEFAULT_EXCLUDED_EXTENSIONS: &'static [&'static str] = &["fmt", "base", "mem"];

    /// Sets the input limits.
    #[must_use]
    pub fn with_limits(mut self, limits: WorkspaceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Replaces [`WorkspaceConfig::excluded_names`].
    #[must_use]
    pub fn with_excluded_names<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.excluded_names = names.into_iter().map(Into::into).collect();
        self
    }

    /// Replaces [`WorkspaceConfig::excluded_root_names`].
    #[must_use]
    pub fn with_excluded_root_names<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.excluded_root_names = names.into_iter().map(Into::into).collect();
        self
    }

    /// Replaces [`WorkspaceConfig::excluded_extensions`].
    #[must_use]
    pub fn with_excluded_extensions<I, S>(mut self, extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.excluded_extensions = extensions.into_iter().map(Into::into).collect();
        self
    }

    /// Replaces [`WorkspaceConfig::excluded_paths`].
    #[must_use]
    pub fn with_excluded_paths<I>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = WorkspacePath>,
    {
        self.excluded_paths = paths.into_iter().collect();
        self
    }

    /// Sets whether the workspace is kept on drop (and on failure).
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

fn owned(names: &[&str]) -> Vec<String> {
    names.iter().map(|&s| s.to_owned()).collect()
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            limits: WorkspaceLimits::default(),
            excluded_names: owned(Self::DEFAULT_EXCLUDED_NAMES),
            excluded_root_names: owned(Self::DEFAULT_EXCLUDED_ROOT_NAMES),
            excluded_extensions: owned(Self::DEFAULT_EXCLUDED_EXTENSIONS),
            excluded_paths: Vec::new(),
            keep: false,
            temp_parent: None,
        }
    }
}
