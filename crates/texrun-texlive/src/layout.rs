//! Directories the engine prepares in the workspace, and output size scans.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use texrun_core::WorkspacePath;

/// texrun's directory in the workspace root. The workspace layer never
/// copies a directory of this name from the input project.
pub(crate) const TEXRUN_DIR: &str = ".texrun";

/// `HOME` of the engine, relative to the workspace root (§3.4).
pub(crate) const HOME_DIR: &str = ".texrun/home";

/// Upper bound on directories mirrored into the output directory. The
/// workspace layer already limits the number of entries (10,000 by
/// default); this only keeps a misconfigured workspace from making the walk
/// unbounded.
const MAX_MIRRORED_DIRS: usize = 10_000;

/// Creates `dir` (and missing parents) and checks that it is a real
/// directory, not a symlink.
pub(crate) fn ensure_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if meta.is_dir() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )))
    }
}

/// Recreates the directory tree below the entrypoint's directory inside the
/// output directory (§3.5): pdflatex's `-output-directory` does not create
/// subdirectories, so `\include{chapters/intro}` could not write
/// `chapters/intro.aux` otherwise.
///
/// - `entry_dir_rel` is the entrypoint's directory relative to the workspace
///   root (`None` for the root itself) and `entry_dir` its host path;
/// - directories that are the output directory or one of its ancestors, and
///   texrun's own `.texrun` directory, are not mirrored;
/// - symlinks are not followed.
pub(crate) fn mirror_subdirs(
    entry_dir: &Path,
    entry_dir_rel: Option<&WorkspacePath>,
    output_dir_rel: &WorkspacePath,
    output_dir: &Path,
) -> io::Result<()> {
    let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
    let mut created = 0usize;
    while let Some(rel) = stack.pop() {
        for entry in fs::read_dir(entry_dir.join(&rel))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let child_rel = rel.join(entry.file_name());
            let Ok(child) = WorkspacePath::from_path(&child_rel) else {
                // Not representable (e.g. not UTF-8): TeX could not name it
                // through texrun either.
                continue;
            };
            let ws_rel = match entry_dir_rel {
                Some(base) => base.join(&child),
                None => child.clone(),
            };
            if output_dir_rel.starts_with(&ws_rel) || ws_rel.as_str() == TEXRUN_DIR {
                continue;
            }
            created += 1;
            if created > MAX_MIRRORED_DIRS {
                return Err(io::Error::other(format!(
                    "more than {MAX_MIRRORED_DIRS} directories to mirror into the output directory"
                )));
            }
            fs::create_dir_all(output_dir.join(child.as_path()))?;
            stack.push(child_rel);
        }
    }
    Ok(())
}

/// Sizes of the regular files below some directories.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TreeSize {
    /// Sum of all file sizes.
    pub(crate) total: u64,
    /// Size of the largest file.
    pub(crate) largest: u64,
}

/// Sums the sizes of regular files below each of `dirs` without following
/// symlinks. Entries that disappear or cannot be read while the engine is
/// running are skipped.
pub(crate) fn tree_size(dirs: &[&Path]) -> TreeSize {
    let mut size = TreeSize::default();
    let mut stack: Vec<PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                size.total = size.total.saturating_add(meta.len());
                size.largest = size.largest.max(meta.len());
            }
        }
    }
    size
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wp(s: &str) -> WorkspacePath {
        WorkspacePath::new(s).unwrap()
    }

    fn dirs_below(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            for e in fs::read_dir(root.join(&rel)).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    let r = rel.join(e.file_name());
                    out.push(r.to_str().unwrap().to_owned());
                    stack.push(r);
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn mirrors_tree_of_root_entrypoint_without_texrun_dir() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        for d in ["chapters/part1", "figs", ".texrun/out", ".texrun/home"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("main.tex"), "").unwrap();
        fs::write(root.join("chapters/intro.tex"), "").unwrap();
        std::os::unix::fs::symlink("chapters", root.join("link")).unwrap();
        let out = root.join(".texrun/out");
        mirror_subdirs(root, None, &wp(".texrun/out"), &out).unwrap();
        assert_eq!(dirs_below(&out), ["chapters", "chapters/part1", "figs"]);
    }

    #[test]
    fn mirrors_tree_of_subdirectory_entrypoint() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        for d in ["src/chapters", "other", ".texrun/out"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let out = root.join(".texrun/out");
        mirror_subdirs(
            &root.join("src"),
            Some(&wp("src")),
            &wp(".texrun/out"),
            &out,
        )
        .unwrap();
        assert_eq!(dirs_below(&out), ["chapters"]);
    }

    #[test]
    fn skips_ancestors_of_a_custom_output_dir() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        for d in ["build/out", "build/cache", "sec"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let out = root.join("build/out");
        mirror_subdirs(root, None, &wp("build/out"), &out).unwrap();
        // `build` leads to the output directory itself and is skipped as a
        // whole; `sec` is mirrored.
        assert_eq!(dirs_below(&out), ["sec"]);
    }

    #[test]
    fn tree_size_counts_files_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a"), vec![0u8; 10]).unwrap();
        fs::write(dir.path().join("sub/b"), vec![0u8; 30]).unwrap();
        let big = tempfile::tempdir().unwrap();
        fs::write(big.path().join("huge"), vec![0u8; 1000]).unwrap();
        std::os::unix::fs::symlink(big.path(), dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(big.path().join("huge"), dir.path().join("flink")).unwrap();
        assert_eq!(
            tree_size(&[dir.path()]),
            TreeSize {
                total: 40,
                largest: 30
            }
        );
        assert_eq!(
            tree_size(&[Path::new("/nonexistent-texrun")]),
            TreeSize::default()
        );
    }

    #[test]
    fn ensure_dir_rejects_symlinks_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        fs::write(dir.path().join("file"), "").unwrap();
        assert!(ensure_dir(&dir.path().join("new/nested")).is_ok());
        assert!(ensure_dir(&dir.path().join("link")).is_err());
        assert!(ensure_dir(&dir.path().join("file")).is_err());
    }
}
