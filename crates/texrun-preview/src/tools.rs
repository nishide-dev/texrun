//! Detection of the external preview tools.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::options::BackendChoice;
use crate::report::BackendKind;

/// Executable names looked up in the search path.
pub const PDFTOPPM: &str = "pdftoppm";
/// See [`PDFTOPPM`].
pub const PDFINFO: &str = "pdfinfo";
/// See [`PDFTOPPM`].
pub const MUTOOL: &str = "mutool";

/// The preview tools found on a search path.
///
/// Tools are resolved to absolute paths once, at detection time, and started
/// by that path. Relative entries in the search path (such as `.` or an empty
/// entry) are ignored, so that a tool is never picked up from the current
/// directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Toolset {
    search_path: Option<OsString>,
    pdftoppm: Option<PathBuf>,
    pdfinfo: Option<PathBuf>,
    mutool: Option<PathBuf>,
}

/// A backend with the absolute paths of its programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend<'a> {
    Poppler {
        pdfinfo: &'a Path,
        pdftoppm: &'a Path,
    },
    Mupdf {
        mutool: &'a Path,
    },
}

impl Backend<'_> {
    pub(crate) fn kind(self) -> BackendKind {
        match self {
            Self::Poppler { .. } => BackendKind::Poppler,
            Self::Mupdf { .. } => BackendKind::Mupdf,
        }
    }
}

impl Toolset {
    /// Looks the tools up in the `PATH` of this process.
    pub fn detect() -> Self {
        Self::from_search_path(env::var_os("PATH").as_deref())
    }

    /// Looks the tools up in `search_path` (a `PATH`-style list). Its
    /// absolute entries, in order, are also passed to the tools as their
    /// `PATH` (the tools do not start other programs; relative entries would
    /// be resolved against their working directory).
    pub fn from_search_path(search_path: Option<&OsStr>) -> Self {
        let absolute = search_path.and_then(|p| {
            env::join_paths(env::split_paths(p).filter(|dir| dir.is_absolute())).ok()
        });
        let find = |name| absolute.as_deref().and_then(|p| find_executable(p, name));
        Self {
            search_path: absolute.clone(),
            pdftoppm: find(PDFTOPPM),
            pdfinfo: find(PDFINFO),
            mutool: find(MUTOOL),
        }
    }

    /// The tools at `found` (absolute paths, named like the tools), with
    /// `search_path` as their `PATH`, without looking at this host: the
    /// tools of a container image.
    pub(crate) fn at(search_path: &str, found: &[PathBuf]) -> Self {
        let find = |name: &str| {
            found
                .iter()
                .find(|p| p.is_absolute() && p.file_name().is_some_and(|n| n == name))
                .cloned()
        };
        Self {
            search_path: Some(search_path.into()),
            pdftoppm: find(PDFTOPPM),
            pdfinfo: find(PDFINFO),
            mutool: find(MUTOOL),
        }
    }

    /// A toolset without any tool (every run is skipped with
    /// [`NoticeKind::ToolUnavailable`](crate::NoticeKind::ToolUnavailable)).
    pub fn none() -> Self {
        Self::default()
    }

    /// The `PATH` value handed to the tools.
    pub fn search_path(&self) -> Option<&OsStr> {
        self.search_path.as_deref()
    }

    /// Whether `kind` is usable (all of its programs were found).
    pub fn has(&self, kind: BackendKind) -> bool {
        match kind {
            BackendKind::Poppler => self.pdfinfo.is_some() && self.pdftoppm.is_some(),
            BackendKind::Mupdf => self.mutool.is_some(),
        }
    }

    /// The usable backends in [`BackendChoice::Auto`] preference order.
    pub fn available(&self) -> Vec<BackendKind> {
        [BackendKind::Mupdf, BackendKind::Poppler]
            .into_iter()
            .filter(|&k| self.has(k))
            .collect()
    }

    /// Picks the backend for `choice`, or explains why none is usable.
    pub(crate) fn select(&self, choice: BackendChoice) -> Result<Backend<'_>, String> {
        let poppler = || match (&self.pdfinfo, &self.pdftoppm) {
            (Some(pdfinfo), Some(pdftoppm)) => Some(Backend::Poppler { pdfinfo, pdftoppm }),
            _ => None,
        };
        let mupdf = || {
            self.mutool
                .as_deref()
                .map(|mutool| Backend::Mupdf { mutool })
        };
        match choice {
            BackendChoice::Poppler => poppler()
                .ok_or_else(|| format!("{PDFINFO} and {PDFTOPPM} (Poppler) were not found")),
            BackendChoice::Mupdf => {
                mupdf().ok_or_else(|| format!("{MUTOOL} (MuPDF) was not found"))
            }
            BackendChoice::Auto => mupdf().or_else(poppler).ok_or_else(|| {
                format!(
                    "no PDF preview tool was found: install MuPDF ({MUTOOL}) \
                     or Poppler ({PDFINFO} + {PDFTOPPM})"
                )
            }),
        }
    }
}

/// Finds `name` as an executable regular file in an absolute entry of
/// `search_path`.
pub(crate) fn find_executable(search_path: &OsStr, name: &str) -> Option<PathBuf> {
    env::split_paths(search_path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    // Follows symlinks: package managers (e.g. Homebrew) install symlinks.
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn touch(dir: &Path, name: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn join(dirs: &[&Path]) -> OsString {
        env::join_paths(dirs).unwrap()
    }

    #[test]
    fn finds_executables_in_search_order() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        touch(a.path(), MUTOOL, 0o644); // not executable: skipped
        let mutool = touch(b.path(), MUTOOL, 0o755);
        let pdfinfo_a = touch(a.path(), PDFINFO, 0o755);
        touch(b.path(), PDFINFO, 0o755);
        fs::create_dir(a.path().join(PDFTOPPM)).unwrap(); // a directory: skipped

        let path = join(&[a.path(), b.path()]);
        let tools = Toolset::from_search_path(Some(&path));
        assert_eq!(tools.mutool.as_deref(), Some(mutool.as_path()));
        assert_eq!(tools.pdfinfo.as_deref(), Some(pdfinfo_a.as_path()));
        assert_eq!(tools.pdftoppm, None);
        assert_eq!(tools.search_path(), Some(path.as_os_str()));
        // Poppler needs both programs.
        assert_eq!(tools.available(), vec![BackendKind::Mupdf]);
    }

    #[test]
    fn ignores_relative_search_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), MUTOOL, 0o755);
        // Relative entries would be resolved against the current directory.
        let rel = OsString::from(".:relative/bin:");
        assert_eq!(find_executable(&rel, MUTOOL), None);

        // They are not passed on to the tools either.
        let mut mixed = OsString::from(".:");
        mixed.push(dir.path());
        mixed.push(":relative/bin::/usr/bin");
        let tools = Toolset::from_search_path(Some(&mixed));
        let expected = join(&[dir.path(), Path::new("/usr/bin")]);
        assert_eq!(tools.search_path(), Some(expected.as_os_str()));
        assert!(tools.has(BackendKind::Mupdf));
    }

    #[test]
    fn selection_prefers_mupdf_and_explains_missing_tools() {
        let dir = tempfile::tempdir().unwrap();
        for name in [PDFINFO, PDFTOPPM, MUTOOL] {
            touch(dir.path(), name, 0o755);
        }
        let path = join(&[dir.path()]);
        let all = Toolset::from_search_path(Some(&path));
        assert_eq!(
            all.available(),
            vec![BackendKind::Mupdf, BackendKind::Poppler]
        );
        assert_eq!(
            all.select(BackendChoice::Auto).unwrap().kind(),
            BackendKind::Mupdf
        );
        assert_eq!(
            all.select(BackendChoice::Poppler).unwrap().kind(),
            BackendKind::Poppler
        );

        let only_poppler = Toolset {
            mutool: None,
            ..all.clone()
        };
        assert_eq!(
            only_poppler.select(BackendChoice::Auto).unwrap().kind(),
            BackendKind::Poppler
        );
        assert!(
            only_poppler
                .select(BackendChoice::Mupdf)
                .unwrap_err()
                .contains(MUTOOL)
        );
        let no_pdfinfo = Toolset {
            pdfinfo: None,
            ..all.clone()
        };
        assert!(
            no_pdfinfo
                .select(BackendChoice::Poppler)
                .unwrap_err()
                .contains(PDFINFO)
        );

        let none = Toolset::none();
        assert!(none.available().is_empty());
        let msg = none.select(BackendChoice::Auto).unwrap_err();
        assert!(msg.contains("Poppler") && msg.contains("MuPDF"), "{msg}");
    }
}
