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
//!   line <n>` or `at line(s) <n>`.
//! - `file`: the `file:line:` prefix, otherwise the innermost file of the
//!   `(./chapter1.tex ... )` file stack TeX prints while reading. The stack is
//!   tracked heuristically and may be wrong for logs with unbalanced
//!   parentheses in unrecognized output.
//! - File names are converted to workspace paths. Relative names are taken to
//!   be relative to TeX's working directory, which must be the workspace root
//!   (the TeX Live engine runs TeX there). Absolute names are kept only when
//!   they are inside the root given to [`LogParser::with_workspace_root`];
//!   everything else (e.g. installed packages under `texmf-dist`) yields
//!   `file = None`.
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

use texrun_core::WorkspaceRoot;
pub use texrun_core::{Diagnostic, DiagnosticKind, Severity};

/// TeX's default `max_print_line`.
const TEX_DEFAULT_MAX_PRINT_LINE: usize = 79;

/// Configurable log parser. [`parse_log`] is a shortcut for the default
/// configuration.
#[derive(Debug, Clone, Default)]
pub struct LogParser {
    workspace_root: Option<WorkspaceRoot>,
    max_print_line: Option<usize>,
}

impl LogParser {
    /// A parser with the default configuration: no workspace root (absolute
    /// file names are never attributed) and unknown `max_print_line`.
    pub fn new() -> Self {
        Self::default()
    }

    /// The absolute workspace root as TeX saw it. Absolute file names in the
    /// log below it are converted to workspace paths.
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

    /// Parses a log. Never fails: unrecognized or malformed input (including
    /// invalid UTF-8) yields fewer diagnostics, not an error.
    pub fn parse(&self, log: &[u8]) -> Vec<Diagnostic> {
        let lines = lines::split(log, self.max_print_line);
        let config = parser::Config {
            workspace_root: self.workspace_root.as_ref(),
            suspect_wrap_width: match self.max_print_line {
                Some(_) => None,
                None => Some(TEX_DEFAULT_MAX_PRINT_LINE),
            },
        };
        parser::parse(&lines, &config)
    }
}

/// Parses a log with the default [`LogParser`] configuration.
pub fn parse_log(log: &[u8]) -> Vec<Diagnostic> {
    LogParser::new().parse(log)
}
