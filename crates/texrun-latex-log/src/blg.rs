//! BibTeX logs (`.blg`).
//!
//! BibTeX writes its messages to `<aux stem>.blg` (and the terminal). The
//! format is simpler than TeX's: every message BibTeX prints itself starts a
//! line, the text it quotes from an input file is on lines starting with
//! ` : `, and a location is either on the message line or on the next one:
//!
//! ```text
//! Database file #1: refs.bib
//! I was expecting a `,' or a `}'---line 4 of file refs.bib
//!  :
//!  :   year = {1984}
//! (Error may have been on previous line)
//! I'm skipping whatever remains of this entry
//! I couldn't open style file nostyle.bst
//! ---line 4 of file main.aux
//!  : \bibstyle{nostyle
//!  :                  }
//! I'm skipping whatever remains of this command
//! Warning--I didn't find a database entry for "nokey"
//! Warning--string name "acm" is undefined
//! --line 5 of file refs.bib
//! (There were 2 error messages)
//! ```
//!
//! See [`BlgParser`] for what is recognized and how locations are chosen.

use std::collections::{HashMap, HashSet};

use texrun_core::{Diagnostic, DiagnosticKind, Severity, WorkspacePath};

use crate::parser::{
    Collector, MAX_EXCERPT_BYTES, MAX_EXCERPT_LINES, MAX_MESSAGE_BYTES, prefix, sanitize,
};

/// At most this many lines after a message are consumed as its location,
/// quoted input, `(Error may have been on previous line)` and `I'm skipping
/// ...` lines.
const MAX_TRAILING_LINES: usize = 8;

/// At most this many database / style names are remembered from the
/// `Database file #n:` / `The style file:` lines; later ones are never
/// attributed. A real document names a handful.
const MAX_KNOWN_FILES: usize = 64;

/// Resolves the database and style names BibTeX printed to workspace files,
/// for [`BlgParser::parse_with_files`].
///
/// Implemented for closures `Fn(&WorkspacePath) -> Option<WorkspacePath>`.
pub trait BibFiles {
    /// The workspace path of the file BibTeX opened as `name` (the name as
    /// printed, e.g. `refs.bib` or `bib/more.bib`, which BibTeX looked up
    /// in its search path), or `None` if that is not certain (e.g. an
    /// installed style, or a name that could be found in several places).
    /// Called at most once per name and log.
    fn locate(&self, name: &WorkspacePath) -> Option<WorkspacePath>;
}

impl<F: Fn(&WorkspacePath) -> Option<WorkspacePath>> BibFiles for F {
    fn locate(&self, name: &WorkspacePath) -> Option<WorkspacePath> {
        self(name)
    }
}

/// No files are attributed.
struct NoFiles;

impl BibFiles for NoFiles {
    fn locate(&self, _: &WorkspacePath) -> Option<WorkspacePath> {
        None
    }
}

/// The result of parsing a BibTeX log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ParsedBlg {
    /// The diagnostics in log order, then the [`DiagnosticKind::BibtexFailed`]
    /// summary if BibTeX failed, then the notice about omitted diagnostics
    /// (see [`crate::ParsedLog::diagnostics`]) if any were.
    pub diagnostics: Vec<Diagnostic>,
    /// Number of recognized diagnostics dropped because of the limit.
    pub omitted: usize,
    /// BibTeX reported errors (in its summary line, or as recognized
    /// messages) or a fatal error.
    pub failed: bool,
    /// `N` of BibTeX's `(There were N error messages)` summary, if present.
    pub error_messages: Option<u32>,
}

/// Parser for BibTeX logs (`.blg`). [`parse_blg`] is a shortcut for the
/// default configuration.
///
/// Like the TeX log parser it is a pure, best-effort function of the input:
/// it never fails, never panics and ignores lines it does not understand.
///
/// # Recognized messages
///
/// | `.blg` text | [`DiagnosticKind`] | [`Severity`] |
/// | --- | --- | --- |
/// | `I couldn't open database file x.bib`, `... style file x.bst`, `... auxiliary file x.aux`, `I couldn't open file name` | `MissingFile` | error |
/// | `<message>---line N of file x.bib` (also with the location on the next line), `<message>---while reading file x.aux`, `Sorry---...` | `BibtexError` | error |
/// | `Warning--I didn't find a database entry for "key"` | `UndefinedCitation` | warning |
/// | any other `Warning--...` (`empty author in key`, ...) | `Other` | warning |
/// | `(There were N error messages)`, `(That was a fatal error)`, or any error above | `BibtexFailed` | info after an error; error otherwise |
///
/// Messages are prefixed with `BibTeX: ` / `BibTeX warning: `.
///
/// Like latexmk (4.86, `check_bibtex_log`), `I found no \citation commands`
/// (a document without `\cite` yet) is only a warning, and a log whose
/// errors are all such "weak" errors (also a missing `.aux` or database
/// file) is not a failure: those messages are warnings and there is no
/// `BibtexFailed`. A missing database with nothing else (`I found no
/// database files` follows) stays an error, as for latexmk. For a file
/// that exists but that kpathsea's paranoid mode refuses (absolute, `..`
/// or a component starting with `.`), the message says so.
///
/// # Locations
///
/// - `file` is set only for a `.bib` / `.bst` file that the log itself names
///   as one of the databases (`Database file #n: x.bib`) or the style
///   (`The style file: x.bst`), and that [`BibFiles::locate`] resolves.
///   Locations in `.aux` files (generated by LaTeX, e.g. of a missing
///   database) are not reported. Without [`BlgParser::parse_with_files`]
///   no file is set.
/// - `line` is set only together with `file`, from `---line N`: the line
///   BibTeX was reading when it found the error, i.e. the line of the
///   unexpected token. When BibTeX adds `(Error may have been on previous
///   line)` (the token starts its line, so e.g. a missing `,` at the end of
///   the previous line is likely), the message says so.
/// - `line` is `None` (the file is kept, and the message says where BibTeX
///   noticed and what is likely wrong) when BibTeX read on past the
///   mistake before noticing: the quoted text after the position starts
///   with `@` (it reached the next entry, so the entry before is not closed
///   or has an unbalanced `{` / `"`; also a `@string` without its closing
///   brace, which cannot be told apart), `Unbalanced braces`, `Illegal end
///   of database file`, and every error in a `.bst` style.
/// - Of the warnings with a `--line N`, only `string name "x" is undefined`
///   keeps it: BibTeX reports the other ones (e.g. `I'm ignoring key's
///   extra "year" field`) after looking ahead past the field, so `N` may be
///   a later line.
///
/// # Volume
///
/// Parsing is linear in the log size: every line is looked at once, plus a
/// look-ahead of a constant number of lines after a message. At most
/// [`BlgParser::with_max_diagnostics`] diagnostics are kept (errors first),
/// at most 64 database / style names are remembered, and
/// [`BibFiles::locate`] is called at most once per name. The whole log is
/// decoded into memory; callers should bound its size.
#[derive(Debug, Clone)]
pub struct BlgParser {
    max_diagnostics: usize,
}

impl Default for BlgParser {
    fn default() -> Self {
        Self {
            max_diagnostics: crate::DEFAULT_MAX_DIAGNOSTICS,
        }
    }
}

impl BlgParser {
    /// A parser with the default configuration: at most
    /// [`DEFAULT_MAX_DIAGNOSTICS`](crate::DEFAULT_MAX_DIAGNOSTICS)
    /// diagnostics.
    pub fn new() -> Self {
        Self::default()
    }

    /// The maximum number of diagnostics returned, like
    /// [`LogParser::with_max_diagnostics`](crate::LogParser::with_max_diagnostics).
    /// The [`DiagnosticKind::BibtexFailed`] summary does not count.
    #[must_use]
    pub fn with_max_diagnostics(mut self, max_diagnostics: usize) -> Self {
        self.max_diagnostics = max_diagnostics;
        self
    }

    /// Parses a BibTeX log without attributing files.
    pub fn parse(&self, blg: &[u8]) -> ParsedBlg {
        self.parse_with_files(blg, &NoFiles)
    }

    /// Parses a BibTeX log, attributing messages in `.bib` / `.bst` files
    /// through `files` (see the Locations section).
    pub fn parse_with_files(&self, blg: &[u8], files: &dyn BibFiles) -> ParsedBlg {
        let text = String::from_utf8_lossy(blg);
        let lines: Vec<&str> = text
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect();
        let mut parser = Parser {
            lines: &lines,
            files,
            known: HashSet::new(),
            located: HashMap::new(),
            out: Collector::new(self.max_diagnostics),
            errors_reported: 0,
            weak_only: weak_only(&lines),
        };
        parser.run()
    }
}

/// Parses a BibTeX log with the default [`BlgParser`] configuration.
pub fn parse_blg(blg: &[u8]) -> Vec<Diagnostic> {
    BlgParser::new().parse(blg).diagnostics
}

struct Parser<'a> {
    lines: &'a [&'a str],
    files: &'a dyn BibFiles,
    /// Database and style names the log announced.
    known: HashSet<&'a str>,
    /// Cache of [`BibFiles::locate`].
    located: HashMap<&'a str, Option<WorkspacePath>>,
    out: Collector,
    errors_reported: usize,
    /// All errors BibTeX counted are weak (see [`weak_only`]).
    weak_only: bool,
}

/// Where BibTeX says a message happened.
#[derive(Debug, Clone, Copy)]
struct Location<'a> {
    line: Option<u32>,
    file: &'a str,
}

impl<'a> Parser<'a> {
    fn text(&self, i: usize) -> &'a str {
        let lines: &'a [&'a str] = self.lines;
        lines.get(i).map_or("", |l| l.trim_end())
    }

    fn run(&mut self) -> ParsedBlg {
        let mut error_messages = None;
        let mut fatal = false;
        let mut summary_line = None;
        let mut i = 0;
        while i < self.lines.len() {
            let t = self.text(i);
            if let Some(rest) = t.strip_prefix("Database file #") {
                if let Some((_, name)) = rest.split_once(": ") {
                    self.remember(name);
                }
            } else if let Some(name) = t.strip_prefix("The style file: ") {
                self.remember(name);
            } else if let Some(body) = t.strip_prefix("Warning--") {
                i = self.warning(i, body);
                continue;
            } else if let Some(n) = error_count(t) {
                error_messages = Some(n);
                summary_line = Some(i);
            } else if t == "(That was a fatal error)" {
                fatal = true;
                summary_line = summary_line.or(Some(i));
            } else if let Some(end) = self.error(i) {
                i = end;
                continue;
            }
            i += 1;
        }

        // latexmk takes a log with only weak errors as a success.
        let failed = fatal
            || !self.weak_only
                && (error_messages.is_some_and(|n| n > 0) || self.errors_reported > 0);
        let mut parsed = std::mem::replace(&mut self.out, Collector::new(0)).finish();
        let summary = failed.then(|| {
            let severity = if self.errors_reported > 0 {
                Severity::Info
            } else {
                Severity::Error
            };
            let message = match error_messages {
                _ if fatal => "BibTeX stopped with a fatal error; the bibliography is incomplete \
                               or missing"
                    .to_owned(),
                Some(n) if n > 0 => format!(
                    "BibTeX failed with {n} error message{}; the bibliography is incomplete",
                    if n == 1 { "" } else { "s" }
                ),
                _ => "BibTeX failed; the bibliography is incomplete".to_owned(),
            };
            let mut d = Diagnostic::new(severity, DiagnosticKind::BibtexFailed, message);
            if let Some(line) = summary_line {
                d = d.with_raw_excerpt(self.excerpt(line, line + 1));
            }
            d
        });
        if let Some(summary) = summary {
            // Before the notice about omitted diagnostics.
            let at = parsed.diagnostics.len() - usize::from(parsed.omitted > 0);
            parsed.diagnostics.insert(at, summary);
        }
        ParsedBlg {
            diagnostics: parsed.diagnostics,
            omitted: parsed.omitted,
            failed,
            error_messages,
        }
    }

    fn remember(&mut self, name: &'a str) {
        if self.known.len() < MAX_KNOWN_FILES {
            self.known.insert(name.trim());
        }
    }

    /// Handles an error message starting at line `i`; returns the index of
    /// the first line after it, or `None` if line `i` is not one.
    fn error(&mut self, i: usize) -> Option<usize> {
        let t = self.text(i);
        if t.is_empty() || is_quoted(self.lines[i]) || t.starts_with("---") {
            return None;
        }
        let next_location = self
            .text(i + 1)
            .strip_prefix("---")
            .and_then(split_location);
        let (kind, message, location, mut end) = if open_failure(t) {
            // The location is in the `.aux` file that names the file, which
            // is not where to fix anything.
            let end = if next_location.is_some() {
                i + 2
            } else {
                i + 1
            };
            (DiagnosticKind::MissingFile, t, None, end)
        } else if let Some((message, location)) = split_location_suffix(t) {
            (DiagnosticKind::BibtexError, message, Some(location), i + 1)
        } else if let Some(location) = next_location {
            (DiagnosticKind::BibtexError, t, Some(location), i + 2)
        } else if t.starts_with("Sorry---") {
            (DiagnosticKind::BibtexError, t, None, i + 1)
        } else {
            return None;
        };

        let mut previous_line = false;
        let mut quoted = Vec::new();
        let limit = end.saturating_add(MAX_TRAILING_LINES).min(self.lines.len());
        while end < limit {
            let next = self.text(end);
            if is_quoted(self.lines[end]) {
                quoted.push(next.get(3..).unwrap_or(""));
                end += 1;
            } else if next == "(Error may have been on previous line)" {
                previous_line = true;
                end += 1;
            } else if next.starts_with("I'm skipping whatever remains of this ") {
                end += 1;
                break;
            } else {
                break;
            }
        }

        let message = message.trim();
        let read_on = if kind == DiagnosticKind::BibtexError {
            read_on_note(message, location, quoted.get(1).copied())
        } else {
            None
        };
        let (file, line) = match location {
            Some(location) => self.resolve(location, read_on.is_none()),
            None => (None, None),
        };
        let mut text = format!("BibTeX: {message}");
        match read_on {
            Some(note) => text.push_str(&note),
            None if previous_line => text.push_str(" (the error may be on the previous line)"),
            None => {}
        }
        if kind == DiagnosticKind::MissingFile && unsafe_name(message) {
            text.push_str(
                " (TeX Live's safe settings do not let BibTeX open files by absolute path, in \
                 parent directories or in directories starting with `.`)",
            );
        }
        let weak = weak_error(t);
        let severity = if weak == Some(Weak::NoCitations) || weak.is_some() && self.weak_only {
            Severity::Warning
        } else {
            Severity::Error
        };
        self.push(severity, kind, &text, file, line, i, end);
        Some(end)
    }

    /// Handles `Warning--<body>` at line `i`; returns the index of the first
    /// line after it.
    fn warning(&mut self, i: usize, body: &str) -> usize {
        let mut end = i + 1;
        let location = self
            .text(end)
            .strip_prefix("--")
            .filter(|rest| !rest.starts_with('-'))
            .and_then(split_location);
        if location.is_some() {
            end += 1;
        }
        let kind = if body.starts_with("I didn't find a database entry for ") {
            DiagnosticKind::UndefinedCitation
        } else {
            DiagnosticKind::Other
        };
        // Only this warning is printed before BibTeX looks ahead.
        let line_certain = body.starts_with("string name ") && body.ends_with(" is undefined");
        let (file, line) = match location {
            Some(location) => self.resolve(location, line_certain),
            None => (None, None),
        };
        let message = format!("BibTeX warning: {}", body.trim());
        self.push(Severity::Warning, kind, &message, file, line, i, end);
        end
    }

    /// The workspace file (and line, if `keep_line`) of a location.
    fn resolve(
        &mut self,
        location: Location<'a>,
        keep_line: bool,
    ) -> (Option<WorkspacePath>, Option<u32>) {
        let name = location.file.trim();
        let is_input = has_extension(name, "bib") || has_extension(name, "bst");
        if !is_input || !self.known.contains(name) {
            return (None, None);
        }
        let files = self.files;
        let file = self
            .located
            .entry(name)
            .or_insert_with(|| WorkspacePath::new(name).ok().and_then(|p| files.locate(&p)))
            .clone();
        let line = file.as_ref().and(location.line).filter(|_| keep_line);
        (file, line)
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        severity: Severity,
        kind: DiagnosticKind,
        message: &str,
        file: Option<WorkspacePath>,
        line: Option<u32>,
        start: usize,
        end: usize,
    ) {
        if severity == Severity::Error {
            self.errors_reported += 1;
        }
        if !self.out.accepts(severity) {
            return;
        }
        let mut d = Diagnostic::new(severity, kind, sanitize(message, MAX_MESSAGE_BYTES));
        if let Some(file) = file {
            d = d.with_file(file);
        }
        if let Some(line) = line {
            d = d.with_line(line);
        }
        d = d.with_raw_excerpt(self.excerpt(start, end));
        self.out.push(d);
    }

    fn excerpt(&self, start: usize, end: usize) -> String {
        let end = end.min(self.lines.len());
        let mut out = String::new();
        for (n, i) in (start.min(end)..end).take(MAX_EXCERPT_LINES).enumerate() {
            if n > 0 {
                out.push('\n');
            }
            let budget = MAX_EXCERPT_BYTES.saturating_sub(out.len());
            out.push_str(prefix(self.lines[i], budget));
            if out.len() >= MAX_EXCERPT_BYTES {
                break;
            }
        }
        let keep = prefix(&out, MAX_EXCERPT_BYTES).len();
        out.truncate(keep);
        out
    }
}

/// A line quoting input (` : ...`).
fn is_quoted(line: &str) -> bool {
    line.starts_with(" : ") || line.trim_end() == " :"
}

/// When BibTeX read on past the actual mistake before it noticed, a note
/// saying where it noticed (its line is then not reported, see the
/// Locations section of [`BlgParser`]); `None` when its line is where the
/// unexpected token is. `after` is the second quoted line (the text after
/// the position BibTeX reached).
fn read_on_note(
    message: &str,
    location: Option<Location<'_>>,
    after: Option<&str>,
) -> Option<String> {
    let location = location?;
    let at = |what: &str| {
        location
            .line
            .map_or(String::new(), |n| format!("{what} at line {n}; "))
    };
    if has_extension(location.file, "bst") {
        Some(format!(
            " ({}the mistake may be earlier in the style)",
            at("detected")
        ))
    } else if message.starts_with("Unbalanced braces") {
        Some(format!(
            " ({}an unbalanced `{{` or `\"` earlier in this entry is likely)",
            at("detected")
        ))
    } else if message.starts_with("Illegal end of database file") {
        Some(format!(
            " ({}an entry is probably not closed)",
            at("reached the end of the file")
        ))
    } else if after.is_some_and(|a| a.trim_start().starts_with('@')) {
        Some(format!(
            " ({}the entry before it is probably not closed: a missing `}}` or an unbalanced \
             `{{` / `\"` in one of its fields)",
            at("found the next entry")
        ))
    } else {
        None
    }
}

/// An error latexmk does not count as a failure when it is the only kind
/// of error in the log (the "weak errors" of latexmk 4.86's
/// `check_bibtex_log`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Weak {
    /// `I found no \citation commands` (a document without `\cite` yet):
    /// always only a warning.
    NoCitations,
    /// A missing `.aux` or database file.
    Missing,
}

fn weak_error(t: &str) -> Option<Weak> {
    if t.starts_with("I found no \\citation commands---while reading file") {
        Some(Weak::NoCitations)
    } else if t.starts_with("I couldn't open auxiliary file ")
        || t.starts_with("I couldn't open database file ")
    {
        Some(Weak::Missing)
    } else {
        None
    }
}

/// Whether all errors BibTeX counted in its summary are weak, as latexmk
/// decides (it then treats the run as a success). A linear pre-scan.
fn weak_only(lines: &[&str]) -> bool {
    let mut weak = 0u64;
    let mut errors = None;
    for line in lines {
        if is_quoted(line) {
            continue;
        }
        let t = line.trim_end();
        if weak_error(t).is_some() {
            weak += 1;
        } else if let Some(n) = error_count(t) {
            errors = Some(u64::from(n));
        }
    }
    weak > 0 && errors.is_some_and(|n| n <= weak)
}

/// Whether the file of `I couldn't open ... file <name>` is one kpathsea's
/// paranoid mode refuses (absolute, or with a `..` or dot component).
fn unsafe_name(message: &str) -> bool {
    let name = message.rsplit_once(" file ").map_or("", |(_, n)| n);
    let name = name.trim_matches(['`', '\'']);
    name.starts_with('/') || name.split('/').any(|c| c.starts_with('.'))
}

fn has_extension(name: &str, ext: &str) -> bool {
    name.trim()
        .rsplit_once('.')
        .is_some_and(|(_, e)| e.eq_ignore_ascii_case(ext))
}

/// `I couldn't open ...`: a file BibTeX could not open.
fn open_failure(t: &str) -> bool {
    [
        "I couldn't open database file ",
        "I couldn't open style file ",
        "I couldn't open auxiliary file ",
        "I couldn't open file name ",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

/// `line N of file X` or `while reading file X`.
fn split_location(s: &str) -> Option<Location<'_>> {
    if let Some(file) = s.strip_prefix("while reading file ") {
        return (!file.is_empty()).then_some(Location { line: None, file });
    }
    let (n, file) = s.strip_prefix("line ")?.split_once(" of file ")?;
    let line = n.parse().ok().filter(|&n| n > 0)?;
    (!file.is_empty()).then_some(Location {
        line: Some(line),
        file,
    })
}

/// `<message>---line N of file X` / `<message>---while reading file X`.
fn split_location_suffix(t: &str) -> Option<(&str, Location<'_>)> {
    let at = t
        .rfind("---line ")
        .or_else(|| t.rfind("---while reading file "))?;
    let message = &t[..at];
    if message.trim().is_empty() {
        return None;
    }
    Some((message, split_location(&t[at + 3..])?))
}

/// `N` of `(There was 1 error message)` / `(There were N error messages)`.
fn error_count(t: &str) -> Option<u32> {
    if t == "(There was 1 error message)" {
        return Some(1);
    }
    t.strip_prefix("(There were ")?
        .strip_suffix(" error messages)")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn locate_all(name: &WorkspacePath) -> Option<WorkspacePath> {
        WorkspacePath::new(&format!("paper/{name}")).ok()
    }

    fn parse(blg: &str) -> ParsedBlg {
        BlgParser::new().parse_with_files(blg.as_bytes(), &locate_all)
    }

    #[test]
    fn locations() {
        assert_eq!(
            split_location("line 4 of file refs.bib").map(|l| (l.line, l.file)),
            Some((Some(4), "refs.bib"))
        );
        assert_eq!(
            split_location("while reading file main.aux").map(|l| (l.line, l.file)),
            Some((None, "main.aux"))
        );
        assert!(split_location("line 0 of file refs.bib").is_none());
        assert!(split_location("line x of file refs.bib").is_none());
        assert!(split_location("line 3 of file ").is_none());
        assert!(split_location_suffix("---line 3 of file a.bib").is_none());
        assert_eq!(error_count("(There were 12 error messages)"), Some(12));
        assert_eq!(error_count("(There was 1 error message)"), Some(1));
        assert_eq!(error_count("(There were 2 warnings)"), None);
    }

    #[test]
    fn files_must_be_announced_databases_or_styles() {
        let blg = "Database file #1: refs.bib\n\
                   Repeated entry---line 7 of file refs.bib\n\
                   Repeated entry---line 9 of file other.bib\n\
                   Repeated entry---line 9 of file main.aux\n\
                   (There were 3 error messages)\n";
        let parsed = parse(blg);
        let located: Vec<_> = parsed
            .diagnostics
            .iter()
            .map(|d| (d.file.as_ref().map(WorkspacePath::as_str), d.line))
            .collect();
        assert_eq!(
            located,
            [
                (Some("paper/refs.bib"), Some(7)),
                (None, None),
                (None, None),
                (None, None)
            ]
        );
        assert!(parsed.failed);
        assert_eq!(parsed.error_messages, Some(3));
        let summary = parsed.diagnostics.last().unwrap();
        assert_eq!(summary.kind, DiagnosticKind::BibtexFailed);
        assert_eq!(summary.severity, Severity::Info);
        assert_eq!(
            summary.message,
            "BibTeX failed with 3 error messages; the bibliography is incomplete"
        );
    }

    #[test]
    fn without_files_nothing_is_located() {
        let blg = "Database file #1: refs.bib\nRepeated entry---line 7 of file refs.bib\n";
        let parsed = BlgParser::new().parse(blg.as_bytes());
        assert_eq!(parsed.diagnostics[0].file, None);
        assert_eq!(parsed.diagnostics[0].line, None);
        // No summary line, but an error: failed.
        assert!(parsed.failed);
        assert_eq!(parsed.diagnostics[1].kind, DiagnosticKind::BibtexFailed);
    }

    #[test]
    fn locate_is_called_once_per_name() {
        let calls = std::cell::Cell::new(0);
        let files = |name: &WorkspacePath| {
            calls.set(calls.get() + 1);
            Some(name.clone())
        };
        let mut blg = String::from("Database file #1: refs.bib\n");
        for n in 1..=50 {
            let _ = writeln!(blg, "Repeated entry---line {n} of file refs.bib");
        }
        let parsed = BlgParser::new().parse_with_files(blg.as_bytes(), &files);
        assert_eq!(parsed.diagnostics.len(), 51);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn unrecognized_summary_without_errors_is_an_error() {
        let parsed = parse("(There were 2 error messages)\n");
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].severity, Severity::Error);
        assert_eq!(parsed.diagnostics[0].kind, DiagnosticKind::BibtexFailed);
        assert_eq!(
            parsed.diagnostics[0].raw_excerpt.as_deref(),
            Some("(There were 2 error messages)")
        );
        let parsed =
            parse("Sorry---you've exceeded BibTeX's hash size 100\n(That was a fatal error)\n");
        assert_eq!(parsed.diagnostics[0].kind, DiagnosticKind::BibtexError);
        assert!(parsed.diagnostics[1].message.contains("fatal error"));
        assert!(parsed.failed);
    }

    #[test]
    fn styles_and_unclosed_entries_keep_only_the_file() {
        let blg = "The style file: my.bst\n\
                   read is an unknown function---line 7 of file my.bst\n\
                   Database file #1: refs.bib\n\
                   Illegal end of database file---line 9 of file refs.bib\n \
                   : @string{x = \"y\"\n \
                   :                  \n\
                   I was expecting a `,' or a `}'---line 3 of file refs.bib\n \
                   : \n \
                   : @string{b = \"c\"}\n\
                   (Error may have been on previous line)\n";
        let parsed = parse(blg);
        let d = &parsed.diagnostics;
        let located: Vec<_> = d
            .iter()
            .map(|d| (d.file.as_ref().map(WorkspacePath::as_str), d.line))
            .collect();
        assert_eq!(
            located[..3],
            [
                (Some("paper/my.bst"), None),
                (Some("paper/refs.bib"), None),
                (Some("paper/refs.bib"), None),
            ]
        );
        assert_eq!(
            d[0].message,
            "BibTeX: read is an unknown function (detected at line 7; the mistake may be \
             earlier in the style)"
        );
        assert!(
            d[1].message
                .contains("reached the end of the file at line 9")
        );
        assert!(d[2].message.contains("found the next entry at line 3"));
        assert!(!d[2].message.contains("previous line"));
    }

    #[test]
    fn weak_errors_follow_latexmk() {
        // Only a missing database: weak, as for latexmk.
        let blg = "I couldn't open database file x.bib\n---line 3 of file main.aux\n\
                   (There was 1 error message)\n";
        let parsed = parse(blg);
        assert!(!parsed.failed);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].severity, Severity::Warning);
        // With another error, it counts.
        let blg = "I couldn't open database file x.bib\n---line 3 of file main.aux\n\
                   I found no database files---while reading file main.aux\n\
                   (There were 2 error messages)\n";
        let parsed = parse(blg);
        assert!(parsed.failed);
        assert_eq!(parsed.diagnostics[0].severity, Severity::Error);
        // No citations stays a warning next to a real error.
        let blg = "I found no \\citation commands---while reading file main.aux\n\
                   Database file #1: refs.bib\nRepeated entry---line 2 of file refs.bib\n\
                   (There were 2 error messages)\n";
        let parsed = parse(blg);
        assert!(parsed.failed);
        let severities: Vec<_> = parsed.diagnostics.iter().map(|d| d.severity).collect();
        assert_eq!(
            severities,
            [Severity::Warning, Severity::Error, Severity::Info]
        );
    }

    #[test]
    fn refused_names_are_explained() {
        for name in [
            ".hidden/refs.bib",
            "../refs.bib",
            "/abs/refs.bib",
            "a/.b/c.bst",
        ] {
            let blg = format!("I couldn't open database file {name}\n");
            let d = &parse(&blg).diagnostics[0];
            assert!(d.message.contains("safe settings"), "{name}: {d:#?}");
        }
        let d = &parse("I couldn't open database file refs.bib\n").diagnostics[0];
        assert!(!d.message.contains("safe settings"));
    }

    #[test]
    fn warnings_only_do_not_fail() {
        let parsed = parse("Warning--empty author in x\n(There was 1 warning)\n");
        assert!(!parsed.failed);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(
            parsed.diagnostics[0].message,
            "BibTeX warning: empty author in x"
        );
    }

    #[test]
    fn quoted_input_is_never_a_message() {
        let blg = "Database file #1: refs.bib\n\
                   I was expecting a `,' or a `}'---line 4 of file refs.bib\n \
                   : Repeated entry---line 1 of file refs.bib\n \
                   : (There were 9 error messages)\n\
                   I'm skipping whatever remains of this entry\n \
                   : Warning--x\n";
        let parsed = parse(blg);
        let kinds: Vec<_> = parsed.diagnostics.iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            [DiagnosticKind::BibtexError, DiagnosticKind::BibtexFailed]
        );
        assert_eq!(parsed.error_messages, None);
        assert_eq!(parsed.diagnostics[0].line, Some(4));
    }

    #[test]
    fn limit_keeps_errors_and_the_summary() {
        let mut blg = String::from("Database file #1: refs.bib\n");
        for _ in 0..10 {
            blg.push_str("Warning--empty author in x\n");
        }
        blg.push_str("Repeated entry---line 3 of file refs.bib\n(There was 1 error message)\n");
        let parsed = BlgParser::new()
            .with_max_diagnostics(2)
            .parse_with_files(blg.as_bytes(), &locate_all);
        let kinds: Vec<_> = parsed.diagnostics.iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            [
                DiagnosticKind::Other,
                DiagnosticKind::BibtexError,
                DiagnosticKind::BibtexFailed,
                DiagnosticKind::Other
            ]
        );
        assert_eq!(parsed.omitted, 9);
        assert!(
            parsed.diagnostics[3]
                .message
                .starts_with("9 more diagnostics omitted")
        );
    }

    /// Linear in the input: long lines, many messages and many names.
    #[test]
    fn large_inputs_are_cheap() {
        let mut blg = String::new();
        for n in 0..100_000 {
            let _ = writeln!(blg, "Database file #{n}: f{n}.bib");
            let _ = writeln!(blg, "Repeated entry---line 1 of file f{n}.bib");
        }
        blg.push_str(&"-".repeat(4_000_000));
        blg.push('\n');
        blg.push_str(&"Warning--".repeat(400_000));
        let calls = std::cell::Cell::new(0);
        let files = |name: &WorkspacePath| {
            calls.set(calls.get() + 1);
            Some(name.clone())
        };
        let start = std::time::Instant::now();
        let parsed = BlgParser::new().parse_with_files(blg.as_bytes(), &files);
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() < 3000, "{elapsed:?}");
        assert_eq!(calls.get(), MAX_KNOWN_FILES);
        assert!(parsed.omitted > 0);
        assert!(
            parsed
                .diagnostics
                .iter()
                .all(|d| d.message.len() <= MAX_MESSAGE_BYTES)
        );
    }
}
