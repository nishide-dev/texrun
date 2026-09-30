//! Copying artifacts from a workspace's output directory to the host.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use texrun_core::Artifact;

use crate::error::WorkspaceError;

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
    let out_canon =
        fs::canonicalize(out_root).map_err(WorkspaceError::io("resolving", out_root))?;
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
        let src = out_root.join(artifact.path.as_path());
        match fs::symlink_metadata(&src) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(WorkspaceError::ArtifactNotFile(artifact.path.clone())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(WorkspaceError::ArtifactMissing(artifact.path.clone()));
            }
            Err(e) => return Err(WorkspaceError::io("inspecting", &src)(e)),
        }
        // Intermediate directories must not lead out of the output dir.
        let resolved = fs::canonicalize(&src).map_err(WorkspaceError::io("resolving", &src))?;
        if !resolved.starts_with(&out_canon) {
            return Err(WorkspaceError::ArtifactNotFile(artifact.path.clone()));
        }

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

        let size = copy_atomically(&src, &dir, &target, policy)?;
        collected.push(artifact.clone().with_size_bytes(size));
    }
    Ok(collected)
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
    src: &Path,
    dir: &Path,
    target: &Path,
    policy: OverwritePolicy,
) -> Result<u64, WorkspaceError> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".texrun-collect-");
    #[cfg(unix)]
    let perms = {
        use std::os::unix::fs::PermissionsExt;
        // Subject to the umask, like a normally created file.
        fs::Permissions::from_mode(0o666)
    };
    #[cfg(unix)]
    builder.permissions(perms);
    let mut tmp = builder
        .tempfile_in(dir)
        .map_err(WorkspaceError::io("creating a temporary file in", dir))?;
    let mut input = File::open(src).map_err(WorkspaceError::io("opening", src))?;
    let size =
        io::copy(&mut input, tmp.as_file_mut()).map_err(WorkspaceError::io("copying", src))?;

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
