//! Workspace-relative paths.
//!
//! Every path that crosses the core API is relative, never a host path:
//!
//! - entrypoints, [`CompileOptions::output_dir`](crate::CompileOptions::output_dir)
//!   and [`Diagnostic::file`](crate::Diagnostic::file) are relative to the
//!   workspace root (= the input project root);
//! - [`Artifact::path`](crate::Artifact::path) is relative to the output root.
//!
//! Resolving them to host paths is up to the layer that knows the host
//! locations (the workspace layer #4 and the CLI #6). [`WorkspacePath`]
//! enforces relativity *lexically* at construction time: absolute paths, drive
//! prefixes and `..` components are rejected, so a value of this type can never
//! name a location outside its base directory by itself.
//!
//! Filesystem-level concerns such as symlinks pointing outside the workspace
//! cannot be decided without touching the disk and are handled by the
//! workspace layer (#4), which resolves a `WorkspacePath` against a concrete
//! root directory.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Reason a string or path was rejected as a [`WorkspacePath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WorkspacePathError {
    /// The path is empty or consists only of `.` / separators.
    #[error("workspace path is empty")]
    Empty,
    /// The path is absolute or has a root / drive prefix.
    #[error("workspace path must be relative: {0:?}")]
    Absolute(String),
    /// The path contains a `..` component.
    #[error("workspace path must not contain `..`: {0:?}")]
    ParentTraversal(String),
    /// The path contains a character that is not allowed (a control
    /// character or `\`).
    #[error("workspace path contains a forbidden character {ch:?}: {path:?}")]
    ForbiddenCharacter {
        /// The offending path.
        path: String,
        /// The forbidden character.
        ch: char,
    },
    /// The path is not valid UTF-8.
    #[error("workspace path is not valid UTF-8: {0:?}")]
    NonUtf8(PathBuf),
}

/// A normalized path relative to the compile workspace root.
///
/// Invariants (checked on construction):
///
/// - non-empty and relative (no leading `/`, no Windows drive or UNC prefix);
/// - no `..` components;
/// - no control characters (C0, DEL, C1): they would allow terminal escape
///   injection when paths are displayed and cannot be reported reliably in
///   `file:line:` style logs;
/// - no `\` (rejected so the meaning of a path does not depend on the host
///   platform);
/// - valid UTF-8, so it serializes losslessly to JSON.
///
/// The stored form uses `/` as separator with `.` and empty components removed,
/// e.g. `./chapters//intro.tex` is stored as `chapters/intro.tex`.
///
/// Equality, ordering and hashing compare the stored bytes. No Unicode
/// normalization is applied, so `caf\u{e9}.tex` and `cafe\u{301}.tex` are
/// different values even though some filesystems (e.g. APFS) treat them as
/// the same file.
///
/// A path may begin with `-` (e.g. `-draft.tex`), which a command-line tool
/// would parse as an option. When passing a path as a process argument, use
/// [`WorkspacePath::to_cli_arg`] instead of [`WorkspacePath::as_str`].
///
/// What a path is relative to depends on where it appears (the workspace root
/// for inputs and diagnostics, the output root for artifacts); see the
/// documentation of the containing field.
///
/// Serialized as a plain JSON string; deserialization re-validates and
/// normalizes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspacePath(String);

impl WorkspacePath {
    /// Validates and normalizes `path`.
    pub fn new(path: &str) -> Result<Self, WorkspacePathError> {
        if let Some(ch) = path.chars().find(|&c| c.is_control() || c == '\\') {
            return Err(WorkspacePathError::ForbiddenCharacter {
                path: path.to_owned(),
                ch,
            });
        }
        if path.starts_with('/') || has_drive_prefix(path) {
            return Err(WorkspacePathError::Absolute(path.to_owned()));
        }

        let mut parts = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => return Err(WorkspacePathError::ParentTraversal(path.to_owned())),
                normal => parts.push(normal),
            }
        }
        if parts.is_empty() {
            return Err(WorkspacePathError::Empty);
        }
        Ok(Self(parts.join("/")))
    }

    /// Converts a host [`Path`] that is meant to be relative to the workspace.
    pub fn from_path(path: &Path) -> Result<Self, WorkspacePathError> {
        let Some(text) = path.to_str() else {
            return Err(WorkspacePathError::NonUtf8(path.to_path_buf()));
        };
        // Catch platform-specific absolute forms (e.g. `C:\` on Windows) that
        // the string check alone might classify differently.
        if path
            .components()
            .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
        {
            return Err(WorkspacePathError::Absolute(text.to_owned()));
        }
        Self::new(text)
    }

    /// Returns the normalized `/`-separated form. May start with `-`; see
    /// [`WorkspacePath::to_cli_arg`].
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the path as a [`Path`] (still relative; join it onto a root, or
    /// use [`WorkspaceRoot::resolve`](crate::WorkspaceRoot::resolve)).
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// Returns the path prefixed with `./`, safe to pass as a positional
    /// process argument (it can never be mistaken for an option such as
    /// `-shell-escape`). Engines building argv (#5) must use this for
    /// relative paths.
    pub fn to_cli_arg(&self) -> String {
        format!("./{}", self.0)
    }

    /// Returns the final component.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Returns the final component without its extension (the part after the
    /// last `.`, unless the name starts with that `.`).
    pub fn file_stem(&self) -> &str {
        let name = self.file_name();
        match name.rfind('.') {
            Some(0) | None => name,
            Some(idx) => &name[..idx],
        }
    }

    /// Appends a relative path. Both sides are already validated, so the
    /// result is always a valid workspace path.
    #[must_use]
    pub fn join(&self, other: &Self) -> Self {
        Self(format!("{}/{}", self.0, other.0))
    }
}

/// `C:`, `c:foo` etc. — meaningful as a drive prefix on Windows.
///
/// This is only a guard against Windows-style absolute input being accepted by
/// mistake; it checks the start of the path only. `:` elsewhere (`a/C:b`,
/// `main.tex:ads`) is allowed, since Windows hosts are not supported.
fn has_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<Path> for WorkspacePath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl FromStr for WorkspacePath {
    type Err = WorkspacePathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<&str> for WorkspacePath {
    type Error = WorkspacePathError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for WorkspacePath {
    type Error = WorkspacePathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<WorkspacePath> for String {
    fn from(value: WorkspacePath) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wp(s: &str) -> WorkspacePath {
        WorkspacePath::new(s).unwrap()
    }

    #[test]
    fn accepts_and_normalizes_nested_paths() {
        assert_eq!(wp("main.tex").as_str(), "main.tex");
        assert_eq!(wp("chapters/intro.tex").as_str(), "chapters/intro.tex");
        assert_eq!(wp("./chapters//intro.tex/").as_str(), "chapters/intro.tex");
        assert_eq!(wp("a/./b").as_str(), "a/b");
    }

    #[test]
    fn rejects_empty() {
        for input in ["", ".", "./", ".//./"] {
            assert_eq!(WorkspacePath::new(input), Err(WorkspacePathError::Empty));
        }
    }

    #[test]
    fn rejects_absolute_and_drive_prefixes() {
        for input in ["/etc/passwd", "//server/share", "C:/x", "c:foo"] {
            assert!(
                matches!(
                    WorkspacePath::new(input),
                    Err(WorkspacePathError::Absolute(_))
                ),
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_parent_traversal_anywhere() {
        for input in ["..", "../secret", "a/../../b", "a/.."] {
            assert!(
                matches!(
                    WorkspacePath::new(input),
                    Err(WorkspacePathError::ParentTraversal(_))
                ),
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_forbidden_characters() {
        assert!(matches!(
            WorkspacePath::new("a\\..\\b"),
            Err(WorkspacePathError::ForbiddenCharacter { ch: '\\', .. })
        ));
        assert!(matches!(
            WorkspacePath::new("a\0b"),
            Err(WorkspacePathError::ForbiddenCharacter { ch: '\0', .. })
        ));
    }

    #[test]
    fn rejects_control_characters() {
        for (input, ch) in [
            ("a\nb.tex", '\n'),
            ("a\tb.tex", '\t'),
            ("\u{1b}[31mx.tex", '\u{1b}'),
            ("a\u{7f}.tex", '\u{7f}'),
            ("a\u{85}.tex", '\u{85}'),
        ] {
            assert_eq!(
                WorkspacePath::new(input),
                Err(WorkspacePathError::ForbiddenCharacter {
                    path: input.to_owned(),
                    ch
                }),
                "{input:?}"
            );
        }
        assert!(serde_json::from_str::<WorkspacePath>(r#""a\u001b.tex""#).is_err());
    }

    #[test]
    fn allows_leading_dash_but_cli_arg_neutralizes_it() {
        let p = wp("-shell-escape");
        assert_eq!(p.as_str(), "-shell-escape");
        assert_eq!(p.to_cli_arg(), "./-shell-escape");
        assert_eq!(wp("sub/main.tex").to_cli_arg(), "./sub/main.tex");
    }

    #[test]
    fn no_unicode_normalization() {
        assert_ne!(wp("caf\u{e9}.tex"), wp("cafe\u{301}.tex"));
    }

    #[test]
    fn from_path_rejects_absolute_host_paths() {
        let abs = std::env::current_dir().unwrap().join("main.tex");
        assert!(matches!(
            WorkspacePath::from_path(&abs),
            Err(WorkspacePathError::Absolute(_))
        ));
        assert_eq!(
            WorkspacePath::from_path(Path::new("sub/main.tex")).unwrap(),
            wp("sub/main.tex")
        );
    }

    #[test]
    fn name_helpers() {
        let p = wp("chapters/intro.tex");
        assert_eq!(p.file_name(), "intro.tex");
        assert_eq!(p.file_stem(), "intro");
        assert_eq!(wp(".latexmkrc").file_stem(), ".latexmkrc");
        assert_eq!(wp("archive.tar.gz").file_stem(), "archive.tar");
        assert_eq!(wp("out").join(&p).as_str(), "out/chapters/intro.tex");
    }

    #[test]
    fn serde_is_a_plain_string_and_revalidates() {
        let p = wp("./a//b.tex");
        assert_eq!(serde_json::to_string(&p).unwrap(), r#""a/b.tex""#);
        let back: WorkspacePath = serde_json::from_str(r#""a/b.tex""#).unwrap();
        assert_eq!(back, p);
        let normalized: WorkspacePath = serde_json::from_str(r#""./a//b.tex/""#).unwrap();
        assert_eq!(normalized.as_str(), "a/b.tex");
        assert!(serde_json::from_str::<WorkspacePath>(r#""../x""#).is_err());
        assert!(serde_json::from_str::<WorkspacePath>(r#""/x""#).is_err());
    }
}
