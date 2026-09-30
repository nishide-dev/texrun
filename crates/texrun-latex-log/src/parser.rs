//! The line-oriented parse loop.

use std::path::Path;

use texrun_core::{Diagnostic, DiagnosticKind, Severity, WorkspacePath, WorkspaceRoot};

use crate::lines::Lines;
use crate::patterns::{self, ErrorHeader};
use crate::stack::FileStack;

/// Maximum number of continuation lines appended to a message.
const MAX_CONTINUATION_LINES: usize = 8;
/// How far after an error header to look for its `l.<n>` context line.
const MAX_CONTEXT_SCAN: usize = 40;
/// Maximum number of help / box detail lines consumed after a diagnostic.
const MAX_TRAILING_LINES: usize = 24;
/// Upper bound of [`Diagnostic::message`], in bytes.
const MAX_MESSAGE_BYTES: usize = 2048;
/// Upper bound of [`Diagnostic::raw_excerpt`], in bytes.
const MAX_EXCERPT_BYTES: usize = 4096;
/// Upper bound of the number of lines in [`Diagnostic::raw_excerpt`].
const MAX_EXCERPT_LINES: usize = 32;

pub(crate) struct Config<'a> {
    pub workspace_root: Option<&'a WorkspaceRoot>,
    /// Lines of exactly this many bytes may have been wrapped by TeX (only
    /// set when wrapped lines were not already joined).
    pub suspect_wrap_width: Option<usize>,
    /// See [`LogParser::with_max_diagnostics`](crate::LogParser::with_max_diagnostics).
    pub max_diagnostics: usize,
}

/// The result of parsing a log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ParsedLog {
    /// The diagnostics in log order. When some were omitted because of
    /// [`LogParser::with_max_diagnostics`](crate::LogParser::with_max_diagnostics),
    /// the last entry is a [`Severity::Info`] / [`DiagnosticKind::Other`]
    /// notice saying how many (it does not count towards the limit).
    pub diagnostics: Vec<Diagnostic>,
    /// Number of recognized diagnostics that were dropped because of the
    /// limit.
    pub omitted: usize,
}

pub(crate) fn parse(lines: &Lines, config: &Config<'_>) -> ParsedLog {
    let mut parser = Parser {
        lines,
        config,
        stack: FileStack::default(),
        out: Collector::new(config.max_diagnostics),
        stopped: false,
        undefined_reported: false,
        untrusted_stop_line: false,
    };
    let mut i = 0;
    while i < lines.len() {
        let consumed = parser
            .runaway(i)
            .or_else(|| parser.error(i, i))
            .or_else(|| parser.overfull_or_underfull(i))
            .or_else(|| parser.warning(i));
        if let Some(end) = consumed {
            i = end.max(i + 1);
        } else {
            // Only lines that are not part of a recognized message reach the
            // file stack: messages may contain unbalanced parentheses.
            parser.stack.feed(lines.text(i), parser.may_continue(i));
            i += 1;
        }
    }
    parser.out.finish()
}

struct Parser<'a> {
    lines: &'a Lines,
    config: &'a Config<'a>,
    stack: FileStack,
    out: Collector,
    /// An emergency stop has been reported.
    stopped: bool,
    /// An undefined reference or citation has been reported.
    undefined_reported: bool,
    /// The next `Emergency stop.` repeats a line that was found to be wrong.
    untrusted_stop_line: bool,
}

impl<'a> Parser<'a> {
    fn text(&self, i: usize) -> &'a str {
        let lines: &'a Lines = self.lines;
        lines.text(i)
    }

    /// Whether line `i` might continue on the next line because of wrapping.
    fn may_continue(&self, i: usize) -> bool {
        i < self.lines.len() && self.config.suspect_wrap_width == Some(self.lines.raw_len(i))
    }

    fn is_block_start(&self, i: usize) -> bool {
        let t = self.text(i);
        patterns::error_header(t).is_some()
            || patterns::box_header(t).is_some()
            || patterns::warning_header(t).is_some()
    }

    /// The file TeX is reading, as a workspace path.
    fn current_file(&self) -> Option<WorkspacePath> {
        self.stack.current().and_then(|f| self.normalize(f))
    }

    fn normalize(&self, printed: &str) -> Option<WorkspacePath> {
        normalize_path(printed, self.config.workspace_root)
    }

    /// `Runaway argument?` and the argument text precede the error they
    /// explain (e.g. `Paragraph ended before \textbf was complete.`); they
    /// become part of that error's excerpt instead of being fed to the file
    /// stack.
    fn runaway(&mut self, i: usize) -> Option<usize> {
        let t = self.text(i);
        if !(t.starts_with("Runaway ") && t.trim_end().ends_with('?')) {
            return None;
        }
        let header = (i + 1..=i + 2).find(|&h| patterns::error_header(self.text(h)).is_some())?;
        self.error(header, i)
    }

    /// Handles a TeX error whose header is line `i` and whose excerpt starts
    /// at `excerpt_start`; returns the index of the first line after it.
    fn error(&mut self, i: usize, excerpt_start: usize) -> Option<usize> {
        let header = patterns::error_header(self.text(i))?;
        let class = patterns::classify_error(header.text);

        let mut message = header.text.to_owned();
        let mut end = i + 1;
        if let Some(name) = class.continuation {
            end = self.append_continuation(&mut message, end, name);
        }

        // A file-line-error header names the file; a wrapped header (the
        // previous line may continue into it) only has a partial name.
        let file = match header {
            ErrorHeader { file: Some(_), .. } if i > 0 && self.may_continue(i - 1) => None,
            ErrorHeader {
                file: Some(printed),
                ..
            } => self.normalize(printed),
            ErrorHeader { file: None, .. } => self.current_file(),
        };

        // Set by a missing package / class whose line was dropped: this is
        // the `Emergency stop.` right after it, reporting the same position.
        let drop_line = std::mem::take(&mut self.untrusted_stop_line)
            && class.kind == DiagnosticKind::EmergencyStop
            && !class.fatal_summary;
        let mut untrusted_stop_line = false;

        let message_end = end;
        let context = self.find_context(end);
        let mut line = header.line;
        let mut excerpt_end = end;
        if let Some(ctx) = context {
            if !ctx.crossed_stop {
                line = line.or(Some(ctx.line));
                // `l.<n> <before>` is followed by the rest of the line.
                excerpt_end = (ctx.index + 2).min(self.lines.len());
                end = self.skip_trailing(excerpt_end);
            } else if class.kind != DiagnosticKind::MissingFile
                || self.stop_context_is_request(header.text, ctx.index)
            {
                line = line.or(Some(ctx.line));
            } else {
                // The following `Emergency stop.` has the same (wrong) line.
                untrusted_stop_line = true;
            }
        }
        if drop_line {
            line = None;
        }

        if class.kind == DiagnosticKind::UndefinedControlSequence
            && let Some(cs) = patterns::trailing_control_sequence(self.text(message_end))
        {
            message = format!("Undefined control sequence {cs}");
        }
        if class.fatal_summary {
            if self.stopped {
                return Some(end);
            }
            message = message.trim_start_matches("==>").trim_start().to_owned();
        }
        if class.kind == DiagnosticKind::EmergencyStop {
            self.stopped = true;
        }
        self.untrusted_stop_line = untrusted_stop_line;

        self.push(
            Severity::Error,
            class.kind,
            &message,
            file,
            line,
            excerpt_start..excerpt_end,
        );
        Some(end)
    }

    /// For a missing file reported through the following `Emergency stop.`:
    /// whether the stop's context line (at `index`) is where the file was
    /// requested.
    ///
    /// For `\input` it is. For packages and classes LaTeX has already looked
    /// ahead for an optional `[date]` argument, so TeX usually reports the
    /// next line; the line is only trusted when it shows the loading command.
    fn stop_context_is_request(&self, error_text: &str, index: usize) -> bool {
        let package = patterns::missing_file_name(error_text).is_some_and(|f| {
            Path::new(f).extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("sty") || ext.eq_ignore_ascii_case("cls")
            })
        });
        !package || patterns::loads_package_or_class(self.text(index))
    }

    /// Handles `Overfull \hbox ...` / `Underfull \vbox ...`.
    fn overfull_or_underfull(&mut self, i: usize) -> Option<usize> {
        let header = self.text(i);
        let kind = patterns::box_header(header)?;
        let line = patterns::box_line(header);
        let message = header.trim().to_owned();
        // The box contents follow until a blank line; they may contain
        // unbalanced parentheses, so they are never fed to the file stack.
        let end = self.skip_trailing(i + 1);
        let file = self.current_file();
        self.push(Severity::Warning, kind, &message, file, line, i..end);
        Some(end)
    }

    /// Handles `LaTeX Warning: ...`, `Package x Warning: ...` etc.
    fn warning(&mut self, i: usize) -> Option<usize> {
        let header = patterns::warning_header(self.text(i))?;
        let mut body = header.body.trim().to_owned();
        let mut message = self.text(i).trim().to_owned();
        let mut end = i + 1;
        if let Some(name) = header.continuation {
            let before = message.len();
            end = self.append_continuation(&mut message, end, name);
            body.push_str(&message[before..]);
        }

        if let Some(kind) = patterns::undefined_summary(&body) {
            if !self.undefined_reported {
                let file = self.current_file();
                self.push(Severity::Warning, kind, &message, file, None, i..end);
            }
            return Some(end);
        }

        let (severity, kind) = patterns::classify_warning(&body);
        let line = patterns::input_line(&body);
        let file = self.current_file();
        self.push(severity, kind, &message, file, line, i..end);
        Some(end)
    }

    /// Appends continuation lines starting at `start` to `message` and
    /// returns the index of the first line that is not one.
    fn append_continuation(&self, message: &mut String, start: usize, name: &str) -> usize {
        let mut end = start;
        while end < self.lines.len()
            && end - start < MAX_CONTINUATION_LINES
            && !self.is_block_start(end)
            && patterns::context_line(self.text(end)).is_none()
        {
            let Some(text) = patterns::continuation(self.text(end), name) else {
                break;
            };
            message.push(' ');
            message.push_str(text);
            end += 1;
        }
        end
    }

    /// Finds the `l.<n>` line of an error whose message ends before `start`.
    ///
    /// In nonstop mode a fatal error (e.g. a missing file) is immediately
    /// followed by `Emergency stop.`, whose context is where TeX was reading;
    /// the scan continues past one such line and reports that it did.
    fn find_context(&self, start: usize) -> Option<Context> {
        let mut crossed_stop = false;
        let limit = start.saturating_add(MAX_CONTEXT_SCAN).min(self.lines.len());
        for index in start..limit {
            let t = self.text(index);
            if let Some(line) = patterns::context_line(t) {
                return Some(Context {
                    index,
                    line,
                    crossed_stop,
                });
            }
            if let Some(header) = patterns::error_header(t) {
                let class = patterns::classify_error(header.text);
                if class.kind == DiagnosticKind::EmergencyStop
                    && !class.fatal_summary
                    && !crossed_stop
                {
                    crossed_stop = true;
                    continue;
                }
                return None;
            }
            if patterns::box_header(t).is_some() || patterns::warning_header(t).is_some() {
                return None;
            }
        }
        None
    }

    /// Skips help text / box details: non-blank lines up to the next blank
    /// line or diagnostic.
    fn skip_trailing(&self, start: usize) -> usize {
        let mut end = start;
        while end < self.lines.len()
            && end - start < MAX_TRAILING_LINES
            && !self.text(end).trim().is_empty()
            && !self.is_block_start(end)
        {
            end += 1;
        }
        end
    }

    fn push(
        &mut self,
        severity: Severity,
        kind: DiagnosticKind,
        message: &str,
        file: Option<WorkspacePath>,
        line: Option<u32>,
        excerpt: std::ops::Range<usize>,
    ) {
        if matches!(
            kind,
            DiagnosticKind::UndefinedReference | DiagnosticKind::UndefinedCitation
        ) {
            self.undefined_reported = true;
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
        d = d.with_raw_excerpt(self.excerpt(excerpt));
        self.out.push(d);
    }

    fn excerpt(&self, range: std::ops::Range<usize>) -> String {
        let end = range.end.min(self.lines.len());
        let start = range.start.min(end);
        let mut out = String::new();
        for (n, i) in (start..end).take(MAX_EXCERPT_LINES).enumerate() {
            if n > 0 {
                out.push('\n');
            }
            let budget = MAX_EXCERPT_BYTES.saturating_sub(out.len());
            out.push_str(prefix(self.text(i), budget));
            if out.len() >= MAX_EXCERPT_BYTES {
                break;
            }
        }
        let keep = prefix(&out, MAX_EXCERPT_BYTES).len();
        out.truncate(keep);
        out
    }
}

/// Collects diagnostics up to a limit, keeping errors in preference to
/// warnings / info.
///
/// Errors and the rest are stored separately, each up to the limit (so
/// memory stays bounded however long the log is, and warnings never push
/// out a later error); [`Collector::finish`] then lets errors fill the limit
/// first and restores log order.
struct Collector {
    limit: usize,
    seen: usize,
    errors: Vec<(usize, Diagnostic)>,
    others: Vec<(usize, Diagnostic)>,
}

impl Collector {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            seen: 0,
            errors: Vec::new(),
            others: Vec::new(),
        }
    }

    /// Counts one diagnostic of `severity` and returns whether it may be
    /// kept, so that callers skip building dropped ones.
    fn accepts(&mut self, severity: Severity) -> bool {
        self.seen += 1;
        let bucket = if severity == Severity::Error {
            &self.errors
        } else {
            &self.others
        };
        bucket.len() < self.limit
    }

    fn push(&mut self, d: Diagnostic) {
        let seq = self.seen;
        if d.severity == Severity::Error {
            self.errors.push((seq, d));
        } else {
            self.others.push((seq, d));
        }
    }

    fn finish(self) -> ParsedLog {
        let Self {
            limit,
            seen,
            errors,
            mut others,
        } = self;
        others.truncate(limit.saturating_sub(errors.len()));
        let mut kept: Vec<_> = errors.into_iter().chain(others).collect();
        kept.sort_by_key(|(seq, _)| *seq);
        let mut diagnostics: Vec<_> = kept.into_iter().map(|(_, d)| d).collect();
        let omitted = seen - diagnostics.len();
        if omitted > 0 {
            diagnostics.push(Diagnostic::new(
                Severity::Info,
                DiagnosticKind::Other,
                format!(
                    "{omitted} more diagnostics omitted (limit: {limit}, errors are kept \
                     first); see the full log"
                ),
            ));
        }
        ParsedLog {
            diagnostics,
            omitted,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Context {
    index: usize,
    line: u32,
    crossed_stop: bool,
}

/// Converts a file name printed by TeX to a workspace path.
///
/// Relative names are relative to TeX's working directory, which is the
/// workspace root. Absolute names are kept only when they are inside
/// `workspace_root`.
pub(crate) fn normalize_path(
    printed: &str,
    workspace_root: Option<&WorkspaceRoot>,
) -> Option<WorkspacePath> {
    let printed = printed.trim().trim_matches('"');
    if printed.starts_with('/') {
        let rest = Path::new(printed)
            .strip_prefix(workspace_root?.path())
            .ok()?;
        return WorkspacePath::from_path(rest).ok();
    }
    WorkspacePath::new(printed).ok()
}

/// Replaces tabs by spaces and other control characters by U+FFFD (so a
/// message is safe to print on a terminal), and bounds the length. Stops
/// reading at the bound, so a huge line costs no more than `max` bytes.
fn sanitize(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    for c in text.chars() {
        let c = match c {
            '\t' => ' ',
            c if c.is_control() => '\u{FFFD}',
            c => c,
        };
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

/// The longest prefix of `text` of at most `max` bytes.
fn prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_relative_and_workspace_absolute_paths() {
        let root = WorkspaceRoot::new("/work/space").unwrap();
        let n = |p| normalize_path(p, Some(&root)).map(|w| w.as_str().to_owned());
        assert_eq!(n("./main.tex").as_deref(), Some("main.tex"));
        assert_eq!(n("\"./my file.tex\"").as_deref(), Some("my file.tex"));
        assert_eq!(
            n("./chapters//intro.tex").as_deref(),
            Some("chapters/intro.tex")
        );
        assert_eq!(n("/work/space/sub/a.tex").as_deref(), Some("sub/a.tex"));
        assert_eq!(n("/work/spaceship/a.tex"), None);
        assert_eq!(n("/work/space/../etc/passwd"), None);
        assert_eq!(n("/work/space"), None);
        assert_eq!(
            n("/usr/share/texlive/texmf-dist/tex/latex/base/article.cls"),
            None
        );
        assert_eq!(n("../outside.tex"), None);
        assert_eq!(n("C:/x.tex"), None);
        assert_eq!(normalize_path("/work/space/a.tex", None), None);
    }

    #[test]
    fn sanitize_and_prefix() {
        assert_eq!(sanitize("a\u{1b}[31mb", 100), "a\u{FFFD}[31mb");
        assert_eq!(sanitize("ééé", 3), "é");
        assert_eq!(sanitize("a\tb", 3), "a b");
        assert_eq!(prefix("ééé", 5), "éé");
        assert_eq!(prefix("abc", 10), "abc");
    }

    fn diag(severity: Severity, n: u32) -> Diagnostic {
        Diagnostic::new(severity, DiagnosticKind::Other, "m").with_line(n)
    }

    fn collect(limit: usize, items: &[(Severity, u32)]) -> ParsedLog {
        let mut c = Collector::new(limit);
        for &(severity, n) in items {
            if c.accepts(severity) {
                c.push(diag(severity, n));
            }
        }
        c.finish()
    }

    #[test]
    fn collector_keeps_everything_under_the_limit() {
        let log = collect(3, &[(Severity::Warning, 1), (Severity::Error, 2)]);
        assert_eq!(log.omitted, 0);
        assert_eq!(log.diagnostics.len(), 2);
    }

    #[test]
    fn collector_prefers_errors_and_keeps_log_order() {
        use Severity::{Error, Info, Warning};
        let log = collect(
            3,
            &[
                (Warning, 1),
                (Warning, 2),
                (Info, 3),
                (Warning, 4),
                (Error, 5),
                (Error, 6),
            ],
        );
        let lines: Vec<_> = log.diagnostics.iter().map(|d| d.line).collect();
        // Two errors + the first warning, in log order, then the notice.
        assert_eq!(lines, [Some(1), Some(5), Some(6), None]);
        assert_eq!(log.omitted, 3);
        let notice = log.diagnostics.last().unwrap();
        assert_eq!(
            (notice.severity, notice.kind),
            (Info, DiagnosticKind::Other)
        );
        assert!(notice.message.starts_with("3 more diagnostics omitted"));

        // More errors than the limit: the first ones are kept.
        let log = collect(2, &[(Error, 1), (Warning, 2), (Error, 3), (Error, 4)]);
        let lines: Vec<_> = log.diagnostics.iter().map(|d| d.line).collect();
        assert_eq!(lines, [Some(1), Some(3), None]);
        assert_eq!(log.omitted, 2);
    }
}
