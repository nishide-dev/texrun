//! Resolving and creating the host output directory.
//!
//! The default output directory (`texrun-out/` next to the entrypoint) lies
//! inside the project, which is untrusted input (docs/security.md §1), and
//! an explicit `--output` may point into it too. So the path is walked one
//! component at a time without following symlinks that are located inside
//! the project root: such a symlink (or a non-directory in the way) is
//! refused. Symlinks outside the project (e.g. `/tmp` on macOS, or a link
//! the user created for `--output`) are followed; that part of the path is
//! the user's choice.
//!
//! Missing components are created one at a time with `create_dir` and
//! checked again after creation. Entries *below* the returned directory are
//! checked by the workspace layer when artifacts are copied.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Why the output directory cannot be used.
#[derive(Debug)]
pub enum OutputDirError {
    /// A symlink inside the project root is in the way.
    Symlink(PathBuf),
    /// A file (or other non-directory) is in the way.
    NotDirectory(PathBuf),
    /// An I/O error at `path`.
    Io(&'static str, PathBuf, io::Error),
}

/// Walks `output` (relative to the current directory, or absolute),
/// refusing symlinks inside `project_root` (canonical). With `create`,
/// missing directories are created and the canonical path of the output
/// directory is returned; without, the walk stops at the first missing
/// component and returns `None` (a check before compiling).
pub fn walk(
    output: &Path,
    project_root: &Path,
    create: bool,
) -> Result<Option<PathBuf>, OutputDirError> {
    let absolute = std::path::absolute(output)
        .map_err(|e| OutputDirError::Io("resolving", output.to_path_buf(), e))?;
    let mut current = PathBuf::from("/");
    for component in absolute.components() {
        let name = match component {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => continue,
            Component::ParentDir => {
                // `current` is a real directory path, so its parent is the
                // real parent.
                current.pop();
                continue;
            }
            Component::Normal(name) => name,
        };
        let next = current.join(name);
        match fs::symlink_metadata(&next) {
            Ok(meta) if meta.is_dir() => current = real_dir(&current, next)?,
            Ok(meta) if meta.file_type().is_symlink() => {
                if current.starts_with(project_root) {
                    return Err(OutputDirError::Symlink(next));
                }
                let target = fs::canonicalize(&next)
                    .map_err(|e| OutputDirError::Io("resolving", next.clone(), e))?;
                if !target.is_dir() {
                    return Err(OutputDirError::NotDirectory(next));
                }
                current = target;
            }
            Ok(_) => return Err(OutputDirError::NotDirectory(next)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                match fs::create_dir(&next) {
                    Ok(()) => {}
                    // Created concurrently: inspected below like any entry.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(OutputDirError::Io("creating directory", next, e)),
                }
                match fs::symlink_metadata(&next) {
                    Ok(meta) if meta.is_dir() => current = real_dir(&current, next)?,
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(OutputDirError::Symlink(next));
                    }
                    Ok(_) => return Err(OutputDirError::NotDirectory(next)),
                    Err(e) => return Err(OutputDirError::Io("inspecting", next, e)),
                }
            }
            Err(e) => return Err(OutputDirError::Io("inspecting", next, e)),
        }
    }
    Ok(Some(current))
}

/// The canonical spelling of `next`, a directory just seen (not a symlink)
/// in the directory `parent` (canonical).
///
/// The walk compares paths with the project root, which is canonical. On a
/// case- or normalization-insensitive filesystem (default APFS) the path as
/// given may be spelled differently from the stored names, so every
/// component is replaced with its stored spelling. If `next` was replaced
/// by a symlink in the meantime, its canonical path is somewhere else and
/// it is refused.
fn real_dir(parent: &Path, next: PathBuf) -> Result<PathBuf, OutputDirError> {
    let real =
        fs::canonicalize(&next).map_err(|e| OutputDirError::Io("resolving", next.clone(), e))?;
    if real.parent() != Some(parent) {
        return Err(OutputDirError::Symlink(next));
    }
    match fs::symlink_metadata(&real) {
        Ok(meta) if meta.is_dir() => Ok(real),
        Ok(meta) if meta.file_type().is_symlink() => Err(OutputDirError::Symlink(next)),
        Ok(_) => Err(OutputDirError::NotDirectory(next)),
        Err(e) => Err(OutputDirError::Io("inspecting", real, e)),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(base.join("proj/src")).unwrap();
        fs::create_dir_all(base.join("elsewhere")).unwrap();
        (dir, base)
    }

    #[test]
    fn creates_missing_directories() {
        let (_dir, base) = setup();
        let root = base.join("proj");
        let out = walk(&root.join("src/texrun-out/a"), &root, true)
            .unwrap()
            .unwrap();
        assert_eq!(out, root.join("src/texrun-out/a"));
        assert!(out.is_dir());
        assert_eq!(walk(&root.join("new"), &root, false).unwrap(), None);
        assert!(!root.join("new").exists());
    }

    #[test]
    fn refuses_symlinks_inside_the_project() {
        let (_dir, base) = setup();
        let root = base.join("proj");
        symlink(base.join("elsewhere"), root.join("src/texrun-out")).unwrap();
        for create in [false, true] {
            assert!(matches!(
                walk(&root.join("src/texrun-out"), &root, create),
                Err(OutputDirError::Symlink(p)) if p == root.join("src/texrun-out")
            ));
            assert!(matches!(
                walk(&root.join("src/texrun-out/deeper"), &root, create),
                Err(OutputDirError::Symlink(_))
            ));
        }
        assert!(
            fs::read_dir(base.join("elsewhere"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn refuses_files_in_the_way() {
        let (_dir, base) = setup();
        let root = base.join("proj");
        fs::write(root.join("file"), "x").unwrap();
        assert!(matches!(
            walk(&root.join("file"), &root, true),
            Err(OutputDirError::NotDirectory(_))
        ));
    }

    #[test]
    fn follows_symlinks_outside_the_project() {
        let (_dir, base) = setup();
        let root = base.join("proj");
        symlink(base.join("elsewhere"), base.join("link")).unwrap();
        let out = walk(&base.join("link/out"), &root, true).unwrap().unwrap();
        assert_eq!(out, base.join("elsewhere/out"));
        // A link outside that points into the project is followed, but the
        // part inside is checked.
        symlink(root.join("src"), base.join("into")).unwrap();
        symlink(base.join("elsewhere"), root.join("src/evil")).unwrap();
        assert!(matches!(
            walk(&base.join("into/evil"), &root, true),
            Err(OutputDirError::Symlink(_))
        ));
    }

    #[test]
    fn parent_components_are_resolved() {
        let (_dir, base) = setup();
        let root = base.join("proj");
        let out = walk(&root.join("src/../out"), &root, true)
            .unwrap()
            .unwrap();
        assert_eq!(out, root.join("out"));
    }

    /// Default APFS matches names case- and normalization-insensitively; a
    /// differently spelled path must still be recognized as inside the
    /// project, so the symlink below is refused, not followed.
    #[cfg(target_os = "macos")]
    #[test]
    fn differently_spelled_paths_are_still_inside_the_project() {
        let (_dir, base) = setup();
        let root = base.join("proj");
        symlink(base.join("elsewhere"), root.join("src/texrun-out")).unwrap();

        // Case: the path as given says `PROJ/SRC`.
        let upper = base.join("PROJ/SRC/texrun-out");
        assert!(upper.exists(), "the test expects a case-insensitive APFS");
        for create in [false, true] {
            assert!(matches!(
                walk(&upper, &root, create),
                Err(OutputDirError::Symlink(_))
            ));
        }

        // Normalization: the project directory is stored in NFC, the path
        // as given uses NFD.
        let nfc = base.join("caf\u{e9}");
        fs::create_dir_all(nfc.join("src")).unwrap();
        symlink(base.join("elsewhere"), nfc.join("src/texrun-out")).unwrap();
        let nfd = base.join("cafe\u{301}/src/texrun-out");
        assert!(
            nfd.exists(),
            "the test expects a normalization-insensitive APFS"
        );
        for create in [false, true] {
            assert!(matches!(
                walk(&nfd, &nfc, create),
                Err(OutputDirError::Symlink(_))
            ));
        }
        assert!(
            fs::read_dir(base.join("elsewhere"))
                .unwrap()
                .next()
                .is_none()
        );

        // A differently spelled path to a real directory resolves to the
        // stored spelling.
        let out = walk(&base.join("PROJ/SRC"), &root, true).unwrap().unwrap();
        assert_eq!(out, root.join("src"));
    }
}
