//! BibTeX failures: the `.blg` files in the output directory and what
//! latexmk says about BibTeX on its console.
//!
//! latexmk runs BibTeX in the output directory on each `.aux` file that
//! needs it (usually only `<stem>.aux`, which also covers `\include`d
//! files; packages such as `bibunits` add more), with `BIBINPUTS` /
//! `BSTINPUTS` starting with the entrypoint's directory. So:
//!
//! - every `*.blg` below the output directory written during this compile
//!   is parsed (`texrun_latex_log::BlgParser`), and
//! - a `.bib` / `.bst` name BibTeX printed is attributed to
//!   `<entrypoint dir>/<name>` when that is a regular file inside the
//!   workspace: the entrypoint's directory is the first element of the
//!   search path, so a file there is the one BibTeX read. Otherwise (an
//!   installed style, a file generated into the output directory) the
//!   diagnostic has no file.
//!
//! latexmk does not run BibTeX at all when a database named by
//! `\bibliography` does not exist (it "vetoes" the rule and exits with 0),
//! so no `.blg` is written; that is only visible on latexmk's console.
//!
//! The output directory is assumed to be new for each compile (the CLI
//! always creates a new workspace). The modification time check only keeps
//! logs of an earlier compile in a reused output directory out when they
//! are older than the second latexmk started in; and when latexmk finds
//! BibTeX's results up to date and does not run it again, the problems of
//! the earlier run are not reported again.
//!
//! # Cost
//!
//! Runs after the compile, outside its timeout, on files the document
//! controls, so everything is bounded: the walk of the output directory
//! visits at most [`MAX_SCANNED_ENTRIES`] entries, [`MAX_SCAN_DEPTH`] levels
//! deep, without following symlinks; at most [`MAX_BLG_FILES`] logs are read,
//! each regular file opened with `O_NOFOLLOW | O_NONBLOCK` and read up to
//! [`MAX_BLG_BYTES`] (its head: BibTeX reports errors in order), starting
//! with `<stem>.blg`; all logs together yield at most
//! [`MAX_BLG_DIAGNOSTICS`] diagnostics (errors first within each log; plus a
//! summary and a notice per log and a few notes). Parsing is linear in the
//! bytes read, and the console scan is linear in the captured output.

use std::collections::HashSet;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use texrun_core::{Diagnostic, DiagnosticKind, Severity, WorkspacePath};
use texrun_latex_log::BlgParser;

/// At most this many `.blg` files are read per compile.
pub(crate) const MAX_BLG_FILES: usize = 16;

/// At most this many bytes of each `.blg` are read.
pub(crate) const MAX_BLG_BYTES: u64 = 1024 * 1024;

/// At most this many diagnostics are kept from all `.blg` files of a
/// compile together (not counting their summaries and notices).
pub(crate) const MAX_BLG_DIAGNOSTICS: usize = 200;

/// The walk of the output directory stops after this many entries.
pub(crate) const MAX_SCANNED_ENTRIES: usize = 20_000;

/// The walk does not descend deeper than this below the output directory.
pub(crate) const MAX_SCAN_DEPTH: usize = 16;

/// At most this many missing databases are reported from latexmk's output.
const MAX_MISSING_DATABASES: usize = 32;

/// Where BibTeX looked for files, to attribute its messages.
pub(crate) struct Inputs<'a> {
    /// Canonical workspace root.
    pub root: &'a Path,
    /// The entrypoint's directory (host path, canonical).
    pub entry_dir: &'a Path,
    /// The same relative to the root (`None`: the root).
    pub entry_dir_rel: Option<&'a WorkspacePath>,
}

impl Inputs<'_> {
    /// `<entry dir>/<name>` if it is a regular file inside the workspace.
    fn locate(&self, name: &WorkspacePath) -> Option<WorkspacePath> {
        let real = fs::canonicalize(self.entry_dir.join(name.as_path())).ok()?;
        if !real.starts_with(self.root) || !fs::symlink_metadata(&real).ok()?.is_file() {
            return None;
        }
        Some(match self.entry_dir_rel {
            Some(dir) => dir.join(name),
            None => name.clone(),
        })
    }
}

/// Diagnostics about BibTeX: from the `.blg` files below `output_dir`
/// modified at or after `since` (`<stem>.blg` first), and from latexmk's
/// console output.
pub(crate) fn diagnostics(
    output_dir: &Path,
    stem: &str,
    since: SystemTime,
    inputs: &Inputs<'_>,
    stdout: &[u8],
    stderr: &[u8],
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let (blgs, walk_cut) = find_blgs(output_dir, &format!("{stem}.blg"), since);
    let locate = |name: &WorkspacePath| inputs.locate(name);
    let mut failed = false;
    let mut budget = MAX_BLG_DIAGNOSTICS;
    for (n, path) in blgs.iter().enumerate() {
        if budget == 0 {
            out.push(Diagnostic::new(
                Severity::Info,
                DiagnosticKind::Other,
                format!(
                    "{} more BibTeX logs were not analyzed (limit: {MAX_BLG_DIAGNOSTICS} \
                     diagnostics from BibTeX logs)",
                    blgs.len() - n
                ),
            ));
            break;
        }
        let Some((bytes, cut)) = read_head(path, MAX_BLG_BYTES) else {
            continue;
        };
        let parsed = BlgParser::new()
            .with_max_diagnostics(budget)
            .parse_with_files(&bytes, &locate);
        failed |= parsed.failed;
        let kept = parsed
            .diagnostics
            .iter()
            .filter(|d| d.kind != DiagnosticKind::BibtexFailed)
            .count()
            - usize::from(parsed.omitted > 0);
        budget = budget.saturating_sub(kept);
        if parsed.omitted > 0 {
            budget = 0;
        }
        out.extend(parsed.diagnostics);
        if cut {
            out.push(Diagnostic::new(
                Severity::Info,
                DiagnosticKind::Other,
                format!(
                    "a BibTeX log is larger than {MAX_BLG_BYTES} bytes; only its beginning was \
                     analyzed"
                ),
            ));
        }
    }
    if walk_cut {
        out.push(Diagnostic::new(
            Severity::Info,
            DiagnosticKind::Other,
            format!(
                "BibTeX logs were looked for in at most {MAX_SCANNED_ENTRIES} entries and \
                 {MAX_BLG_FILES} files of the output directory"
            ),
        ));
    }

    let console = scan_console(stdout, stderr);
    for name in &console.missing_databases {
        out.push(Diagnostic::new(
            Severity::Warning,
            DiagnosticKind::MissingFile,
            format!(
                "BibTeX was not run: latexmk did not find the bibliography database `{name}` \
                 (named by \\bibliography; looked up relative to the document)"
            ),
        ));
    }
    if console.vetoed && console.missing_databases.is_empty() {
        out.push(Diagnostic::new(
            Severity::Warning,
            DiagnosticKind::BibtexFailed,
            "BibTeX was not run: latexmk did not find a bibliography database; citations are \
             unresolved",
        ));
    }
    if console.bibtex_errors && !failed {
        out.push(Diagnostic::new(
            Severity::Error,
            DiagnosticKind::BibtexFailed,
            "latexmk reports that BibTeX failed, but no BibTeX log (.blg) of this compile \
             could be read",
        ));
    }
    out
}

/// Regular `*.blg` files below `dir` modified at or after `since` (whole
/// seconds, for file systems with coarse timestamps; a safety net only, see
/// the module docs): `dir/<first>` first (found even when the limits cut
/// the walk short), then the others sorted; and whether the walk was cut
/// short by a limit.
pub(crate) fn find_blgs(dir: &Path, first: &str, since: SystemTime) -> (Vec<PathBuf>, bool) {
    let since = since
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(SystemTime::UNIX_EPOCH, |d| {
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(d.as_secs())
        });
    let fresh = |meta: &fs::Metadata| meta.modified().is_ok_and(|m| m >= since);
    let first = dir.join(first);
    let first = fs::symlink_metadata(&first)
        .is_ok_and(|m| m.is_file() && fresh(&m))
        .then_some(first);
    let mut found = Vec::new();
    let mut visited = 0usize;
    let mut cut = false;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    'walk: while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries {
            visited += 1;
            if visited > MAX_SCANNED_ENTRIES {
                cut = true;
                break 'walk;
            }
            let Ok(entry) = entry else { continue };
            // `file_type` does not follow symlinks.
            let Ok(ty) = entry.file_type() else { continue };
            let path = entry.path();
            if ty.is_dir() {
                if depth < MAX_SCAN_DEPTH {
                    stack.push((path, depth + 1));
                } else {
                    cut = true;
                }
            } else if ty.is_file()
                && path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("blg"))
                && first.as_ref() != Some(&path)
                && entry.metadata().is_ok_and(|m| fresh(&m))
            {
                if found.len() + usize::from(first.is_some()) == MAX_BLG_FILES {
                    cut = true;
                    break 'walk;
                }
                found.push(path);
            }
        }
    }
    found.sort();
    if let Some(first) = first {
        found.insert(0, first);
    }
    (found, cut)
}

/// The first `max` bytes of the regular file `path` (opened with
/// `O_NOFOLLOW | O_NONBLOCK`, so a symlink, FIFO or device is refused
/// without blocking), and whether there was more.
pub(crate) fn read_head(path: &Path, max: u64) -> Option<(Vec<u8>, bool)> {
    use rustix::fs::{Mode, OFlags};

    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = fs::File::from(rustix::fs::open(path, flags, Mode::empty()).ok()?);
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf).ok()?;
    Some((buf, meta.len() > max))
}

/// What latexmk's console output says about BibTeX.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Console {
    /// Databases latexmk did not find, so it did not run BibTeX (from
    /// `Reason: I am configured only to use bibtex/biber if all .bib files
    /// exist, but the following didn't:`).
    pub missing_databases: Vec<String>,
    /// latexmk vetoed a BibTeX rule (`Veto of running of 'bibtex ...'`).
    pub vetoed: bool,
    /// latexmk's error summary has `... Bibtex errors: See file ...`.
    pub bibtex_errors: bool,
}

/// Scans latexmk's stdout and stderr (latexmk 4.86 wording). The document
/// can print arbitrary text to the console too, so this only ever adds
/// diagnostics without a file.
pub(crate) fn scan_console(stdout: &[u8], stderr: &[u8]) -> Console {
    let mut console = Console::default();
    let mut seen = HashSet::new();
    for stream in [stdout, stderr] {
        let text = String::from_utf8_lossy(stream);
        let mut lines = text.lines().map(str::trim_end).peekable();
        while let Some(line) = lines.next() {
            if line.starts_with("Latexmk: Veto of running of 'bibtex ") {
                console.vetoed = true;
            } else if line.starts_with("  bibtex ") && line.contains(": Bibtex errors: See file ") {
                console.bibtex_errors = true;
            } else if line
                == "Reason: I am configured only to use bibtex/biber if all .bib files exist,"
                && lines.next_if_eq(&"but the following didn't:").is_some()
            {
                console.vetoed = true;
                while let Some(name) =
                    lines.next_if(|l| l.starts_with("  ") && !l.trim().is_empty())
                {
                    let name = name.trim().to_owned();
                    if console.missing_databases.len() < MAX_MISSING_DATABASES
                        && seen.insert(name.clone())
                    {
                        console.missing_databases.push(name);
                    }
                }
            }
        }
    }
    console
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_veto_and_errors() {
        let stdout = b"Latexmk: The following rules were vetoed from being run, because of the\n\
            setting for the non-use/condititional use of bibtex/biber:\n  bibtex .texrun/out/main\n\
            Reason: I am configured only to use bibtex/biber if all .bib files exist,\n\
            but the following didn't:\n  nodb.bib\n  other.bib\n\
            Latexmk: Summary of warnings from last run of *latex:\n\
            Reason: I am configured only to use bibtex/biber if all .bib files exist,\n\
            but the following didn't:\n  nodb.bib\n";
        let stderr = b"Latexmk: Veto of running of 'bibtex .texrun/out/main' ($bibtex_use=1)\n";
        let console = scan_console(stdout, stderr);
        assert_eq!(console.missing_databases, ["nodb.bib", "other.bib"]);
        assert!(console.vetoed);
        assert!(!console.bibtex_errors);

        let stdout = b"Collected error summary (may duplicate other messages):\n  \
            bibtex .texrun/out/main: Bibtex errors: See file '.texrun/out/main.blg'\n";
        let console = scan_console(stdout, b"");
        assert!(console.bibtex_errors);
        assert!(!console.vetoed);
        assert_eq!(scan_console(b"", b""), Console::default());
    }

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn blgs_are_found_recursively_and_only_when_fresh() {
        let (_dir, root) = workspace();
        let out = root.join("out");
        fs::create_dir_all(out.join("chapters/deep")).unwrap();
        for f in [
            "main.blg",
            "chapters/deep/x.blg",
            "main.log",
            "chapters/x.BLG",
        ] {
            fs::write(out.join(f), "x").unwrap();
        }
        // Symlinks are not followed.
        std::os::unix::fs::symlink(out.join("main.blg"), out.join("link.blg")).unwrap();
        std::os::unix::fs::symlink(&out, out.join("loop")).unwrap();
        let (found, cut) = find_blgs(&out, "main.blg", SystemTime::UNIX_EPOCH);
        let names: Vec<_> = found
            .iter()
            .map(|p| p.strip_prefix(&out).unwrap().to_str().unwrap().to_owned())
            .collect();
        // `<stem>.blg` first.
        assert_eq!(names, ["main.blg", "chapters/deep/x.blg", "chapters/x.BLG"]);
        assert!(!cut);
        // Files from before the compile are stale.
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        assert!(find_blgs(&out, "main.blg", later).0.is_empty());
    }

    #[test]
    fn blg_count_and_walk_are_bounded() {
        let (_dir, root) = workspace();
        for n in 0..(MAX_BLG_FILES + 3) {
            fs::write(root.join(format!("{n}.blg")), "x").unwrap();
        }
        let (found, cut) = find_blgs(&root, "main.blg", SystemTime::UNIX_EPOCH);
        assert_eq!(found.len(), MAX_BLG_FILES);
        assert!(cut);

        let (_dir, root) = workspace();
        let mut deep = root.clone();
        for _ in 0..(MAX_SCAN_DEPTH + 2) {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("x.blg"), "x").unwrap();
        let (found, cut) = find_blgs(&root, "main.blg", SystemTime::UNIX_EPOCH);
        assert!(found.is_empty());
        assert!(cut);
    }

    #[test]
    fn the_main_log_is_read_even_past_the_file_limit() {
        let (_dir, root) = workspace();
        fs::create_dir(root.join("sub")).unwrap();
        for n in 0..(MAX_BLG_FILES + 3) {
            fs::write(root.join(format!("sub/{n}.blg")), "x").unwrap();
        }
        fs::write(root.join("main.blg"), "x").unwrap();
        let (found, cut) = find_blgs(&root, "main.blg", SystemTime::UNIX_EPOCH);
        assert_eq!(found.len(), MAX_BLG_FILES);
        assert_eq!(found[0], root.join("main.blg"));
        assert!(cut);
    }

    #[test]
    fn diagnostics_are_bounded_for_the_whole_compile() {
        let (_dir, root) = workspace();
        let many = "Warning--empty author in x\n".repeat(150);
        for n in 0..4 {
            fs::write(root.join(format!("{n}.blg")), &many).unwrap();
        }
        let inputs = Inputs {
            root: &root,
            entry_dir: &root,
            entry_dir_rel: None,
        };
        let d = diagnostics(&root, "main", SystemTime::UNIX_EPOCH, &inputs, b"", b"");
        let warnings = d.iter().filter(|d| d.severity == Severity::Warning).count();
        assert_eq!(warnings, MAX_BLG_DIAGNOSTICS);
        // One omitted notice, and one note for the logs not analyzed.
        assert!(d.len() <= MAX_BLG_DIAGNOSTICS + 3, "{}", d.len());
        assert!(d.last().unwrap().message.contains("2 more BibTeX logs"));
    }

    #[test]
    fn read_head_refuses_special_files_and_bounds_the_size() {
        let (_dir, root) = workspace();
        let big = root.join("big.blg");
        fs::write(&big, vec![b'x'; 100]).unwrap();
        assert_eq!(read_head(&big, 10), Some((vec![b'x'; 10], true)));
        assert_eq!(read_head(&big, 100), Some((vec![b'x'; 100], false)));
        let link = root.join("link.blg");
        std::os::unix::fs::symlink(&big, &link).unwrap();
        assert_eq!(read_head(&link, 10), None);
        let fifo = root.join("fifo.blg");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(read_head(&fifo, 10), None);
        assert_eq!(read_head(&root, 10), None);
    }

    #[test]
    fn databases_are_attributed_in_the_entrypoint_directory_only() {
        let (_dir, root) = workspace();
        let paper = root.join("paper");
        fs::create_dir_all(paper.join("bib")).unwrap();
        fs::write(paper.join("refs.bib"), "").unwrap();
        fs::write(paper.join("bib/more.bib"), "").unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.bib"), "").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.bib"), paper.join("secret.bib"))
            .unwrap();
        let rel = WorkspacePath::new("paper").unwrap();
        let inputs = Inputs {
            root: &root,
            entry_dir: &paper,
            entry_dir_rel: Some(&rel),
        };
        let at = |n: &str| {
            inputs
                .locate(&WorkspacePath::new(n).unwrap())
                .map(|p| p.as_str().to_owned())
        };
        assert_eq!(at("refs.bib").as_deref(), Some("paper/refs.bib"));
        assert_eq!(at("bib/more.bib").as_deref(), Some("paper/bib/more.bib"));
        assert_eq!(at("plain.bst"), None);
        assert_eq!(at("secret.bib"), None);
        assert_eq!(at("bib"), None);
    }

    #[test]
    fn diagnostics_combine_logs_and_console() {
        let (_dir, root) = workspace();
        let out = root.join("out");
        fs::create_dir_all(out.join("chapters")).unwrap();
        fs::write(root.join("refs.bib"), "").unwrap();
        fs::write(
            out.join("main.blg"),
            "Database file #1: refs.bib\nRepeated entry---line 7 of file refs.bib\n \
             : @book{x\nI'm skipping whatever remains of this entry\n(There was 1 error message)\n",
        )
        .unwrap();
        fs::write(
            out.join("chapters/one.blg"),
            "Warning--I didn't find a database entry for \"nokey\"\n(There was 1 warning)\n",
        )
        .unwrap();
        let inputs = Inputs {
            root: &root,
            entry_dir: &root,
            entry_dir_rel: None,
        };
        let stdout = b"  bibtex out/main: Bibtex errors: See file 'out/main.blg'\n";
        let d = diagnostics(&out, "main", SystemTime::UNIX_EPOCH, &inputs, stdout, b"");
        let summary: Vec<_> = d
            .iter()
            .map(|d| {
                (
                    d.severity,
                    d.kind,
                    d.file.as_ref().map(WorkspacePath::as_str),
                    d.line,
                )
            })
            .collect();
        // `main.blg` first.
        assert_eq!(
            summary,
            [
                (
                    Severity::Error,
                    DiagnosticKind::BibtexError,
                    Some("refs.bib"),
                    Some(7)
                ),
                (Severity::Info, DiagnosticKind::BibtexFailed, None, None),
                (
                    Severity::Warning,
                    DiagnosticKind::UndefinedCitation,
                    None,
                    None
                ),
            ]
        );

        // latexmk says BibTeX failed, but there is no log.
        let empty = root.join("empty");
        fs::create_dir(&empty).unwrap();
        let d = diagnostics(&empty, "main", SystemTime::UNIX_EPOCH, &inputs, stdout, b"");
        assert_eq!(d.len(), 1);
        assert_eq!(
            (d[0].severity, d[0].kind),
            (Severity::Error, DiagnosticKind::BibtexFailed)
        );

        // A vetoed rule without names.
        let d = diagnostics(
            &empty,
            "main",
            SystemTime::UNIX_EPOCH,
            &inputs,
            b"",
            b"Latexmk: Veto of running of 'bibtex out/main' ($bibtex_use=1)\n",
        );
        assert_eq!(
            (d[0].severity, d[0].kind),
            (Severity::Warning, DiagnosticKind::BibtexFailed)
        );
    }
}
