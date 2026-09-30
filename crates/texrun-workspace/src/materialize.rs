//! Copying a project root into a workspace directory.

use std::ffi::OsStr;
use std::fs::{self, DirEntry, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use texrun_core::WorkspacePath;

use crate::config::WorkspaceConfig;
use crate::error::{Limit, WorkspaceError};

/// File names that latexmk reads as Perl configuration from the working
/// directory. They are never copied into a workspace, independently of
/// [`WorkspaceConfig::excluded_names`] (matched ignoring ASCII case).
pub const LATEXMK_RC_NAMES: &[&str] = &["latexmkrc", ".latexmkrc"];

/// Why a project entry was not copied into the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExclusionReason {
    /// The name is in [`WorkspaceConfig::excluded_names`].
    ExcludedName,
    /// A `latexmkrc` / `.latexmkrc` file (Perl code run by latexmk).
    LatexmkRc,
    /// The entry is (or would shadow) the workspace output directory.
    OutputDirectory,
    /// A symlink inside the root whose target is itself excluded.
    SymlinkToExcluded,
    /// A symlink whose target does not exist or cannot be resolved (it stays
    /// inside the root lexically; otherwise it is an error).
    UnresolvableSymlink,
    /// Not a regular file, directory or symlink (FIFO, socket, device, ...).
    SpecialFile,
    /// The directory containing the workspace itself (only when workspaces
    /// are created below the project root).
    WorkspaceDirectory,
}

/// An entry of the project that was left out of the workspace. Entries
/// below an excluded directory are not listed individually.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedEntry {
    /// Path relative to the project root.
    pub path: PathBuf,
    /// Why it was left out.
    pub reason: ExclusionReason,
}

/// What was copied into a workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MaterializeReport {
    /// Number of copied entries (files, directories and symlinks).
    pub entries: u64,
    /// Total bytes of copied regular files.
    pub bytes: u64,
    /// Entries that were left out, in traversal order.
    pub excluded: Vec<ExcludedEntry>,
}

impl MaterializeReport {
    /// Excluded entries with the given reason.
    pub fn excluded_with(&self, reason: ExclusionReason) -> impl Iterator<Item = &ExcludedEntry> {
        self.excluded.iter().filter(move |e| e.reason == reason)
    }
}

/// Decides which relative paths never enter the workspace.
pub(crate) struct Exclusions<'a> {
    names: &'a [String],
    output_dir: Vec<&'a str>,
}

impl<'a> Exclusions<'a> {
    pub(crate) fn new(config: &'a WorkspaceConfig, output_dir: &'a WorkspacePath) -> Self {
        Self {
            names: &config.excluded_names,
            output_dir: output_dir.as_str().split('/').collect(),
        }
    }

    /// Reason for excluding the entry named `name` at `rel`, which is a
    /// directory iff `is_dir`. Only the last component is inspected; callers
    /// walk top-down.
    fn entry_reason(&self, rel: &Path, name: &OsStr, is_dir: bool) -> Option<ExclusionReason> {
        if let Some(reason) = self.name_reason(name) {
            return Some(reason);
        }
        match self.output_relation(rel) {
            // The output directory itself, or a non-directory where one of
            // its ancestors has to be (e.g. a `.texrun` file or symlink).
            Some(true) => Some(ExclusionReason::OutputDirectory),
            Some(false) if !is_dir => Some(ExclusionReason::OutputDirectory),
            _ => None,
        }
    }

    /// Whether any component of `rel` (a path to a directory entry of any
    /// type) would be excluded, or `rel` touches the output directory (is
    /// it, is below it, or is one of its ancestors). Used for symlink targets
    /// and the entrypoint.
    pub(crate) fn excludes_path(&self, rel: &Path) -> bool {
        let comps: Vec<&OsStr> = rel.iter().collect();
        if comps.iter().any(|c| self.name_reason(c).is_some()) {
            return true;
        }
        // `rel` is an ancestor of (or equal to) the output dir, or below it.
        let common = comps
            .iter()
            .zip(&self.output_dir)
            .take_while(|(a, b)| a.to_str().is_some_and(|a| a.eq_ignore_ascii_case(b)))
            .count();
        common == comps.len() || common == self.output_dir.len()
    }

    /// Exclusion by name alone. Non-UTF-8 names never match.
    fn name_reason(&self, name: &OsStr) -> Option<ExclusionReason> {
        let name = name.to_str()?;
        if LATEXMK_RC_NAMES
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name))
        {
            Some(ExclusionReason::LatexmkRc)
        } else if self.names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            Some(ExclusionReason::ExcludedName)
        } else {
            None
        }
    }

    /// `Some(true)` if `rel` is the output dir, `Some(false)` if it is a
    /// strict ancestor of it, `None` otherwise. Ignores ASCII case.
    fn output_relation(&self, rel: &Path) -> Option<bool> {
        let mut n = 0;
        for (i, comp) in rel.iter().enumerate() {
            let out = self.output_dir.get(i)?;
            if !comp.to_str()?.eq_ignore_ascii_case(out) {
                return None;
            }
            n += 1;
        }
        Some(n == self.output_dir.len())
    }
}

/// Copies `src_root` into `dst_root` (an existing empty directory).
pub(crate) struct Materializer<'a> {
    config: &'a WorkspaceConfig,
    exclusions: &'a Exclusions<'a>,
    /// Canonical project root.
    src_root: &'a Path,
    /// Canonical workspace root.
    dst_root: &'a Path,
    report: MaterializeReport,
    /// Top-level project entry that contains the workspace, when the
    /// workspace was created below the project root (e.g. a temp parent
    /// inside it). It is never copied, so the workspace is not copied into
    /// itself.
    skip: Option<PathBuf>,
}

impl<'a> Materializer<'a> {
    pub(crate) fn new(
        config: &'a WorkspaceConfig,
        exclusions: &'a Exclusions<'a>,
        src_root: &'a Path,
        dst_root: &'a Path,
    ) -> Self {
        let skip = dst_root
            .strip_prefix(src_root)
            .ok()
            .and_then(|rel| rel.iter().next())
            .map(|first| src_root.join(first));
        Self {
            config,
            exclusions,
            src_root,
            dst_root,
            report: MaterializeReport::default(),
            skip,
        }
    }

    pub(crate) fn run(mut self) -> Result<MaterializeReport, WorkspaceError> {
        self.copy_dir(Path::new(""), 0)?;
        Ok(self.report)
    }

    fn exclude(&mut self, path: PathBuf, reason: ExclusionReason) {
        self.report.excluded.push(ExcludedEntry { path, reason });
    }

    fn copy_dir(&mut self, rel: &Path, depth: usize) -> Result<(), WorkspaceError> {
        let src_dir = self.src_root.join(rel);
        let mut entries = fs::read_dir(&src_dir)
            .and_then(Iterator::collect::<io::Result<Vec<DirEntry>>>)
            .map_err(WorkspaceError::io("reading directory", &src_dir))?;
        entries.sort_by_key(DirEntry::file_name);

        for entry in entries {
            let src = entry.path();
            let name = entry.file_name();
            let rel_child = rel.join(&name);
            if self.skip.as_deref() == Some(src.as_path()) {
                self.exclude(rel_child, ExclusionReason::WorkspaceDirectory);
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(WorkspaceError::io("inspecting", &src))?;

            if let Some(reason) =
                self.exclusions
                    .entry_reason(&rel_child, &name, file_type.is_dir())
            {
                self.exclude(rel_child, reason);
                continue;
            }
            if !(file_type.is_dir() || file_type.is_file() || file_type.is_symlink()) {
                self.exclude(rel_child, ExclusionReason::SpecialFile);
                continue;
            }

            let limits = self.config.limits;
            if depth + 1 > limits.max_depth {
                return Err(WorkspaceError::LimitExceeded {
                    limit: Limit::Depth,
                    max: limits.max_depth as u64,
                });
            }
            self.report.entries += 1;
            if self.report.entries > limits.max_entries {
                return Err(WorkspaceError::LimitExceeded {
                    limit: Limit::Entries,
                    max: limits.max_entries,
                });
            }

            let dst = self.dst_root.join(&rel_child);
            if file_type.is_dir() {
                fs::create_dir(&dst).map_err(WorkspaceError::io("creating directory", &dst))?;
                self.copy_dir(&rel_child, depth + 1)?;
            } else if file_type.is_file() {
                self.copy_file(&entry, &rel_child, &dst)?;
            } else if let Some(reason) = self.copy_symlink(&src, &rel_child, &dst)? {
                self.report.entries -= 1;
                self.exclude(rel_child, reason);
            }
        }
        Ok(())
    }

    fn copy_file(
        &mut self,
        entry: &DirEntry,
        rel: &Path,
        dst: &Path,
    ) -> Result<(), WorkspaceError> {
        let src = entry.path();
        let max = self.config.limits.max_total_bytes;
        let over = || WorkspaceError::LimitExceeded {
            limit: Limit::TotalBytes,
            max,
        };
        let listed = entry
            .metadata()
            .map_err(WorkspaceError::io("inspecting", &src))?;
        let remaining = max.checked_sub(self.report.bytes).ok_or_else(over)?;
        if listed.len() > remaining {
            return Err(over());
        }

        let mut input = File::open(&src).map_err(WorkspaceError::io("opening", &src))?;
        // `open` follows symlinks: make sure we opened the file that was
        // listed, not something swapped in since.
        let opened = input
            .metadata()
            .map_err(WorkspaceError::io("inspecting", &src))?;
        if !opened.is_file() || !same_file(&listed, &opened) {
            return Err(WorkspaceError::InputChanged(rel.to_path_buf()));
        }

        let mut output = File::create_new(dst).map_err(WorkspaceError::io("creating", dst))?;
        // Read at most one byte more than allowed, so a file that grows
        // during the copy is still caught.
        let copied = io::copy(&mut (&mut input).take(remaining + 1), &mut output)
            .map_err(WorkspaceError::io("copying", &src))?;
        if copied > remaining {
            return Err(over());
        }
        self.report.bytes += copied;
        Ok(())
    }

    /// Recreates a symlink whose target is inside the root as a relative
    /// symlink inside the workspace. Returns a reason if it was skipped.
    fn copy_symlink(
        &mut self,
        src: &Path,
        rel: &Path,
        dst: &Path,
    ) -> Result<Option<ExclusionReason>, WorkspaceError> {
        let stored = fs::read_link(src).map_err(WorkspaceError::io("reading symlink", src))?;
        let outside = || WorkspaceError::SymlinkOutsideRoot {
            link: rel.to_path_buf(),
            target: stored.clone(),
        };
        let Ok(resolved) = fs::canonicalize(src) else {
            // Dangling or looping: reject it if it lexically escapes the
            // root, otherwise leave it out.
            let parent = rel.parent().unwrap_or(Path::new(""));
            if stored.is_absolute() {
                if !stored.starts_with(self.src_root) {
                    return Err(outside());
                }
            } else if lexical_join(parent, &stored).is_none() {
                return Err(outside());
            }
            return Ok(Some(ExclusionReason::UnresolvableSymlink));
        };
        let Ok(rel_target) = resolved.strip_prefix(self.src_root) else {
            return Err(outside());
        };
        let in_skipped = self
            .skip
            .as_deref()
            .is_some_and(|s| resolved.starts_with(s));
        if in_skipped || self.exclusions.excludes_path(rel_target) {
            return Ok(Some(ExclusionReason::SymlinkToExcluded));
        }

        let ups = rel.parent().map_or(0, |p| p.iter().count());
        let mut link_text: PathBuf = std::iter::repeat_n(Component::ParentDir, ups).collect();
        link_text.push(rel_target);
        if link_text.as_os_str().is_empty() {
            link_text.push(".");
        }
        make_symlink(&link_text, dst, rel)?;
        Ok(None)
    }
}

/// Joins `rel` onto `base` lexically; `None` if it climbs above `base`'s
/// starting point (the project root).
fn lexical_join(base: &Path, rel: &Path) -> Option<PathBuf> {
    let mut out: Vec<&OsStr> = base.iter().collect();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out.iter().collect())
}

#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.len() == b.len()
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path, _rel: &Path) -> Result<(), WorkspaceError> {
    std::os::unix::fs::symlink(target, link).map_err(WorkspaceError::io("creating symlink", link))
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _link: &Path, rel: &Path) -> Result<(), WorkspaceError> {
    Err(WorkspaceError::SymlinkUnsupported(rel.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exclusions_for<'a>(config: &'a WorkspaceConfig, out: &'a WorkspacePath) -> Exclusions<'a> {
        Exclusions::new(config, out)
    }

    #[test]
    fn entry_reasons() {
        let config = WorkspaceConfig::default();
        let out = WorkspacePath::new(".texrun/out").unwrap();
        let ex = exclusions_for(&config, &out);
        let reason = |rel: &str, is_dir| {
            let p = Path::new(rel);
            ex.entry_reason(p, p.file_name().unwrap(), is_dir)
        };
        assert_eq!(reason("latexmkrc", false), Some(ExclusionReason::LatexmkRc));
        assert_eq!(
            reason("sub/.LatexMkRc", false),
            Some(ExclusionReason::LatexmkRc)
        );
        assert_eq!(reason(".git", true), Some(ExclusionReason::ExcludedName));
        assert_eq!(
            reason("sub/Target", true),
            Some(ExclusionReason::ExcludedName)
        );
        assert_eq!(reason("main.tex", false), None);

        let custom = WorkspacePath::new("build/out").unwrap();
        let ex = exclusions_for(&config, &custom);
        let reason = |rel: &str, is_dir| {
            let p = Path::new(rel);
            ex.entry_reason(p, p.file_name().unwrap(), is_dir)
        };
        assert_eq!(reason("build", true), None);
        assert_eq!(
            reason("build", false),
            Some(ExclusionReason::OutputDirectory)
        );
        assert_eq!(
            reason("build/out", true),
            Some(ExclusionReason::OutputDirectory)
        );
        assert_eq!(
            reason("BUILD/Out", true),
            Some(ExclusionReason::OutputDirectory)
        );
        assert_eq!(reason("build/other", true), None);
    }

    #[test]
    fn excludes_path_checks_every_component_and_output_relation() {
        let config = WorkspaceConfig::default();
        let out = WorkspacePath::new("build/out").unwrap();
        let ex = exclusions_for(&config, &out);
        assert!(ex.excludes_path(Path::new(".git/config")));
        assert!(ex.excludes_path(Path::new("a/latexmkrc")));
        assert!(ex.excludes_path(Path::new("build/out/x.pdf")));
        assert!(ex.excludes_path(Path::new("build")));
        assert!(ex.excludes_path(Path::new("")));
        assert!(!ex.excludes_path(Path::new("build/other.tex")));
        assert!(!ex.excludes_path(Path::new("src/main.tex")));
    }

    #[test]
    fn lexical_join_detects_escapes() {
        assert_eq!(
            lexical_join(Path::new("a/b"), Path::new("../c")),
            Some(PathBuf::from("a/c"))
        );
        assert_eq!(lexical_join(Path::new("a"), Path::new("../../c")), None);
        assert_eq!(lexical_join(Path::new(""), Path::new("/etc")), None);
    }
}
