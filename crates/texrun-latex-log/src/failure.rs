//! An error for a compile that failed without a recognized error.

use texrun_core::{Diagnostic, DiagnosticKind, Severity};

use crate::parser::{MAX_EXCERPT_BYTES, MAX_MESSAGE_BYTES, prefix, sanitize};

/// At most this many bytes at the end of the log are looked at.
const MAX_TAIL_BYTES: usize = 16 * 1024;
/// At most this many non-blank lines at the end of the log are excerpted.
const MAX_TAIL_LINES: usize = 12;

/// An error saying that a compile failed although no error was recognized
/// in its log (`log`, possibly empty if there was none), for callers that
/// know the compile failed and found no error diagnostic: a failed compile
/// should always come with an error.
///
/// The [`Diagnostic::raw_excerpt`] is the last non-blank lines of the log
/// (at most 12, from its last 16 KiB), where TeX says how the job ended.
/// When one of them starts with `!` (an error in a form the parser does not
/// know), the last such line is quoted in the message. No file or line is
/// given. Costs at most a constant amount of work, whatever the log size.
///
/// ```
/// use texrun_core::{DiagnosticKind, Severity};
/// use texrun_latex_log::unexplained_failure;
///
/// let d = unexplained_failure(b"(./main.tex)\n!XeTeX error: something odd\n");
/// assert_eq!((d.severity, d.kind), (Severity::Error, DiagnosticKind::Other));
/// assert!(d.message.contains("something odd"));
/// assert!(d.file.is_none() && d.line.is_none());
/// ```
pub fn unexplained_failure(log: &[u8]) -> Diagnostic {
    let mut start = log.len().saturating_sub(MAX_TAIL_BYTES);
    // Start at a line boundary, unless the tail is a single huge line.
    if start > 0
        && let Some(nl) = log[start..].iter().position(|&b| b == b'\n')
        && nl + 1 < log.len() - start
    {
        start += nl + 1;
    }
    let tail = String::from_utf8_lossy(&log[start..]);
    let lines: Vec<&str> = tail
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(MAX_TAIL_LINES)
        .collect();

    if lines.is_empty() {
        return Diagnostic::new(
            Severity::Error,
            DiagnosticKind::Other,
            "the compile failed, but TeX wrote no log to explain why",
        );
    }

    let bang = lines
        .iter()
        .find_map(|l| l.strip_prefix('!').map(str::trim))
        .filter(|t| !t.is_empty());
    let message = match bang {
        Some(text) => format!("the compile failed: {text}"),
        None => "the compile failed, but no error was recognized in the log; read the end of \
                 the log"
            .to_owned(),
    };
    let mut excerpt = String::new();
    for line in lines.iter().rev() {
        if !excerpt.is_empty() {
            excerpt.push('\n');
        }
        excerpt.push_str(&sanitize(line, MAX_EXCERPT_BYTES));
        if excerpt.len() >= MAX_EXCERPT_BYTES {
            break;
        }
    }
    let keep = prefix(&excerpt, MAX_EXCERPT_BYTES).len();
    excerpt.truncate(keep);
    Diagnostic::new(
        Severity::Error,
        DiagnosticKind::Other,
        sanitize(&message, MAX_MESSAGE_BYTES),
    )
    .with_raw_excerpt(excerpt)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    #[test]
    fn empty_log() {
        for log in [&b""[..], b"\n  \n"] {
            let d = unexplained_failure(log);
            assert_eq!(d.severity, Severity::Error);
            assert!(d.message.contains("no log"), "{}", d.message);
            assert_eq!(d.raw_excerpt, None);
        }
    }

    #[test]
    fn excerpts_the_last_lines() {
        let mut log = String::new();
        for n in 0..100 {
            writeln!(log, "line {n}\n").unwrap();
        }
        let d = unexplained_failure(log.as_bytes());
        assert!(d.message.contains("no error was recognized"));
        let excerpt = d.raw_excerpt.unwrap();
        let lines: Vec<_> = excerpt.lines().collect();
        assert_eq!(lines.len(), MAX_TAIL_LINES);
        assert_eq!(lines.first(), Some(&"line 88"));
        assert_eq!(lines.last(), Some(&"line 99"));
    }

    #[test]
    fn quotes_the_last_bang_line() {
        let d = unexplained_failure(b"! first\n!second\nHere is how much\n");
        assert_eq!(d.message, "the compile failed: second");
        // A bare `!` is not quoted.
        let d = unexplained_failure(b"!\nend\n");
        assert!(d.message.contains("no error was recognized"));
    }

    #[test]
    fn is_bounded_and_safe() {
        let huge = vec![b'x'; 10 * MAX_TAIL_BYTES];
        let d = unexplained_failure(&huge);
        assert!(d.raw_excerpt.unwrap().len() <= MAX_EXCERPT_BYTES);

        let mut log = b"start\n".to_vec();
        log.extend(std::iter::repeat_n(b'y', 3 * MAX_TAIL_BYTES));
        log.extend(b"\n!\x1b[31m\xff boom\n");
        let d = unexplained_failure(&log);
        assert!(d.message.len() <= MAX_MESSAGE_BYTES);
        assert!(!d.message.contains('\u{1b}'), "{}", d.message);
        let excerpt = d.raw_excerpt.unwrap();
        assert!(excerpt.len() <= MAX_EXCERPT_BYTES);
        assert!(!excerpt.contains("start"));
    }
}
