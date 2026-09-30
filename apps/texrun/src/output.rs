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
    /// A symlink inside the project root, or a non-directory, is in the way.
    Unsafe(PathBuf),
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
            Ok(meta) if meta.is_dir() => current = next,
            Ok(meta) if meta.file_type().is_symlink() => {
                if current.starts_with(project_root) {
                    return Err(OutputDirError::Unsafe(next));
                }
                let target = fs::canonicalize(&next)
                    .map_err(|e| OutputDirError::Io("resolving", next.clone(), e))?;
                if !target.is_dir() {
                    return Err(OutputDirError::Unsafe(next));
                }
                current = target;
            }
            Ok(_) => return Err(OutputDirError::Unsafe(next)),
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
                    Ok(meta) if meta.is_dir() => current = next,
                    Ok(_) => return Err(OutputDirError::Unsafe(next)),
                    Err(e) => return Err(OutputDirError::Io("inspecting", next, e)),
                }
            }
            Err(e) => return Err(OutputDirError::Io("inspecting", next, e)),
        }
    }
    Ok(Some(current))
}

/// Whether `output` (canonical) is below `project_root` but not directly in
/// it: such a directory is copied into the next workspace (only
/// `texrun-out` and direct children of the root are excluded).
pub fn is_nested_in(output: &Path, project_root: &Path) -> bool {
    output.starts_with(project_root)
        && output != project_root
        && output.parent() != Some(project_root)
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
                Err(OutputDirError::Unsafe(p)) if p == root.join("src/texrun-out")
            ));
            assert!(matches!(
                walk(&root.join("src/texrun-out/deeper"), &root, create),
                Err(OutputDirError::Unsafe(_))
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
            Err(OutputDirError::Unsafe(_))
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
            Err(OutputDirError::Unsafe(_))
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

    #[test]
    fn nested_outputs() {
        let root = Path::new("/p");
        assert!(is_nested_in(Path::new("/p/a/b"), root));
        assert!(!is_nested_in(Path::new("/p/a"), root));
        assert!(!is_nested_in(Path::new("/p"), root));
        assert!(!is_nested_in(Path::new("/q/a/b"), root));
    }
}
