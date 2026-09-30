//! Isolated compile workspaces for texrun.
//!
//! An engine never runs in the user's project directory. Instead, for every
//! compile:
//!
//! 1. [`ProjectInput`] names the project root on the host (by default the
//!    directory containing the entrypoint) and the entrypoint inside it;
//! 2. [`Workspace::create`] copies the root into a fresh temporary directory
//!    (the *workspace*), leaving out VCS metadata, texrun's own output
//!    directory, tool configuration files such as `latexmkrc`
//!    ([`TOOL_CONFIG_NAMES`]), precompiled formats and anything configured in
//!    [`WorkspaceConfig`], and enforcing [`WorkspaceLimits`];
//! 3. the engine is handed [`Workspace::context`] and [`Workspace::request`]
//!    ([`texrun_core::CompileContext`] / [`texrun_core::CompileRequest`]) and
//!    only ever sees paths inside the workspace;
//! 4. [`Workspace::collect_artifacts`] copies the reported artifacts from the
//!    workspace output directory to a host directory, keeping their
//!    output-root-relative paths;
//! 5. dropping the [`Workspace`] deletes the directory, unless it was marked
//!    to be kept for debugging.
//!
//! # Symlinks
//!
//! A symlink whose target (after full resolution) is outside the project root
//! is an error ([`WorkspaceError::SymlinkOutsideRoot`]). A symlink whose
//! target is inside the root is recreated in the workspace as a *relative*
//! symlink to the same place, so nothing in the workspace points back to the
//! host project. Symlinks to excluded locations and dangling symlinks are
//! left out and reported in the [`MaterializeReport`].
//!
//! # Concurrent modification of the project
//!
//! The project is walked with directory file descriptors and `O_NOFOLLOW`
//! opens, and each opened object is checked against the `(dev, ino)` seen
//! when it was listed. Replacing a file or directory with a symlink while
//! the copy runs is detected ([`WorkspaceError::InputChanged`]) and cannot
//! make the walk leave the project root. The root itself is re-checked
//! against the identity recorded by [`ProjectInput`].
//!
//! # Name matching
//!
//! Excluded names and the components of excluded paths
//! ([`WorkspaceConfig::excluded_paths`], the output directory) are matched
//! on a folded form (Unicode NFKC + lowercase), and, as a second layer, the
//! workspace filesystem is asked after each entry is created whether it is
//! also reachable under a protected name
//! (case- or normalization-insensitive filesystems such as default APFS).
//! Such entries are removed and reported.
//!
//! # What this does not protect against
//!
//! - The workspace is a boundary for *input files*. It does not stop the TeX
//!   engine from reading absolute host paths (`\input{/etc/passwd}`); that
//!   is the job of the engine configuration and sandboxing (#5, #9).
//! - A regular file inside the root that is a **hard link** to a file
//!   elsewhere is copied like any other file (its content is readable by
//!   the user anyway, and hard links cannot be told apart from the
//!   original). Files with more than one link are listed in
//!   [`MaterializeReport::hardlinked`] for information.
//!
//! # Platform support
//!
//! Unix only (Linux, macOS): the implementation relies on `openat`-style
//! file descriptor APIs (via `rustix`, without `unsafe` code).
//!
//! This crate is separate from `texrun-core` because it touches the
//! filesystem, while the core only defines filesystem-independent types.

#[cfg(not(unix))]
compile_error!("texrun-workspace supports Unix hosts only");

mod collect;
mod config;
mod error;
mod fsutil;
mod input;
mod materialize;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use texrun_core::{Artifact, CompileContext, CompileOptions, CompileRequest, WorkspaceRoot};

pub use collect::OverwritePolicy;
pub use config::{WorkspaceConfig, WorkspaceLimits};
pub use error::{Limit, WorkspaceError, WorkspaceErrorKind};
pub use input::ProjectInput;
pub use materialize::{
    ExcludedEntry, ExclusionReason, MAX_RECORDED_ENTRIES, MaterializeReport, TOOL_CONFIG_NAMES,
};

use fsutil::{DIR_FLAGS, FileId, fstat};
use materialize::{Exclusions, Materializer};

/// Prefix of workspace directory names.
const DIR_PREFIX: &str = "texrun-ws-";

/// A materialized, texrun-owned compile workspace.
///
/// Each workspace is a uniquely named fresh directory, so concurrent compiles
/// never share one. The directory is deleted when the value is dropped
/// unless [`Workspace::set_keep`] (or [`WorkspaceConfig::keep`]) says
/// otherwise; use [`Workspace::close`] to observe cleanup errors.
#[derive(Debug)]
pub struct Workspace {
    /// `None` only after [`Workspace::close`] / drop.
    dir: Option<TempDir>,
    root: WorkspaceRoot,
    request: CompileRequest,
    report: MaterializeReport,
    keep: bool,
    /// Identity of the workspace directory, checked again when collecting.
    root_id: FileId,
}

impl Workspace {
    /// Copies `input` into a new workspace and prepares a request for its
    /// entrypoint with `options`.
    ///
    /// Fails before copying anything if the request is invalid
    /// ([`CompileRequest::validate`], e.g. the entrypoint is inside
    /// `options.output_dir`) or the entrypoint lies in an excluded location.
    /// On a later error the partially created directory is removed, unless
    /// [`WorkspaceConfig::keep`] is set: then it is kept and the error is
    /// wrapped in [`WorkspaceError::KeptAfterFailure`] with its path.
    ///
    /// The output directory (`options.output_dir`) is never copied from the
    /// project; it is created empty in the workspace.
    pub fn create(
        input: &ProjectInput,
        options: CompileOptions,
        config: &WorkspaceConfig,
    ) -> Result<Self, WorkspaceError> {
        let request = CompileRequest::new(input.entrypoint().clone()).with_options(options);
        request.validate().map_err(WorkspaceError::InvalidRequest)?;
        let exclusions = Exclusions::new(config, &request.options.output_dir);
        if exclusions.excludes_path(request.entrypoint.as_path(), false) {
            return Err(WorkspaceError::EntrypointExcluded(request.entrypoint));
        }

        let parent = config
            .temp_parent
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let dir = tempfile::Builder::new()
            .prefix(DIR_PREFIX)
            .tempdir_in(&parent)
            .map_err(WorkspaceError::io("creating a workspace in", &parent))?;
        // Canonical, so that it can be compared with the canonical project
        // root and engines see a symlink-free path (e.g. `/private/var` on
        // macOS).
        let path =
            fs::canonicalize(dir.path()).map_err(WorkspaceError::io("resolving", dir.path()))?;
        let root = WorkspaceRoot::new(path).expect("canonical paths are absolute");
        let root_id = FileId::of_metadata(
            &fs::symlink_metadata(root.path())
                .map_err(WorkspaceError::io("inspecting", root.path()))?,
        );

        let report = match materialize(input, &request, &exclusions, config, &root) {
            Ok(report) => report,
            Err(source) if config.keep => {
                let _ = dir.keep();
                return Err(WorkspaceError::KeptAfterFailure {
                    path: root.path().to_path_buf(),
                    source: Box::new(source),
                });
            }
            Err(e) => return Err(e),
        };

        Ok(Self {
            dir: Some(dir),
            root,
            request,
            report,
            keep: config.keep,
            root_id,
        })
    }

    /// The workspace directory, for [`CompileContext`].
    pub fn root(&self) -> &WorkspaceRoot {
        &self.root
    }

    /// Host path of the workspace directory.
    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// The request to pass to the engine: the entrypoint relative to the
    /// workspace root and the options given to [`Workspace::create`].
    pub fn request(&self) -> &CompileRequest {
        &self.request
    }

    /// A context for [`TypesetEngine::compile`](texrun_core::TypesetEngine::compile)
    /// with a fresh cancel token (replace it with
    /// [`CompileContext::with_cancel`]).
    pub fn context(&self) -> CompileContext<'_> {
        CompileContext::new(&self.root)
    }

    /// Host path of the workspace output directory (the output root that
    /// artifact paths are relative to).
    pub fn output_dir(&self) -> PathBuf {
        self.root.output_dir(&self.request.options)
    }

    /// What was copied and what was left out.
    pub fn report(&self) -> &MaterializeReport {
        &self.report
    }

    /// Whether the directory is kept when this value is dropped.
    pub fn is_kept(&self) -> bool {
        self.keep
    }

    /// Keeps (`true`) or deletes (`false`) the directory on drop. Keeping is
    /// meant for debugging a compile; the caller becomes responsible for
    /// removing the directory at [`Workspace::path`].
    pub fn set_keep(&mut self, keep: bool) {
        self.keep = keep;
    }

    /// Copies `artifacts` — as reported by the engine, relative to the
    /// workspace output directory — into the host directory `dest`
    /// (created if missing), preserving relative paths, so the returned
    /// artifacts' paths are valid relative to `dest`. Sizes are filled in.
    ///
    /// - Only listed artifacts are copied. Stale files of the same name in
    ///   the *input* project are never involved, since the output directory
    ///   is not copied from it.
    /// - Duplicates are detected by path only: a second artifact with an
    ///   already collected path is skipped (whatever its kind) and not
    ///   returned.
    /// - An existing file at the destination is handled per `policy`;
    ///   symlinks or directories in the way are always an error.
    /// - Artifacts that are missing, not regular files, or reached through a
    ///   symlink inside the output directory are errors. The output directory
    ///   itself is reached from the workspace root one component at a time
    ///   without following symlinks, and the workspace root must still be
    ///   the directory created by [`Workspace::create`].
    /// - Artifacts are copied one by one, each atomically. If one fails, the
    ///   ones before it **remain** in `dest`; nothing is rolled back.
    pub fn collect_artifacts(
        &self,
        artifacts: &[Artifact],
        dest: &Path,
        policy: OverwritePolicy,
    ) -> Result<Vec<Artifact>, WorkspaceError> {
        collect::collect(
            self.root.path(),
            self.root_id,
            &self.request.options.output_dir,
            artifacts,
            dest,
            policy,
        )
    }

    /// Ends the workspace now: deletes the directory and reports errors, or,
    /// if it is kept, leaves it in place and returns its path.
    pub fn close(mut self) -> io::Result<Option<PathBuf>> {
        let dir = self.dir.take().expect("present until close or drop");
        if self.keep {
            let _ = dir.keep();
            Ok(Some(self.root.path().to_path_buf()))
        } else {
            dir.close().map(|()| None)
        }
    }
}

/// Copies the project into the (empty) workspace and prepares the output
/// directory.
fn materialize(
    input: &ProjectInput,
    request: &CompileRequest,
    exclusions: &Exclusions,
    config: &WorkspaceConfig,
    root: &WorkspaceRoot,
) -> Result<MaterializeReport, WorkspaceError> {
    let open = |path: &Path| {
        rustix::fs::open(path, DIR_FLAGS, rustix::fs::Mode::empty())
            .map_err(|e| WorkspaceError::io("opening", path)(e.into()))
    };
    let src = open(input.root())?;
    let src_id = fstat(&src).map_err(WorkspaceError::io("inspecting", input.root()))?;
    if FileId::of(&src_id) != input.root_id() {
        return Err(WorkspaceError::InputChanged(input.root().to_path_buf()));
    }
    let dst = open(root.path())?;
    let report =
        Materializer::new(config.limits, exclusions, input.root(), root.path()).run(&src, &dst)?;

    let out = root.output_dir(&request.options);
    fs::create_dir_all(&out).map_err(WorkspaceError::io("creating directory", &out))?;
    // The entrypoint was checked on the host; make sure the copy is usable
    // too (e.g. it was not a symlink to an excluded file).
    if !root.resolve(&request.entrypoint).is_file() {
        return Err(WorkspaceError::EntrypointExcluded(
            request.entrypoint.clone(),
        ));
    }
    Ok(report)
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // Without `keep`, `TempDir`'s own drop removes the directory; errors
        // cannot be reported from `drop` (use `close` to see them).
        if let Some(dir) = self.dir.take()
            && self.keep
        {
            let _ = dir.keep();
        }
    }
}
