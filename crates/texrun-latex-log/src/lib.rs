//! Turns TeX / LaTeX engine logs (`main.log`) into structured
//! [`Diagnostic`]s.
//!
//! The parser is a pure function of the log bytes: it runs no processes and
//! touches no files, so it can be tested without a TeX installation. It is
//! deliberately best-effort. It recognizes a fixed set of common patterns,
//! ignores every line it does not understand, never panics and never fails.
//! The full log remains available as a compile artifact, so nothing is lost
//! when a message is not recognized.
//!
//! # Recognized patterns
//!
//! | Log text | [`DiagnosticKind`] | [`Severity`] |
//! | --- | --- | --- |
//! | `Undefined control sequence.` | `UndefinedControlSequence` | error |
//! | `LaTeX Error: File `x.sty' not found.`, `I can't find file` | `MissingFile` | error |
//! | `LaTeX Error: ...`, `Package x Error: ...`, `Class x Error: ...` | `LatexError` | error |
//! | `Emergency stop.`, ` ==> Fatal error occurred` (if no stop was reported) | `EmergencyStop` | error |
//! | any other TeX error (`Missing $ inserted.`, ...) | `Other` | error |
//! | `Overfull \hbox` / `\vbox` | `OverfullBox` | warning |
//! | `Underfull \hbox` / `\vbox` | `UnderfullBox` | warning |
//! | `Reference `x' ... undefined` | `UndefinedReference` | warning |
//! | `Citation `x' ... undefined` | `UndefinedCitation` | warning |
//! | any warning mentioning `Rerun` / `rerun` | `RerunRequired` | info |
//! | other `LaTeX [Font] Warning`, `Package x Warning`, `Class x Warning`, `pdfTeX warning` | `Other` | warning |
//!
//! Errors are recognized both in the `-file-line-error` form
//! (`./main.tex:5: Undefined control sequence.`) and in the classic form
//! (`! Undefined control sequence.` followed by `l.5 ...`). The summary
//! warning `There were undefined references.` is dropped when individual
//! undefined references / citations were reported.
//!
//! # Locations
//!
//! - `line`: the `file:line:` prefix, the `l.<n>` context line, `on input
//!   line <n>` or `at line(s) <n>`. For a missing package or class, TeX
//!   reports the position after LaTeX looked ahead for an optional argument
//!   (usually the next line); that line is only used when it shows the
//!   `\usepackage` / `\documentclass` / ... command, otherwise `line = None`
//!   (the file name is in the message).
//! - `file`: the `file:line:` prefix, otherwise the innermost file of the
//!   `(./chapter1.tex ... )` file stack TeX prints while reading. The stack is
//!   tracked heuristically. Whenever the innermost name is uncertain, `file`
//!   is `None` rather than a guess: names with spaces or parentheses (which
//!   pdfTeX prints unquoted), names that may be cut by line wrapping, and
//!   stacks thrown off by stray `)` in document output (e.g. `\typeout`).
//!   A `(` of document output that is not closed on its line is ignored.
//! - The file is the one TeX was reading, which may be a generated file:
//!   e.g. ``Label `x' multiply defined`` is reported while reading `main.aux`.
//! - File names are converted to workspace paths. Relative names are taken to
//!   be relative to TeX's working directory, which must be the workspace root
//!   (the TeX Live engine runs TeX there). Absolute names are kept only when
//!   they are inside the root given to [`LogParser::with_workspace_root`];
//!   everything else (e.g. installed packages under `texmf-dist`) yields
//!   `file = None`.
//!
//! # Volume
//!
//! Diagnostics are not merged: a line with 60 unsupported Unicode characters
//! yields 60 errors. At most [`LogParser::with_max_diagnostics`] (default
//! [`DEFAULT_MAX_DIAGNOSTICS`]) are returned, errors first, followed by a
//! notice with the number omitted. Aggregating similar diagnostics for
//! display is left to the caller (the CLI).
//!
//! The whole log is decoded into one buffer plus 16 bytes per line, so memory
//! is linear in the log size (about 0.9 GB for 100 MB of very short lines).
//! A streaming parser that keeps only a window of recent lines is possible
//! if that ever matters.
//!
//! # Assumed engine settings
//!
//! The parser works best with the options used by the TeX Live engine:
//! `-file-line-error` and a large `max_print_line` (e.g. `10000`), which keeps
//! TeX from wrapping log lines at 79 bytes. Logs produced without them still
//! parse, with degraded results: wrapped messages are cut, and file names
//! next to a line of exactly 79 bytes are dropped rather than guessed. When a
//! log is known to be wrapped at `n` bytes, [`LogParser::with_max_print_line`]
//! rejoins the wrapped lines.
//!
//! # Example
//!
//! ```
//! use texrun_core::DiagnosticKind;
//! use texrun_latex_log::parse_log;
//!
//! let log = b"(./main.tex\n./main.tex:5: Undefined control sequence.\nl.5 \\foo\n         {bar}\n";
//! let diagnostics = parse_log(log);
//! assert_eq!(diagnostics[0].kind, DiagnosticKind::UndefinedControlSequence);
//! assert_eq!(diagnostics[0].message, "Undefined control sequence \\foo");
//! assert_eq!(diagnostics[0].file.as_ref().unwrap().as_str(), "main.tex");
//! assert_eq!(diagnostics[0].line, Some(5));
//! ```

mod lines;
mod parser;
mod patterns;
mod stack;

pub use parser::ParsedLog;
use texrun_core::WorkspaceRoot;
pub use texrun_core::{Diagnostic, DiagnosticKind, Severity};

/// TeX's default `max_print_line`.
const TEX_DEFAULT_MAX_PRINT_LINE: usize = 79;

/// Default of [`LogParser::with_max_diagnostics`].
pub const DEFAULT_MAX_DIAGNOSTICS: usize = 1000;

/// Configurable log parser. [`parse_log`] is a shortcut for the default
/// configuration.
#[derive(Debug, Clone)]
pub struct LogParser {
    workspace_root: Option<WorkspaceRoot>,
    max_print_line: Option<usize>,
    max_diagnostics: usize,
}

impl Default for LogParser {
    fn default() -> Self {
        Self {
            workspace_root: None,
            max_print_line: None,
            max_diagnostics: DEFAULT_MAX_DIAGNOSTICS,
        }
    }
}

impl LogParser {
    /// A parser with the default configuration: no workspace root (absolute
    /// file names are never attributed), unknown `max_print_line` and at
    /// most [`DEFAULT_MAX_DIAGNOSTICS`] diagnostics.
    pub fn new() -> Self {
        Self::default()
    }

    /// The absolute workspace root as TeX saw it (i.e. TeX's working
    /// directory, spelled the same way: if the engine canonicalizes the root
    /// before running TeX, pass the canonical path). Absolute file names in
    /// the log below it are converted to workspace paths.
    #[must_use]
    pub fn with_workspace_root(mut self, root: &WorkspaceRoot) -> Self {
        self.workspace_root = Some(root.clone());
        self
    }

    /// The `max_print_line` the log was written with. Lines of exactly this
    /// many bytes are treated as wrapped and joined with the following line.
    /// Pass the value the engine set (`0` disables joining).
    #[must_use]
    pub fn with_max_print_line(mut self, max_print_line: usize) -> Self {
        self.max_print_line = Some(max_print_line);
        self
    }

    /// The maximum number of diagnostics returned. Errors are kept in
    /// preference to warnings and info; beyond the limit, diagnostics are
    /// dropped (later ones first) and a single notice with the number
    /// omitted is appended (see [`ParsedLog`]). Within each group, the first
    /// ones in log order are kept; the variety of kinds is not considered, so
    /// a small limit may be filled by repetitions of one message.
    #[must_use]
    pub fn with_max_diagnostics(mut self, max_diagnostics: usize) -> Self {
        self.max_diagnostics = max_diagnostics;
        self
    }

    /// Parses a log. Never fails: unrecognized or malformed input (including
    /// invalid UTF-8) yields fewer diagnostics, not an error.
    pub fn parse(&self, log: &[u8]) -> ParsedLog {
        let lines = lines::split(log, self.max_print_line);
        let config = parser::Config {
            workspace_root: self.workspace_root.as_ref(),
            suspect_wrap_width: match self.max_print_line {
                Some(_) => None,
                None => Some(TEX_DEFAULT_MAX_PRINT_LINE),
            },
            max_diagnostics: self.max_diagnostics,
        };
        parser::parse(&lines, &config)
    }
}

/// Parses a log with the default [`LogParser`] configuration and returns the
/// diagnostics (including the notice about omitted ones, if any).
pub fn parse_log(log: &[u8]) -> Vec<Diagnostic> {
    LogParser::new().parse(log).diagnostics
}
