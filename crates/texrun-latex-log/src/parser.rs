//! The line-oriented parse loop.

use std::path::Path;

use texrun_core::{Diagnostic, DiagnosticKind, Severity, WorkspacePath, WorkspaceRoot};

use crate::lines::Line;
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
}

pub(crate) fn parse(lines: &[Line], config: &Config<'_>) -> Vec<Diagnostic> {
    let mut parser = Parser {
        lines,
        config,
        stack: FileStack::default(),
        out: Vec::new(),
        stopped: false,
    };
    let mut i = 0;
    while i < lines.len() {
        let consumed = parser
            .error(i)
            .or_else(|| parser.overfull_or_underfull(i))
            .or_else(|| parser.warning(i));
        if let Some(end) = consumed {
            i = end.max(i + 1);
        } else {
            // Only lines that are not part of a recognized message reach the
            // file stack: messages may contain unbalanced parentheses.
            parser.stack.feed(&lines[i].text, parser.may_continue(i));
            i += 1;
        }
    }
    parser.out
}

struct Parser<'a> {
    lines: &'a [Line],
    config: &'a Config<'a>,
    stack: FileStack,
    out: Vec<Diagnostic>,
    /// An emergency stop has been reported.
    stopped: bool,
}

impl<'a> Parser<'a> {
    fn text(&self, i: usize) -> &'a str {
        let lines: &'a [Line] = self.lines;
        lines.get(i).map_or("", |l| l.text.as_str())
    }

    /// Whether line `i` might continue on the next line because of wrapping.
    fn may_continue(&self, i: usize) -> bool {
        self.lines
            .get(i)
            .is_some_and(|l| self.config.suspect_wrap_width == Some(l.raw_len))
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

    /// Handles a TeX error starting at line `i`; returns the index of the
    /// first line after it.
    fn error(&mut self, i: usize) -> Option<usize> {
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

        let message_end = end;
        let context = self.find_context(end);
        let mut line = header.line;
        let mut excerpt_end = end;
        if let Some(ctx) = context {
            line = line.or(Some(ctx.line));
            if !ctx.crossed_stop {
                // `l.<n> <before>` is followed by the rest of the line.
                excerpt_end = (ctx.index + 2).min(self.lines.len());
                end = self.skip_trailing(excerpt_end);
            }
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

        self.push(
            Severity::Error,
            class.kind,
            &message,
            file,
            line,
            i..excerpt_end,
        );
        Some(end)
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
            let reported = self.out.iter().any(|d| {
                matches!(
                    d.kind,
                    DiagnosticKind::UndefinedReference | DiagnosticKind::UndefinedCitation
                )
            });
            if !reported {
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
    /// followed by `Emergency stop.`, whose context is where the file was
    /// requested; the scan continues past one such line and reports it.
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
        let lines = &self.lines[start..end];
        let mut out = String::new();
        for (n, line) in lines.iter().take(MAX_EXCERPT_LINES).enumerate() {
            if n > 0 {
                out.push('\n');
            }
            out.push_str(&line.text);
            if out.len() >= MAX_EXCERPT_BYTES {
                break;
            }
        }
        truncate(&mut out, MAX_EXCERPT_BYTES);
        out
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
/// message is safe to print on a terminal), and bounds the length.
fn sanitize(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .map(|c| match c {
            '\t' => ' ',
            c if c.is_control() => '\u{FFFD}',
            c => c,
        })
        .collect();
    truncate(&mut out, max);
    out
}

fn truncate(text: &mut String, max: usize) {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
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
    fn sanitize_and_truncate() {
        assert_eq!(sanitize("a\u{1b}[31mb", 100), "a\u{FFFD}[31mb");
        assert_eq!(sanitize("ééé", 3), "é");
    }
}
