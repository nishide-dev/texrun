//! Copying a project root into a workspace directory.
//!
//! The source tree is walked with directory file descriptors
//! (`openat(dirfd, name, O_DIRECTORY | O_NOFOLLOW)`), never by re-resolving
//! host paths, and every opened directory / file is checked against the
//! `(dev, ino)` seen when it was listed. Replacing an entry (or an
//! intermediate directory) with a symlink during the copy therefore cannot
//! redirect the walk outside the project root; it is reported as
//! [`WorkspaceError::InputChanged`].

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;
use texrun_core::WorkspacePath;

use crate::config::{WorkspaceConfig, WorkspaceLimits};
use crate::error::{Limit, WorkspaceError};
use crate::fsutil::{DIR_FLAGS, FileId, READ_FLAGS, file_type, fold, fstat};

/// Tool configuration files that are read from the working directory and
/// are never copied into a workspace, independently of
/// [`WorkspaceConfig::excluded_names`]: latexmk's rc files (Perl code) and
/// biber's config file.
pub const TOOL_CONFIG_NAMES: &[&str] = &["latexmkrc", ".latexmkrc", "biber.conf", ".biber.conf"];

/// At most this many entries are recorded in each list of a
/// [`MaterializeReport`]; the totals are always exact.
pub const MAX_RECORDED_ENTRIES: usize = 1000;

/// Why a project entry was not copied into the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExclusionReason {
    /// The name is in [`WorkspaceConfig::excluded_names`] or (at the root)
    /// [`WorkspaceConfig::excluded_root_names`].
    ExcludedName,
    /// The extension is in [`WorkspaceConfig::excluded_extensions`].
    ExcludedExtension,
    /// A tool configuration file ([`TOOL_CONFIG_NAMES`]).
    ToolConfig,
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
    /// The entry is at one of [`WorkspaceConfig::excluded_paths`].
    ExcludedPath,
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
///
/// The lists hold at most [`MAX_RECORDED_ENTRIES`] items each; compare with
/// the `*_total` counters (or use [`MaterializeReport::is_truncated`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MaterializeReport {
    /// Number of copied entries (files, directories and symlinks).
    pub entries: u64,
    /// Total bytes of copied regular files.
    pub bytes: u64,
    /// Number of directory entries examined, including excluded ones.
    pub scanned: u64,
    /// Entries that were left out, in traversal order.
    pub excluded: Vec<ExcludedEntry>,
    /// Total number of excluded entries.
    pub excluded_total: u64,
    /// Copied regular files that have more than one hard link. Their
    /// content was copied like any other file, but the other links may be
    /// outside the project root (see the crate documentation).
    pub hardlinked: Vec<PathBuf>,
    /// Total number of such files.
    pub hardlinked_total: u64,
    /// Number of entries that were listed but had disappeared by the time
    /// they were inspected (the project was modified during the copy). They
    /// are not copied; a nonzero value is a hint that the workspace may not
    /// match any single state of the project.
    pub vanished: u64,
}

impl MaterializeReport {
    /// Recorded excluded entries with the given reason.
    pub fn excluded_with(&self, reason: ExclusionReason) -> impl Iterator<Item = &ExcludedEntry> {
        self.excluded.iter().filter(move |e| e.reason == reason)
    }

    /// Whether some entries were counted but not recorded in the lists.
    pub fn is_truncated(&self) -> bool {
        self.excluded_total > self.excluded.len() as u64
            || self.hardlinked_total > self.hardlinked.len() as u64
    }
}

/// Where in the tree a directory is, for name- and path-based decisions
/// about its entries.
#[derive(Debug, Clone)]
struct Level {
    /// The directory is the project root.
    at_root: bool,
    /// Number of components of the directory's path below the root (0 for
    /// the root), i.e. the index of the path component its entries are
    /// compared with.
    depth: usize,
    /// Path rules ([`Exclusions::paths`]) whose first `depth` components
    /// are this directory, so an entry matching component `depth` is on
    /// their path.
    paths: Vec<usize>,
}

impl Level {
    fn root(exclusions: &Exclusions) -> Self {
        Self {
            at_root: true,
            depth: 0,
            paths: (0..exclusions.paths.len()).collect(),
        }
    }

    /// The level of a subdirectory kept with `paths` from its [`Verdict`].
    fn child(&self, paths: Vec<usize>) -> Self {
        Self {
            at_root: false,
            depth: self.depth + 1,
            paths,
        }
    }
}

/// Decision about one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Exclude(ExclusionReason),
    /// Copy it. For a directory on the path of some path rules, `paths` are
    /// the [`Level::paths`] of its contents.
    Keep {
        paths: Vec<usize>,
    },
}

impl Verdict {
    /// Copy it; not on any rule's path.
    fn keep() -> Self {
        Self::Keep { paths: Vec::new() }
    }
}

/// What a root-relative path rule stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    /// The workspace output directory. A non-directory where one of its
    /// ancestors has to be is excluded too, since it would shadow it.
    Output,
    /// One of [`WorkspaceConfig::excluded_paths`]. Only the path itself (and
    /// so everything below it) is excluded; its ancestors and siblings are
    /// not affected.
    Excluded,
}

/// A path relative to the project root, split into components.
#[derive(Debug)]
struct PathRule {
    kind: PathKind,
    /// Components as given, for the filesystem alias check.
    raw: Vec<String>,
    /// Folded components.
    folded: Vec<String>,
}

impl PathRule {
    fn new(kind: PathKind, path: &WorkspacePath) -> Self {
        let raw: Vec<String> = path.as_str().split('/').map(str::to_owned).collect();
        Self {
            kind,
            folded: raw.iter().map(|c| fold(c)).collect(),
            raw,
        }
    }
}

/// What an entry matching component `i` of a path rule means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStep {
    Exclude(ExclusionReason),
    /// A directory on the path: its contents are compared with the next
    /// component.
    Descend,
    /// A non-directory where an ancestor of an excluded path would be: the
    /// rule does not apply to it.
    Unaffected,
}

/// What a protected spelling stands for.
#[derive(Debug, Clone, Copy)]
enum Protected {
    Reason(ExclusionReason),
    /// The current component of path rule `i`.
    Path(usize),
}

/// Decides which entries never enter the workspace.
pub(crate) struct Exclusions {
    /// Folded names, any depth (tool config names first).
    names: Vec<(String, ExclusionReason)>,
    /// Folded names, root only.
    root_names: Vec<String>,
    /// Folded extensions.
    extensions: Vec<String>,
    /// Root-relative paths: the output directory first, then
    /// [`WorkspaceConfig::excluded_paths`].
    paths: Vec<PathRule>,
    /// Spellings asked of the workspace filesystem after creating an entry.
    raw_names: Vec<(String, ExclusionReason)>,
    raw_root_names: Vec<String>,
    /// Name folding is on; only switched off in tests of the filesystem
    /// alias check.
    lexical: bool,
}

impl Exclusions {
    pub(crate) fn new(config: &WorkspaceConfig, output_dir: &WorkspacePath) -> Self {
        let raw_names: Vec<(String, ExclusionReason)> = TOOL_CONFIG_NAMES
            .iter()
            .map(|&n| (n.to_owned(), ExclusionReason::ToolConfig))
            .chain(
                config
                    .excluded_names
                    .iter()
                    .map(|n| (n.clone(), ExclusionReason::ExcludedName)),
            )
            .collect();
        let paths = std::iter::once(PathRule::new(PathKind::Output, output_dir))
            .chain(
                config
                    .excluded_paths
                    .iter()
                    .map(|p| PathRule::new(PathKind::Excluded, p)),
            )
            .collect();
        Self {
            names: raw_names.iter().map(|(n, r)| (fold(n), *r)).collect(),
            root_names: config.excluded_root_names.iter().map(|n| fold(n)).collect(),
            extensions: config
                .excluded_extensions
                .iter()
                .map(|e| fold(e.trim_start_matches('.')))
                .collect(),
            paths,
            raw_names,
            raw_root_names: config.excluded_root_names.clone(),
            lexical: true,
        }
    }

    /// Exclusion by (folded) name or extension alone. Extensions only apply
    /// to non-directories (files and symlinks): formats are only ever looked
    /// up as files, so e.g. a `data.base/` directory is kept.
    fn name_reason(&self, folded: &str, at_root: bool, is_dir: bool) -> Option<ExclusionReason> {
        if let Some((_, reason)) = self.names.iter().find(|(n, _)| n == folded) {
            return Some(*reason);
        }
        if at_root && self.root_names.iter().any(|n| n == folded) {
            return Some(ExclusionReason::ExcludedName);
        }
        match folded.rsplit_once('.') {
            Some((_, ext)) if !is_dir && self.extensions.iter().any(|e| e == ext) => {
                Some(ExclusionReason::ExcludedExtension)
            }
            _ => None,
        }
    }

    /// What an entry matching component `i` of path rule `rule` means.
    fn path_step(&self, rule: usize, i: usize, is_dir: bool) -> PathStep {
        let rule = &self.paths[rule];
        let last = i + 1 == rule.folded.len();
        match rule.kind {
            // The output directory itself, or a non-directory where one of
            // its ancestors has to be (e.g. a `.texrun` file or symlink).
            PathKind::Output if last || !is_dir => {
                PathStep::Exclude(ExclusionReason::OutputDirectory)
            }
            // Any kind of entry at the excluded path itself.
            PathKind::Excluded if last => PathStep::Exclude(ExclusionReason::ExcludedPath),
            _ if is_dir => PathStep::Descend,
            _ => PathStep::Unaffected,
        }
    }

    /// Combines the steps of the path rules `rules`, all matched by an entry
    /// in a directory at `level`: the first exclusion wins (the output
    /// directory comes first), otherwise the entry is kept with the rules it
    /// descends into.
    fn path_verdict(
        &self,
        level: &Level,
        is_dir: bool,
        rules: impl IntoIterator<Item = usize>,
    ) -> Verdict {
        let mut paths = Vec::new();
        for rule in rules {
            match self.path_step(rule, level.depth, is_dir) {
                PathStep::Exclude(reason) => return Verdict::Exclude(reason),
                PathStep::Descend => paths.push(rule),
                PathStep::Unaffected => {}
            }
        }
        Verdict::Keep { paths }
    }

    /// Lexical decision for an entry named `name` in a directory at `level`.
    fn classify(&self, name: &OsStr, level: &Level, is_dir: bool) -> Verdict {
        // Non-UTF-8 names never match lexically; the filesystem alias check
        // still applies to them.
        let Some(raw) = name.to_str() else {
            return Verdict::keep();
        };
        let folded = fold(raw);
        if self.lexical
            && let Some(reason) = self.name_reason(&folded, level.at_root, is_dir)
        {
            return Verdict::Exclude(reason);
        }
        let on_path = level.paths.iter().copied().filter(|&r| {
            let rule = &self.paths[r];
            if self.lexical {
                rule.folded[level.depth] == folded
            } else {
                rule.raw[level.depth] == raw
            }
        });
        self.path_verdict(level, is_dir, on_path)
    }

    /// Spellings that must not be reachable in a directory at `level`.
    fn protected(&self, level: &Level) -> Vec<(&str, Protected)> {
        let mut out: Vec<(&str, Protected)> = self
            .raw_names
            .iter()
            .map(|(n, r)| (n.as_str(), Protected::Reason(*r)))
            .collect();
        if level.at_root {
            out.extend(
                self.raw_root_names
                    .iter()
                    .map(|n| (n.as_str(), Protected::Reason(ExclusionReason::ExcludedName))),
            );
        }
        out.extend(
            level
                .paths
                .iter()
                .map(|&r| (self.paths[r].raw[level.depth].as_str(), Protected::Path(r))),
        );
        out
    }

    /// Asks the workspace filesystem whether the entry just created as
    /// `name` in `dst` (identity `created`) is also reachable under a
    /// protected spelling, e.g. because the filesystem folds case or
    /// normalizes Unicode. Returns the resulting verdict, if any: an
    /// exclusion, or the path rules the directory turns out to be on (in
    /// addition to those found by [`Exclusions::classify`]).
    fn alias_verdict(
        &self,
        dst: &OwnedFd,
        name: &OsStr,
        created: FileId,
        level: &Level,
        is_dir: bool,
    ) -> io::Result<Option<Verdict>> {
        let mut rules = Vec::new();
        for (spelling, protected) in self.protected(level) {
            if spelling.as_bytes() == name.as_bytes() {
                continue;
            }
            match rustix::fs::statat(dst, spelling, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) if FileId::of(&st) == created => match protected {
                    Protected::Reason(r) => return Ok(Some(Verdict::Exclude(r))),
                    Protected::Path(r) => rules.push(r),
                },
                Ok(_) | Err(Errno::NOENT | Errno::NOTDIR | Errno::NAMETOOLONG | Errno::ILSEQ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if rules.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.path_verdict(level, is_dir, rules)))
    }

    /// Whether `rel` (e.g. a symlink target or the entrypoint) is excluded
    /// by any of its components, is at or below an excluded path, or
    /// touches the output directory (is it, is below it, or is one of its
    /// ancestors). `is_dir` tells whether the last component is a
    /// directory; all others are.
    pub(crate) fn excludes_path(&self, rel: &Path, is_dir: bool) -> bool {
        let comps: Vec<Option<String>> = rel.iter().map(|c| c.to_str().map(fold)).collect();
        let last = comps.len().saturating_sub(1);
        let by_name = comps.iter().enumerate().any(|(i, c)| {
            c.as_deref()
                .is_some_and(|c| self.name_reason(c, i == 0, is_dir || i < last).is_some())
        });
        if by_name {
            return true;
        }
        self.paths.iter().any(|rule| {
            let common = comps
                .iter()
                .zip(&rule.folded)
                .take_while(|(a, b)| a.as_deref() == Some(b.as_str()))
                .count();
            match rule.kind {
                PathKind::Output => common == comps.len() || common == rule.folded.len(),
                PathKind::Excluded => common == rule.folded.len(),
            }
        })
    }
}

/// Opens directory `name` in `parent` without following symlinks and
/// checks that it is the object listed as `expected`.
pub(crate) fn open_dir_checked(
    parent: &OwnedFd,
    name: &OsStr,
    expected: FileId,
    rel: &Path,
) -> Result<OwnedFd, WorkspaceError> {
    let fd = rustix::fs::openat(parent, name, DIR_FLAGS, Mode::empty())
        .map_err(|e| open_error(e, rel))?;
    let st = fstat(&fd).map_err(WorkspaceError::io("inspecting", rel))?;
    if FileId::of(&st) != expected {
        return Err(WorkspaceError::InputChanged(rel.to_path_buf()));
    }
    Ok(fd)
}

/// A failed `openat` of a listed entry: a symlink / non-directory / vanished
/// entry where the listing saw something else means the input changed.
fn open_error(e: Errno, rel: &Path) -> WorkspaceError {
    match e {
        Errno::LOOP | Errno::MLINK | Errno::NOTDIR | Errno::NOENT => {
            WorkspaceError::InputChanged(rel.to_path_buf())
        }
        e => WorkspaceError::io("opening", rel)(e.into()),
    }
}

/// One entry being copied.
struct Entry<'n> {
    name: &'n OsStr,
    rel: PathBuf,
    id: FileId,
    /// Level of the directory containing the entry.
    level: &'n Level,
}

/// Copies a project root into a workspace directory.
pub(crate) struct Materializer<'a> {
    limits: WorkspaceLimits,
    exclusions: &'a Exclusions,
    /// Canonical project root (for resolving symlinks and messages).
    src_root: &'a Path,
    report: MaterializeReport,
    /// Top-level project entry that contains the workspace, when the
    /// workspace was created below the project root (e.g. a temp parent
    /// inside it). It is never copied, so the workspace is not copied into
    /// itself.
    skip: Option<(PathBuf, FileId)>,
}

impl<'a> Materializer<'a> {
    pub(crate) fn new(
        limits: WorkspaceLimits,
        exclusions: &'a Exclusions,
        src_root: &'a Path,
        dst_root: &Path,
    ) -> Self {
        let skip = dst_root
            .strip_prefix(src_root)
            .ok()
            .and_then(|rel| rel.iter().next())
            .map(|first| src_root.join(first))
            .and_then(|p| {
                let id = FileId::of_metadata(&std::fs::symlink_metadata(&p).ok()?);
                Some((p, id))
            });
        Self {
            limits,
            exclusions,
            src_root,
            report: MaterializeReport::default(),
            skip,
        }
    }

    /// Copies the directory `src` into the (empty) directory `dst`.
    pub(crate) fn run(
        mut self,
        src: &OwnedFd,
        dst: &OwnedFd,
    ) -> Result<MaterializeReport, WorkspaceError> {
        let level = Level::root(self.exclusions);
        self.copy_dir(src, dst, Path::new(""), 0, &level)?;
        Ok(self.report)
    }

    fn exclude(&mut self, path: PathBuf, reason: ExclusionReason) {
        self.report.excluded_total += 1;
        if self.report.excluded.len() < MAX_RECORDED_ENTRIES {
            self.report.excluded.push(ExcludedEntry { path, reason });
        }
    }

    fn count_entry(&mut self) -> Result<(), WorkspaceError> {
        self.report.entries += 1;
        if self.report.entries > self.limits.max_entries {
            return Err(WorkspaceError::LimitExceeded {
                limit: Limit::Entries,
                max: self.limits.max_entries,
            });
        }
        Ok(())
    }

    /// Lists `src`, counting every entry against
    /// [`WorkspaceLimits::max_scanned_entries`] as it is read.
    fn list(&mut self, src: &OwnedFd, rel: &Path) -> Result<Vec<OsString>, WorkspaceError> {
        let err = |e: Errno| WorkspaceError::io("reading directory", rel)(e.into());
        let mut names = Vec::new();
        for entry in Dir::read_from(src).map_err(err)? {
            let entry = entry.map_err(err)?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            self.report.scanned += 1;
            if self.report.scanned > self.limits.max_scanned_entries {
                return Err(WorkspaceError::LimitExceeded {
                    limit: Limit::ScannedEntries,
                    max: self.limits.max_scanned_entries,
                });
            }
            names.push(OsString::from_vec(name.to_vec()));
        }
        names.sort();
        Ok(names)
    }

    fn copy_dir(
        &mut self,
        src: &OwnedFd,
        dst: &OwnedFd,
        rel: &Path,
        depth: usize,
        level: &Level,
    ) -> Result<(), WorkspaceError> {
        let names = self.list(src, rel)?;
        self.copy_entries(src, dst, rel, depth, level, names)
    }

    /// Copies the listed entries `names` of `src` into `dst`.
    fn copy_entries(
        &mut self,
        src: &OwnedFd,
        dst: &OwnedFd,
        rel: &Path,
        depth: usize,
        level: &Level,
        names: Vec<OsString>,
    ) -> Result<(), WorkspaceError> {
        for name in names {
            let rel_child = rel.join(&name);
            let st = match rustix::fs::statat(src, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => st,
                // Removed since it was listed: nothing to copy.
                Err(Errno::NOENT) => {
                    self.report.vanished += 1;
                    continue;
                }
                Err(e) => return Err(WorkspaceError::io("inspecting", rel_child)(e.into())),
            };
            let id = FileId::of(&st);
            let ft = file_type(&st);
            if self.skip.as_ref().is_some_and(|(_, skip)| *skip == id) {
                self.exclude(rel_child, ExclusionReason::WorkspaceDirectory);
                continue;
            }
            let child_paths =
                match self
                    .exclusions
                    .classify(&name, level, ft == FileType::Directory)
                {
                    Verdict::Exclude(reason) => {
                        self.exclude(rel_child, reason);
                        continue;
                    }
                    Verdict::Keep { paths } => paths,
                };
            if !matches!(
                ft,
                FileType::Directory | FileType::RegularFile | FileType::Symlink
            ) {
                self.exclude(rel_child, ExclusionReason::SpecialFile);
                continue;
            }
            if depth + 1 > self.limits.max_depth {
                return Err(WorkspaceError::LimitExceeded {
                    limit: Limit::Depth,
                    max: self.limits.max_depth as u64,
                });
            }

            let entry = Entry {
                name: &name,
                rel: rel_child,
                id,
                level,
            };
            match ft {
                FileType::Directory => self.copy_subdir(src, dst, &entry, depth, child_paths)?,
                FileType::RegularFile => self.copy_file(src, dst, &entry, &st)?,
                _ => self.copy_symlink(src, dst, &entry)?,
            }
        }
        Ok(())
    }

    /// Removes an entry just created in the workspace that turned out to be
    /// excluded (filesystem alias of a protected name).
    fn undo(
        &mut self,
        dst: &OwnedFd,
        entry: &Entry<'_>,
        flags: AtFlags,
        reason: ExclusionReason,
    ) -> Result<(), WorkspaceError> {
        rustix::fs::unlinkat(dst, entry.name, flags)
            .map_err(|e| WorkspaceError::io("removing", &entry.rel)(e.into()))?;
        self.exclude(entry.rel.clone(), reason);
        Ok(())
    }

    fn copy_subdir(
        &mut self,
        src: &OwnedFd,
        dst: &OwnedFd,
        entry: &Entry<'_>,
        depth: usize,
        mut child_paths: Vec<usize>,
    ) -> Result<(), WorkspaceError> {
        let rel = &entry.rel;
        let sub_src = open_dir_checked(src, entry.name, entry.id, rel)?;
        rustix::fs::mkdirat(dst, entry.name, Mode::from_raw_mode(0o777))
            .map_err(|e| WorkspaceError::io("creating directory", rel)(e.into()))?;
        let sub_dst = rustix::fs::openat(dst, entry.name, DIR_FLAGS, Mode::empty())
            .map_err(|e| WorkspaceError::io("opening", rel)(e.into()))?;
        let created = FileId::of(&fstat(&sub_dst).map_err(WorkspaceError::io("inspecting", rel))?);
        match self
            .exclusions
            .alias_verdict(dst, entry.name, created, entry.level, true)
            .map_err(WorkspaceError::io("inspecting", rel))?
        {
            Some(Verdict::Exclude(reason)) => {
                drop(sub_dst);
                return self.undo(dst, entry, AtFlags::REMOVEDIR, reason);
            }
            Some(Verdict::Keep { paths }) => {
                // Also on the paths of the rules found by the filesystem.
                child_paths.extend(paths);
                child_paths.sort_unstable();
                child_paths.dedup();
            }
            None => {}
        }
        self.count_entry()?;
        let level = entry.level.child(child_paths);
        self.copy_dir(&sub_src, &sub_dst, rel, depth + 1, &level)
    }

    fn copy_file(
        &mut self,
        src: &OwnedFd,
        dst: &OwnedFd,
        entry: &Entry<'_>,
        listed: &rustix::fs::Stat,
    ) -> Result<(), WorkspaceError> {
        let rel = &entry.rel;
        let max = self.limits.max_total_bytes;
        let over = || WorkspaceError::LimitExceeded {
            limit: Limit::TotalBytes,
            max,
        };
        let remaining = max.checked_sub(self.report.bytes).ok_or_else(over)?;
        if u64::try_from(listed.st_size).unwrap_or(0) > remaining {
            return Err(over());
        }

        let input = rustix::fs::openat(src, entry.name, READ_FLAGS, Mode::empty())
            .map_err(|e| open_error(e, rel))?;
        let opened = fstat(&input).map_err(WorkspaceError::io("inspecting", rel))?;
        if file_type(&opened) != FileType::RegularFile || FileId::of(&opened) != entry.id {
            return Err(WorkspaceError::InputChanged(rel.clone()));
        }
        let hardlinked = opened.st_nlink > 1;

        let output = rustix::fs::openat(
            dst,
            entry.name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o666),
        )
        .map_err(|e| WorkspaceError::io("creating", rel)(e.into()))?;
        let created = FileId::of(&fstat(&output).map_err(WorkspaceError::io("inspecting", rel))?);
        if let Some(Verdict::Exclude(reason)) = self
            .exclusions
            .alias_verdict(dst, entry.name, created, entry.level, false)
            .map_err(WorkspaceError::io("inspecting", rel))?
        {
            drop(output);
            return self.undo(dst, entry, AtFlags::empty(), reason);
        }
        self.count_entry()?;
        if hardlinked {
            self.report.hardlinked_total += 1;
            if self.report.hardlinked.len() < MAX_RECORDED_ENTRIES {
                self.report.hardlinked.push(rel.clone());
            }
        }

        // Read at most one byte more than allowed, so a file that grows
        // during the copy is still caught.
        let (mut input, mut output) = (File::from(input), File::from(output));
        let copied = io::copy(&mut (&mut input).take(remaining + 1), &mut output)
            .map_err(WorkspaceError::io("copying", rel))?;
        if copied > remaining {
            return Err(over());
        }
        self.report.bytes += copied;
        Ok(())
    }

    /// Recreates a symlink whose target is inside the root as a relative
    /// symlink inside the workspace.
    ///
    /// The target is resolved by path, but the created link never depends
    /// on that resolution being stable: it is a relative path computed from
    /// the resolved in-root location, so it always points inside the
    /// workspace, and nothing is read through it here.
    fn copy_symlink(
        &mut self,
        src: &OwnedFd,
        dst: &OwnedFd,
        entry: &Entry<'_>,
    ) -> Result<(), WorkspaceError> {
        let rel = &entry.rel;
        let stored =
            rustix::fs::readlinkat(src, entry.name, Vec::new()).map_err(|e| open_error(e, rel))?;
        let stored = PathBuf::from(OsString::from_vec(stored.into_bytes()));
        let outside = || WorkspaceError::SymlinkOutsideRoot {
            link: rel.clone(),
            target: stored.clone(),
        };
        let Ok(resolved) = std::fs::canonicalize(self.src_root.join(rel)) else {
            // Dangling or looping: reject it if it lexically escapes the
            // root, otherwise leave it out.
            let inside = if stored.is_absolute() {
                lexical_normalize(&stored).is_some_and(|p| p.starts_with(self.src_root))
            } else {
                lexical_join(rel.parent().unwrap_or(Path::new("")), &stored).is_some()
            };
            if !inside {
                return Err(outside());
            }
            self.exclude(rel.clone(), ExclusionReason::UnresolvableSymlink);
            return Ok(());
        };
        let Ok(rel_target) = resolved.strip_prefix(self.src_root) else {
            return Err(outside());
        };
        let in_skipped = self
            .skip
            .as_ref()
            .is_some_and(|(p, _)| resolved.starts_with(p));
        // Whether the target is a directory only affects extension-based
        // exclusion; `resolved` exists, so a failed stat means it changed
        // meanwhile and is treated as a file (the stricter choice).
        let target_is_dir = std::fs::metadata(&resolved).is_ok_and(|m| m.is_dir());
        if in_skipped || self.exclusions.excludes_path(rel_target, target_is_dir) {
            self.exclude(rel.clone(), ExclusionReason::SymlinkToExcluded);
            return Ok(());
        }

        let ups = rel.parent().map_or(0, |p| p.iter().count());
        let mut link_text: PathBuf = std::iter::repeat_n(Component::ParentDir, ups).collect();
        link_text.push(rel_target);
        if link_text.as_os_str().is_empty() {
            link_text.push(".");
        }
        rustix::fs::symlinkat(&link_text, dst, entry.name)
            .map_err(|e| WorkspaceError::io("creating symlink", rel)(e.into()))?;
        let created = rustix::fs::statat(dst, entry.name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| WorkspaceError::io("inspecting", rel)(e.into()))?;
        if let Some(Verdict::Exclude(reason)) = self
            .exclusions
            .alias_verdict(dst, entry.name, FileId::of(&created), entry.level, false)
            .map_err(WorkspaceError::io("inspecting", rel))?
        {
            return self.undo(dst, entry, AtFlags::empty(), reason);
        }
        self.count_entry()
    }
}

/// Joins `rel` onto `base` lexically; `None` if it climbs above `base`'s
/// starting point (the project root) or is absolute.
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

/// Resolves `.` and `..` in an absolute path lexically (`/..` stays `/`).
fn lexical_normalize(abs: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::from("/");
    for comp in abs.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir | Component::RootDir => {}
            Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wp(s: &str) -> WorkspacePath {
        WorkspacePath::new(s).unwrap()
    }

    fn classify(ex: &Exclusions, rel: &str, is_dir: bool) -> Verdict {
        // Walk the parents like the materializer does.
        let mut level = Level::root(ex);
        let comps: Vec<&str> = rel.split('/').collect();
        for parent in &comps[..comps.len() - 1] {
            let Verdict::Keep { paths } = ex.classify(OsStr::new(parent), &level, true) else {
                panic!("{parent} excluded");
            };
            level = level.child(paths);
        }
        ex.classify(OsStr::new(comps[comps.len() - 1]), &level, is_dir)
    }

    fn excluded(reason: ExclusionReason) -> Verdict {
        Verdict::Exclude(reason)
    }

    #[test]
    fn classify_by_name_extension_and_output_dir() {
        use ExclusionReason::{ExcludedExtension, ExcludedName, OutputDirectory, ToolConfig};
        let config = WorkspaceConfig::default();
        let ex = Exclusions::new(&config, &wp(".texrun/out"));
        let keep = Verdict::keep();
        assert_eq!(classify(&ex, "latexmkrc", false), excluded(ToolConfig));
        assert_eq!(classify(&ex, "sub/.LatexMkRc", false), excluded(ToolConfig));
        assert_eq!(
            classify(&ex, "latexm\u{212A}rc", false),
            excluded(ToolConfig)
        );
        assert_eq!(classify(&ex, "biber.conf", false), excluded(ToolConfig));
        assert_eq!(classify(&ex, ".git", true), excluded(ExcludedName));
        assert_eq!(classify(&ex, "sub/.git", true), excluded(ExcludedName));
        assert_eq!(classify(&ex, "target", true), excluded(ExcludedName));
        assert_eq!(classify(&ex, "\u{FF54}arget", true), excluded(ExcludedName));
        assert_eq!(classify(&ex, "figures/target", true), keep);
        assert_eq!(
            classify(&ex, "pdflatex.fmt", false),
            excluded(ExcludedExtension)
        );
        assert_eq!(
            classify(&ex, "sub/x.FMT", false),
            excluded(ExcludedExtension)
        );
        assert_eq!(classify(&ex, "main.tex", false), keep);

        let ex = Exclusions::new(&config, &wp("build/out"));
        assert_eq!(
            classify(&ex, "build", true),
            Verdict::Keep { paths: vec![0] }
        );
        assert_eq!(classify(&ex, "build", false), excluded(OutputDirectory));
        assert_eq!(classify(&ex, "build/out", true), excluded(OutputDirectory));
        assert_eq!(classify(&ex, "BUILD/Out", true), excluded(OutputDirectory));
        assert_eq!(classify(&ex, "build/other", true), keep);
        assert_eq!(classify(&ex, "sub/build", true), keep);
    }

    #[test]
    fn excludes_path_checks_every_component_and_output_relation() {
        let config = WorkspaceConfig::default();
        let ex = Exclusions::new(&config, &wp("build/out"));
        assert!(ex.excludes_path(Path::new(".git/config"), false));
        assert!(ex.excludes_path(Path::new("a/latexmkrc"), false));
        assert!(ex.excludes_path(Path::new("a/LATEXM\u{212A}RC"), false));
        assert!(ex.excludes_path(Path::new("target/x"), false));
        assert!(!ex.excludes_path(Path::new("figures/target/x"), false));
        assert!(ex.excludes_path(Path::new("a/p.fmt"), false));
        assert!(ex.excludes_path(Path::new("build/out/x.pdf"), false));
        assert!(ex.excludes_path(Path::new("build"), false));
        assert!(ex.excludes_path(Path::new(""), false));
        assert!(!ex.excludes_path(Path::new("build/other.tex"), false));
        assert!(!ex.excludes_path(Path::new("src/main.tex"), false));
        // Extensions only exclude a non-directory last component.
        assert!(!ex.excludes_path(Path::new("a/data.base"), true));
        assert!(ex.excludes_path(Path::new("a/data.base"), false));
        assert!(!ex.excludes_path(Path::new("x.fmt/main.tex"), false));
        // Names still apply to directories.
        assert!(ex.excludes_path(Path::new("a/.git"), true));
    }

    #[test]
    fn classify_by_excluded_path() {
        use ExclusionReason::{ExcludedPath, OutputDirectory};
        let config = WorkspaceConfig::default().with_excluded_paths([wp("build/pdf"), wp("a/b/c")]);
        let ex = Exclusions::new(&config, &wp("build/out"));
        let keep = Verdict::keep();
        assert_eq!(classify(&ex, "build/pdf", true), excluded(ExcludedPath));
        assert_eq!(classify(&ex, "build/pdf", false), excluded(ExcludedPath));
        assert_eq!(
            classify(&ex, "Build/P\u{FF24}F", true),
            excluded(ExcludedPath)
        );
        assert_eq!(classify(&ex, "build/out", true), excluded(OutputDirectory));
        // `build` is on the path of both the output directory and `build/pdf`.
        assert_eq!(
            classify(&ex, "build", true),
            Verdict::Keep { paths: vec![0, 1] }
        );
        assert_eq!(classify(&ex, "a/b", true), Verdict::Keep { paths: vec![2] });
        assert_eq!(classify(&ex, "a/b/c", true), excluded(ExcludedPath));
        assert_eq!(classify(&ex, "A/B/C", false), excluded(ExcludedPath));
        // Ancestors that are not directories, siblings, and the same names
        // elsewhere are not affected.
        assert_eq!(classify(&ex, "a", false), keep);
        assert_eq!(classify(&ex, "a/b", false), keep);
        assert_eq!(classify(&ex, "a/b/d", true), keep);
        assert_eq!(classify(&ex, "a/b/cc", true), keep);
        assert_eq!(classify(&ex, "build/figures", true), keep);
        assert_eq!(classify(&ex, "pdf", true), keep);
        assert_eq!(classify(&ex, "x/build/pdf", true), keep);
        assert_eq!(classify(&ex, "b/c", true), keep);
    }

    #[test]
    fn excludes_path_checks_excluded_paths() {
        let config = WorkspaceConfig::default().with_excluded_paths([wp("build/pdf")]);
        let ex = Exclusions::new(&config, &wp(".texrun/out"));
        assert!(ex.excludes_path(Path::new("build/pdf"), true));
        assert!(ex.excludes_path(Path::new("build/pdf/main.pdf"), false));
        assert!(ex.excludes_path(Path::new("BUILD/Pdf/x"), false));
        assert!(!ex.excludes_path(Path::new("build"), true));
        assert!(!ex.excludes_path(Path::new("build/pdf2/x"), false));
        assert!(!ex.excludes_path(Path::new("build/figures/x.pdf"), false));
        assert!(!ex.excludes_path(Path::new("x/build/pdf"), true));
    }

    #[test]
    fn excluded_extensions_apply_to_files_and_symlinks_only() {
        use ExclusionReason::ExcludedExtension;
        let config = WorkspaceConfig::default();
        let ex = Exclusions::new(&config, &wp(".texrun/out"));
        let keep = Verdict::keep();
        assert_eq!(classify(&ex, "data.base", true), keep);
        assert_eq!(classify(&ex, "sub/x.FMT", true), keep);
        assert_eq!(classify(&ex, "memo.mem", true), keep);
        assert_eq!(
            classify(&ex, "data.base", false),
            excluded(ExcludedExtension)
        );
        assert_eq!(
            classify(&ex, "memo.mem", false),
            excluded(ExcludedExtension)
        );
    }

    #[test]
    fn entries_vanished_after_listing_are_counted() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("main.tex"), "x").unwrap();
        let config = WorkspaceConfig::default();
        let ex = Exclusions::new(&config, &wp(".texrun/out"));
        let src_root = std::fs::canonicalize(src.path()).unwrap();
        let dst_root = std::fs::canonicalize(dst.path()).unwrap();
        let mut m = Materializer::new(config.limits, &ex, &src_root, &dst_root);
        let level = Level::root(&ex);
        // As if `gone.tex` had been listed and then removed before `statat`.
        let names = vec![OsString::from("gone.tex"), OsString::from("main.tex")];
        m.copy_entries(
            &open_dir(&src_root),
            &open_dir(&dst_root),
            Path::new(""),
            0,
            &level,
            names,
        )
        .unwrap();
        assert_eq!(m.report.vanished, 1);
        assert_eq!(m.report.entries, 1);
        assert!(dst_root.join("main.tex").is_file());
        assert!(!dst_root.join("gone.tex").exists());
    }

    #[test]
    fn lexical_path_helpers() {
        assert_eq!(
            lexical_join(Path::new("a/b"), Path::new("../c")),
            Some(PathBuf::from("a/c"))
        );
        assert_eq!(lexical_join(Path::new("a"), Path::new("../../c")), None);
        assert_eq!(lexical_join(Path::new(""), Path::new("/etc")), None);
        assert_eq!(
            lexical_normalize(Path::new("/root/proj/../../etc/x")),
            Some(PathBuf::from("/etc/x"))
        );
        assert_eq!(
            lexical_normalize(Path::new("/../a/./b")),
            Some(PathBuf::from("/a/b"))
        );
    }

    fn open_dir(p: &Path) -> OwnedFd {
        rustix::fs::open(p, DIR_FLAGS, Mode::empty()).unwrap()
    }

    #[test]
    fn open_dir_checked_rejects_swapped_or_symlinked_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::fs::create_dir(tmp.path().join("other")).unwrap();
        std::os::unix::fs::symlink("other", tmp.path().join("link")).unwrap();
        let parent = open_dir(tmp.path());
        let id = |name: &str| {
            FileId::of_metadata(&std::fs::symlink_metadata(tmp.path().join(name)).unwrap())
        };

        assert!(open_dir_checked(&parent, OsStr::new("d"), id("d"), Path::new("d")).is_ok());
        // Listed as `d`, but something else is there now.
        assert!(matches!(
            open_dir_checked(&parent, OsStr::new("d"), id("other"), Path::new("d")),
            Err(WorkspaceError::InputChanged(_))
        ));
        // A symlink to a directory is never followed.
        assert!(matches!(
            open_dir_checked(&parent, OsStr::new("link"), id("other"), Path::new("link")),
            Err(WorkspaceError::InputChanged(_))
        ));
    }

    /// Whether the filesystem holding `dir` treats `A` and `a` as one name.
    fn case_insensitive(dir: &Path) -> bool {
        std::fs::write(dir.join("probe-case"), "").unwrap();
        let hit = dir.join("PROBE-CASE").exists();
        std::fs::remove_file(dir.join("probe-case")).unwrap();
        hit
    }

    /// With name folding switched off, the filesystem alias check alone must
    /// keep aliases of protected names out of the workspace (on
    /// case-insensitive filesystems such as default APFS; on case-sensitive
    /// ones the entries are distinct names and harmless).
    #[test]
    fn filesystem_alias_check_catches_what_folding_would() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("main.tex"), "x").unwrap();
        std::fs::write(src.path().join("LATEXMKRC"), "evil").unwrap();
        std::fs::create_dir(src.path().join(".GIT")).unwrap();
        std::fs::create_dir_all(src.path().join("BUILD/OUT")).unwrap();
        std::fs::write(src.path().join("BUILD/OUT/stale.pdf"), "stale").unwrap();
        std::fs::create_dir_all(src.path().join("BUILD/PDF")).unwrap();
        std::fs::write(src.path().join("BUILD/PDF/old.pdf"), "old").unwrap();
        std::fs::write(src.path().join("BUILD/keep.tex"), "keep").unwrap();

        let config = WorkspaceConfig::default().with_excluded_paths([wp("build/pdf")]);
        let mut ex = Exclusions::new(&config, &wp("build/out"));
        ex.lexical = false;
        let src_root = std::fs::canonicalize(src.path()).unwrap();
        let dst_root = std::fs::canonicalize(dst.path()).unwrap();
        let report = Materializer::new(config.limits, &ex, &src_root, &dst_root)
            .run(&open_dir(&src_root), &open_dir(&dst_root))
            .unwrap();

        let exists = |rel: &str| std::fs::symlink_metadata(dst_root.join(rel)).is_ok();
        assert!(exists("main.tex"));
        if case_insensitive(&dst_root) {
            assert!(!exists("latexmkrc"));
            assert!(!exists(".git"));
            assert!(!exists("build/out"));
            assert!(!exists("build/pdf"));
            assert!(exists("build"));
            assert!(exists("build/keep.tex"));
            let reasons: Vec<_> = report.excluded.iter().map(|e| e.reason).collect();
            assert!(reasons.contains(&ExclusionReason::ToolConfig), "{report:?}");
            assert!(
                reasons.contains(&ExclusionReason::ExcludedName),
                "{report:?}"
            );
            assert!(
                reasons.contains(&ExclusionReason::OutputDirectory),
                "{report:?}"
            );
            assert!(
                reasons.contains(&ExclusionReason::ExcludedPath),
                "{report:?}"
            );
        } else {
            // Distinct names: `latexmkrc` itself is still absent.
            assert!(!exists("latexmkrc"));
            assert!(!exists(".git"));
        }
    }
}
