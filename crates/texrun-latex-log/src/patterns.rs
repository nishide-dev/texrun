//! Recognizers for single log lines. All functions are total: they return
//! `None` for anything they do not recognize.

use texrun_core::{DiagnosticKind, Severity};

/// The first line of a TeX error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ErrorHeader<'a> {
    /// The file of a `-file-line-error` style header (`./main.tex:5: ...`),
    /// as printed by TeX. `None` for the classic `! ...` form.
    pub file: Option<&'a str>,
    /// The line of a `-file-line-error` style header.
    pub line: Option<u32>,
    /// The error text without the `! ` / `file:line: ` prefix.
    pub text: &'a str,
}

/// Recognizes `! <text>`, `<file>:<line>: <text>` and pdfTeX's own
/// `!pdfTeX error: <text>` (see [`pdftex_fail`]).
pub(crate) fn error_header(line: &str) -> Option<ErrorHeader<'_>> {
    if let Some(text) = pdftex_fail(line) {
        return Some(ErrorHeader {
            file: None,
            line: None,
            text,
        });
    }
    if let Some(rest) = line.strip_prefix('!') {
        let text = rest.trim();
        return (rest.starts_with(' ') && !text.is_empty()).then_some(ErrorHeader {
            file: None,
            line: None,
            text,
        });
    }
    file_line_header(line)
}

/// A fatal error pdfTeX reports itself rather than through TeX's error
/// routine (`pdftex_fail`), e.g. a font or an image it cannot load:
///
/// ```text
/// !pdfTeX error: pdflatex (file ecrm1000): Font ecrm1000 at 600 not found
///  ==> Fatal error occurred, no output PDF file produced!
/// ```
///
/// There is no space after `!`, no `file:line:` prefix even with
/// `-file-line-error` and no `l.<n>` context: the job ends right after it.
/// Returns the text after `!` (`pdfTeX error: ...`). `pdfTeX error (ext4):
/// ...` and the like are ordinary TeX errors (`! pdfTeX error (...)`).
pub(crate) fn pdftex_fail(line: &str) -> Option<&str> {
    let text = line.strip_prefix('!')?;
    let body = text.strip_prefix("pdfTeX error:")?;
    (!body.trim().is_empty()).then(|| text.trim_end())
}

/// The ` ==> Fatal error occurred, ...` line that follows a [`pdftex_fail`]
/// error. Unlike the summary of a TeX error, it has no `!` / `file:line:`
/// prefix.
pub(crate) fn pdftex_fatal_summary(line: &str) -> Option<&str> {
    let text = line.strip_prefix(' ')?;
    text.starts_with("==> Fatal error occurred")
        .then(|| text.trim_start_matches("==>").trim())
}

fn file_line_header(line: &str) -> Option<ErrorHeader<'_>> {
    // Only the first `:<digits>:` is considered; file names containing such a
    // sequence are not supported.
    let (idx, _) = line.match_indices(':').find(|&(idx, _)| {
        let after = &line[idx + 1..];
        let digits = leading_digits(after);
        digits > 0 && after[digits..].starts_with(':')
    })?;
    let path = &line[..idx];
    let after = &line[idx + 1..];
    let digits = leading_digits(after);
    let rest = &after[digits + 1..];
    if !(rest.is_empty() || rest.starts_with(' ')) || !looks_like_header_path(path) {
        return None;
    }
    let text = rest.trim();
    (!text.is_empty()).then_some(ErrorHeader {
        file: Some(path),
        line: parse_line_number(&after[..digits]),
        text,
    })
}

/// TeX prints errors at the start of a line, so the path may contain
/// parentheses (`./a(1).tex:3:`) but cannot start with a file-stack `(`.
fn looks_like_header_path(path: &str) -> bool {
    !path.starts_with(char::is_whitespace)
        && !path.starts_with('(')
        && (path.contains('/') || has_extension(path))
}

/// Whether the text following `(` in the log looks like a file name.
pub(crate) fn looks_like_file(token: &str) -> bool {
    token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('/')
        || has_extension(token)
}

/// `name.ext` with a short alphanumeric extension.
fn has_extension(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=16).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// How an error was classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ErrorClass<'a> {
    pub kind: DiagnosticKind,
    /// The ` ==> Fatal error occurred` line printed when TeX gives up. It
    /// only restates an earlier error, unless no stop was reported before.
    pub fatal_summary: bool,
    /// Prefix of message continuation lines (e.g. `pkg` for `(pkg)`), for
    /// LaTeX / package / class errors.
    pub continuation: Option<&'a str>,
}

pub(crate) fn classify_error(text: &str) -> ErrorClass<'_> {
    let mut class = ErrorClass {
        kind: DiagnosticKind::Other,
        fatal_summary: false,
        continuation: None,
    };
    if text.starts_with("==> Fatal error occurred") {
        class.kind = DiagnosticKind::EmergencyStop;
        class.fatal_summary = true;
    } else if text.starts_with("Emergency stop") {
        class.kind = DiagnosticKind::EmergencyStop;
    } else if text.starts_with("Undefined control sequence") {
        class.kind = DiagnosticKind::UndefinedControlSequence;
    } else {
        let source = source_prefix(text, "Error");
        if is_missing_file(text) {
            class.kind = DiagnosticKind::MissingFile;
        } else if source.is_some() {
            class.kind = DiagnosticKind::LatexError;
        }
        class.continuation = source.map(|(name, _)| name);
    }
    class
}

/// `File `x.sty' not found` (LaTeX) or `I can't find file `x'` (TeX).
fn is_missing_file(text: &str) -> bool {
    missing_file_name(text).is_some() || text.starts_with("I can't find file")
}

/// The file name of LaTeX's ``File `x.sty' not found``.
pub(crate) fn missing_file_name(text: &str) -> Option<&str> {
    ["File `", "File '"].iter().find_map(|open| {
        let start = text.find(open)? + open.len();
        let len = text[start..].find("' not found")?;
        Some(&text[start..start + len])
    })
}

/// Recognizes `LaTeX <what>: `, `LaTeX Font <what>: `, `Package <name>
/// <what>: ` and `Class <name> <what>: ` and returns the continuation-line
/// prefix name and the text after the colon.
fn source_prefix<'a>(text: &'a str, what: &str) -> Option<(&'a str, &'a str)> {
    let rest_after = |rest: &'a str| {
        rest.strip_prefix(what)
            .and_then(|r| r.strip_prefix(':'))
            .map(str::trim_start)
    };
    if let Some(rest) = text.strip_prefix("LaTeX ") {
        if let Some(body) = rest_after(rest) {
            return Some(("LaTeX", body));
        }
        let body = rest.strip_prefix("Font ").and_then(rest_after)?;
        return Some(("Font", body));
    }
    let rest = text
        .strip_prefix("Package ")
        .or_else(|| text.strip_prefix("Class "))?;
    let (name, rest) = rest.split_once(' ')?;
    let body = rest_after(rest)?;
    (!name.is_empty()).then_some((name, body))
}

/// A LaTeX / package / class / pdfTeX warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WarningHeader<'a> {
    /// Prefix of continuation lines (`(name)`), if the warning can span lines.
    pub continuation: Option<&'a str>,
    /// The text after `... Warning: `.
    pub body: &'a str,
}

pub(crate) fn warning_header(line: &str) -> Option<WarningHeader<'_>> {
    if let Some((name, body)) = source_prefix(line, "Warning") {
        return Some(WarningHeader {
            continuation: Some(name),
            body,
        });
    }
    line.starts_with("pdfTeX warning").then_some(WarningHeader {
        continuation: None,
        body: line,
    })
}

/// Returns the text of a continuation line of a multi-line LaTeX message:
/// `(name)   text` or an indented line.
pub(crate) fn continuation<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    if line.trim().is_empty() {
        return None;
    }
    if let Some(rest) = line
        .strip_prefix('(')
        .and_then(|r| r.strip_prefix(name))
        .and_then(|r| r.strip_prefix(')'))
    {
        return Some(rest.trim());
    }
    line.starts_with(char::is_whitespace).then(|| line.trim())
}

/// Severity and kind of a warning, from its body text.
pub(crate) fn classify_warning(body: &str) -> (Severity, DiagnosticKind) {
    if body.contains("Rerun") || body.contains("rerun") {
        (Severity::Info, DiagnosticKind::RerunRequired)
    } else if body.starts_with("Reference ") && body.contains(" undefined") {
        (Severity::Warning, DiagnosticKind::UndefinedReference)
    } else if body.starts_with("Citation ") && body.contains(" undefined") {
        (Severity::Warning, DiagnosticKind::UndefinedCitation)
    } else {
        (Severity::Warning, DiagnosticKind::Other)
    }
}

/// `There were undefined references.` / `... citations.`: a summary that
/// repeats the individual warnings.
pub(crate) fn undefined_summary(body: &str) -> Option<DiagnosticKind> {
    if body.starts_with("There were undefined references") {
        Some(DiagnosticKind::UndefinedReference)
    } else if body.starts_with("There were undefined citations") {
        Some(DiagnosticKind::UndefinedCitation)
    } else {
        None
    }
}

/// `Overfull \hbox ...` and friends.
pub(crate) fn box_header(line: &str) -> Option<DiagnosticKind> {
    let (kind, rest) = if let Some(rest) = line.strip_prefix("Overfull \\") {
        (DiagnosticKind::OverfullBox, rest)
    } else {
        (
            DiagnosticKind::UnderfullBox,
            line.strip_prefix("Underfull \\")?,
        )
    };
    (rest.starts_with("hbox") || rest.starts_with("vbox")).then_some(kind)
}

/// The first line number of `... at lines 3--5` / `... detected at line 7`.
pub(crate) fn box_line(line: &str) -> Option<u32> {
    number_after(line, "at lines ").or_else(|| number_after(line, "at line "))
}

/// `... on input line 12.`
pub(crate) fn input_line(text: &str) -> Option<u32> {
    let idx = text.rfind("on input line ")?;
    number_at(&text[idx + "on input line ".len()..])
}

/// The line number of a `l.<n> <context>` line.
pub(crate) fn context_line(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("l.")?;
    let digits = leading_digits(rest);
    let after = &rest[digits..];
    if !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    parse_line_number(&rest[..digits])
}

/// The text after `l.<n> ` of a context line (`""` for a bare `l.<n>`).
pub(crate) fn context_text(line: &str) -> Option<&str> {
    context_line(line)?;
    let rest = line.strip_prefix("l.")?;
    let rest = &rest[leading_digits(rest)..];
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// The control sequence at the end of the first context line of an
/// `Undefined control sequence` error, e.g. `\foo` in `l.5 \foo`.
pub(crate) fn trailing_control_sequence(line: &str) -> Option<&str> {
    let line = line.trim_end();
    let idx = line.rfind('\\')?;
    let name = &line[idx + 1..];
    let mut chars = name.chars();
    let valid = match (chars.next(), chars.next()) {
        (Some(c), None) => !c.is_whitespace(),
        (Some(_), Some(_)) => name.chars().all(|c| c.is_ascii_alphabetic() || c == '@'),
        (None, _) => false,
    };
    // `\\` at the end is a control symbol, not the start of a name.
    (valid && !line[..idx].ends_with('\\')).then_some(&line[idx..])
}

fn number_after(text: &str, marker: &str) -> Option<u32> {
    let idx = text.find(marker)?;
    number_at(&text[idx + marker.len()..])
}

fn number_at(text: &str) -> Option<u32> {
    parse_line_number(&text[..leading_digits(text)])
}

fn leading_digits(text: &str) -> usize {
    text.bytes().take_while(u8::is_ascii_digit).count()
}

/// A 1-based line number; `0` and overflowing values are rejected.
fn parse_line_number(digits: &str) -> Option<u32> {
    digits.parse().ok().filter(|&n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_headers() {
        let h = error_header("./main.tex:5: Undefined control sequence.").unwrap();
        assert_eq!(h.file, Some("./main.tex"));
        assert_eq!(h.line, Some(5));
        assert_eq!(h.text, "Undefined control sequence.");

        let h = error_header("! LaTeX Error: File `x.sty' not found.").unwrap();
        assert_eq!((h.file, h.line), (None, None));
        assert_eq!(h.text, "LaTeX Error: File `x.sty' not found.");

        let h = error_header("./main.tex:4:  ==> Fatal error occurred, no output").unwrap();
        assert_eq!(h.text, "==> Fatal error occurred, no output");

        for not_header in [
            "!",
            "!!",
            "LaTeX Font Info:    ... okay on input line 2.",
            "main:5: no extension",
            "./main.tex:5:no space",
            "./main.tex:: x",
            "./main.tex:0: line zero",
            "(./a.tex:5: in a paren",
            "  ./main.tex:5: indented",
            "./main.tex:5: ",
        ] {
            let header = error_header(not_header);
            assert!(
                header.is_none() || header.unwrap().line.is_none(),
                "{not_header:?} -> {header:?}"
            );
        }
        assert!(error_header("./main.tex:0: x").unwrap().line.is_none());
        assert!(
            error_header("./a.tex:99999999999: x")
                .unwrap()
                .line
                .is_none()
        );
        // Parentheses inside the path are fine.
        let h = error_header("./a(1).tex:3: Undefined control sequence.").unwrap();
        assert_eq!((h.file, h.line), (Some("./a(1).tex"), Some(3)));
    }

    #[test]
    fn missing_file_names_and_loaders() {
        assert_eq!(
            missing_file_name("LaTeX Error: File `tikz.sty' not found."),
            Some("tikz.sty")
        );
        assert_eq!(missing_file_name("LaTeX Error: File `x"), None);
    }

    #[test]
    fn error_classes() {
        let kind = |t| classify_error(t).kind;
        assert_eq!(
            kind("Undefined control sequence."),
            DiagnosticKind::UndefinedControlSequence
        );
        assert_eq!(
            kind("LaTeX Error: File `x.sty' not found."),
            DiagnosticKind::MissingFile
        );
        assert_eq!(
            kind("LaTeX Error: File `nosuch.cls' not found."),
            DiagnosticKind::MissingFile
        );
        assert_eq!(
            kind("I can't find file `foo'."),
            DiagnosticKind::MissingFile
        );
        assert_eq!(
            kind("LaTeX Error: Environment foo undefined."),
            DiagnosticKind::LatexError
        );
        let c = classify_error("Package babel Error: Unknown option `x'.");
        assert_eq!(c.kind, DiagnosticKind::LatexError);
        assert_eq!(c.continuation, Some("babel"));
        assert_eq!(
            kind("Class article Error: bad option."),
            DiagnosticKind::LatexError
        );
        assert_eq!(kind("Missing $ inserted."), DiagnosticKind::Other);
        assert_eq!(kind("Emergency stop."), DiagnosticKind::EmergencyStop);
        let c = classify_error("==> Fatal error occurred, no output PDF file produced!");
        assert!(c.fatal_summary);
        assert_eq!(c.kind, DiagnosticKind::EmergencyStop);
    }

    #[test]
    fn warnings() {
        let w = warning_header("LaTeX Warning: Reference `a' on page 1 undefined on input line 3.")
            .unwrap();
        assert_eq!(w.continuation, Some("LaTeX"));
        assert_eq!(
            classify_warning(w.body),
            (Severity::Warning, DiagnosticKind::UndefinedReference)
        );
        assert_eq!(input_line(w.body), Some(3));

        let w = warning_header("Package natbib Warning: Citation `k' on page 1 undefined").unwrap();
        assert_eq!(w.continuation, Some("natbib"));
        assert_eq!(
            classify_warning(w.body).1,
            DiagnosticKind::UndefinedCitation
        );

        let w = warning_header("LaTeX Font Warning: Font shape `x' undefined").unwrap();
        assert_eq!(w.continuation, Some("Font"));
        assert_eq!(classify_warning(w.body).1, DiagnosticKind::Other);

        assert_eq!(
            classify_warning("Label(s) may have changed. Rerun to get cross-references right."),
            (Severity::Info, DiagnosticKind::RerunRequired)
        );
        assert_eq!(
            classify_warning("Citation(s) may have changed. Rerun to get citations correct."),
            (Severity::Info, DiagnosticKind::RerunRequired)
        );
        assert!(warning_header("pdfTeX warning (ext4): destination").is_some());
        assert!(warning_header("Package: amsmath 2024/11/05").is_none());
        assert!(warning_header("LaTeX Font Info:    Checking").is_none());
        assert_eq!(
            undefined_summary("There were undefined references."),
            Some(DiagnosticKind::UndefinedReference)
        );
    }

    #[test]
    fn continuation_lines() {
        assert_eq!(
            continuation("(babel)       more text.", "babel"),
            Some("more text.")
        );
        assert_eq!(
            continuation("               indented", "LaTeX"),
            Some("indented")
        );
        assert_eq!(continuation("(./x.tex", "LaTeX"), None);
        assert_eq!(continuation("   ", "LaTeX"), None);
    }

    #[test]
    fn boxes() {
        assert_eq!(
            box_header("Overfull \\hbox (115.0914pt too wide) detected at line 7"),
            Some(DiagnosticKind::OverfullBox)
        );
        assert_eq!(
            box_header("Underfull \\vbox (badness 10000) has occurred while \\output is active"),
            Some(DiagnosticKind::UnderfullBox)
        );
        assert_eq!(box_header("Overfull \\foo"), None);
        assert_eq!(
            box_line("Overfull \\hbox (1pt too wide) in paragraph at lines 3--5"),
            Some(3)
        );
        assert_eq!(
            box_line("Underfull \\hbox (badness 10000) detected at line 9"),
            Some(9)
        );
        assert_eq!(
            box_line("Overfull \\vbox (1pt too high) has occurred"),
            None
        );
    }

    #[test]
    fn context_and_control_sequence() {
        assert_eq!(context_line("l.5 \\foo"), Some(5));
        assert_eq!(context_line("l.12"), Some(12));
        assert_eq!(context_line("l.x"), None);
        assert_eq!(context_line("l.5x"), None);
        assert_eq!(context_text("l.5 \\foo "), Some("\\foo "));
        assert_eq!(context_text("l.8 ^^M"), Some("^^M"));
        assert_eq!(context_text("l.12"), Some(""));
        assert_eq!(context_text("l.x \\foo"), None);
        assert_eq!(trailing_control_sequence("l.5 \\foo"), Some("\\foo"));
        assert_eq!(
            trailing_control_sequence("\\mymacro ->\\foo@bar "),
            Some("\\foo@bar")
        );
        assert_eq!(trailing_control_sequence("l.5 x\\&"), Some("\\&"));
        assert_eq!(trailing_control_sequence("l.4 ...longname"), None);
        assert_eq!(trailing_control_sequence("l.4 \\foo1"), None);
        assert_eq!(trailing_control_sequence("l.4 a\\\\"), None);
    }

    #[test]
    fn file_like_tokens() {
        for yes in ["./main.tex", "../x", "/usr/a.sty", "main.aux", "sub/a.tex"] {
            assert!(looks_like_file(yes), "{yes}");
        }
        for no in ["", "Font", "e.g.", "see", "Default", ".hidden"] {
            assert!(!looks_like_file(no), "{no}");
        }
    }
}
