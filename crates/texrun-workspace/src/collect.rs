//! Copying artifacts from a workspace's output directory to the host.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode};
use rustix::io::Errno;
use texrun_core::{Artifact, WorkspacePath};

use crate::error::WorkspaceError;
use crate::fsutil::{DIR_FLAGS, READ_FLAGS, file_type, fstat};

/// What to do when the destination already has a file with an artifact's
/// name.
///
/// Independently of the policy, texrun never writes through or replaces a
/// symlink (or a directory) at the destination: that is
/// [`WorkspaceError::UnsafeOutputPath`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OverwritePolicy {
    /// Atomically replace existing regular files (the usual "rebuild"
    /// behaviour).
    #[default]
    Replace,
    /// Fail with [`WorkspaceError::OutputExists`] and leave the existing file
    /// untouched.
    Refuse,
}

/// Copies `artifacts` (relative to `out_root`) into `dest`, preserving their
/// relative paths. See [`Workspace::collect_artifacts`](crate::Workspace::collect_artifacts).
pub(crate) fn collect(
    out_root: &Path,
    artifacts: &[Artifact],
    dest: &Path,
    policy: OverwritePolicy,
) -> Result<Vec<Artifact>, WorkspaceError> {
    let out_fd = rustix::fs::open(out_root, DIR_FLAGS, Mode::empty())
        .map_err(|e| WorkspaceError::io("opening", out_root)(e.into()))?;
    // The destination root is chosen by the caller and may itself be a
    // symlink to a directory; only entries *below* it are checked strictly.
    fs::create_dir_all(dest).map_err(WorkspaceError::io("creating directory", dest))?;
    if !dest.is_dir() {
        return Err(WorkspaceError::UnsafeOutputPath(dest.to_path_buf()));
    }

    let mut seen = HashSet::new();
    let mut collected = Vec::new();
    for artifact in artifacts {
        if !seen.insert(&artifact.path) {
            continue;
        }
        let input = open_artifact(&out_fd, &artifact.path)?;

        let mut dir = dest.to_path_buf();
        if let Some(parent) = artifact.path.as_path().parent() {
            for comp in parent {
                dir.push(comp);
                ensure_dir(&dir)?;
            }
        }
        let target = dir.join(artifact.path.file_name());
        match fs::symlink_metadata(&target) {
            Ok(m) if m.is_file() => {
                if policy == OverwritePolicy::Refuse {
                    return Err(WorkspaceError::OutputExists(target));
                }
            }
            Ok(_) => return Err(WorkspaceError::UnsafeOutputPath(target)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(WorkspaceError::io("inspecting", &target)(e)),
        }

        let size = copy_atomically(input, &artifact.path, &dir, &target, policy)?;
        collected.push(artifact.clone().with_size_bytes(size));
    }
    Ok(collected)
}

/// Opens the artifact at `path` below the output directory `out_fd`, one
/// component at a time with `O_NOFOLLOW`, and checks with `fstat` that the
/// opened object is a regular file. A symlink anywhere on the way (even one
/// swapped in by a leftover process) is refused, so nothing outside the
/// output directory can be copied to the host.
fn open_artifact(out_fd: &OwnedFd, path: &WorkspacePath) -> Result<File, WorkspaceError> {
    let fail = |e: Errno| match e {
        Errno::NOENT => WorkspaceError::ArtifactMissing(path.clone()),
        Errno::LOOP | Errno::MLINK | Errno::NOTDIR => WorkspaceError::ArtifactNotFile(path.clone()),
        e => WorkspaceError::io("opening", path.as_path())(e.into()),
    };
    let comps: Vec<&str> = path.as_str().split('/').collect();
    let (file, dirs) = comps.split_last().expect("workspace paths are non-empty");
    let mut dir: Option<OwnedFd> = None;
    for comp in dirs {
        let parent = dir.as_ref().unwrap_or(out_fd);
        dir = Some(rustix::fs::openat(parent, *comp, DIR_FLAGS, Mode::empty()).map_err(fail)?);
    }
    let parent = dir.as_ref().unwrap_or(out_fd);
    let fd = rustix::fs::openat(parent, *file, READ_FLAGS, Mode::empty()).map_err(fail)?;
    let st = fstat(&fd).map_err(WorkspaceError::io("inspecting", path.as_path()))?;
    if file_type(&st) != FileType::RegularFile {
        return Err(WorkspaceError::ArtifactNotFile(path.clone()));
    }
    Ok(File::from(fd))
}

/// Makes sure `dir` is a real directory (creating it if missing), refusing
/// symlinks and non-directories.
fn ensure_dir(dir: &Path) -> Result<(), WorkspaceError> {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(WorkspaceError::UnsafeOutputPath(dir.to_path_buf())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir_all(dir) {
            Ok(()) => ensure_existing_dir(dir),
            Err(e) => Err(WorkspaceError::io("creating directory", dir)(e)),
        },
        Err(e) => Err(WorkspaceError::io("inspecting", dir)(e)),
    }
}

fn ensure_existing_dir(dir: &Path) -> Result<(), WorkspaceError> {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(WorkspaceError::UnsafeOutputPath(dir.to_path_buf())),
        Err(e) => Err(WorkspaceError::io("inspecting", dir)(e)),
    }
}

/// Copies `src` to a temporary file in `dir` and renames it to `target`, so
/// readers never see a partial file and an existing symlink is never written
/// through.
fn copy_atomically(
    mut input: File,
    src: &WorkspacePath,
    dir: &Path,
    target: &Path,
    policy: OverwritePolicy,
) -> Result<u64, WorkspaceError> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".texrun-collect-");
    // Subject to the umask, like a normally created file.
    builder.permissions(fs::Permissions::from_mode(0o666));
    let mut tmp = builder
        .tempfile_in(dir)
        .map_err(WorkspaceError::io("creating a temporary file in", dir))?;
    let size = io::copy(&mut input, tmp.as_file_mut())
        .map_err(WorkspaceError::io("copying", src.as_path()))?;

    let persisted = match policy {
        OverwritePolicy::Replace => tmp.persist(target),
        OverwritePolicy::Refuse => tmp.persist_noclobber(target),
    };
    persisted.map_err(|e| {
        if e.error.kind() == io::ErrorKind::AlreadyExists {
            WorkspaceError::OutputExists(target.to_path_buf())
        } else {
            WorkspaceError::io("writing", PathBuf::from(target))(e.error)
        }
    })?;
    Ok(size)
}
