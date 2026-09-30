//! Best-effort tracking of the file TeX is currently reading.
//!
//! TeX writes `(` followed by the file name when it opens an input file and
//! `)` when it closes it, interleaved with arbitrary other output. Parentheses
//! that do not start a file name (e.g. `(Font)` or `(see above)`) are tracked
//! too, so that their `)` does not close a file.
//!
//! Whenever the name of the innermost file is uncertain, [`FileStack::current`]
//! returns `None`: attributing a diagnostic to no file is better than to a
//! wrong (possibly nonexistent) one.

use crate::patterns;

/// Maximum number of open entries. TeX itself nests at most `max_in_open`
/// (default 15) input files; deeper nesting only comes from stray `(`.
const MAX_DEPTH: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A `(` that does not start a file name. Transparent for
    /// [`FileStack::current`].
    Paren,
    /// A file opened with the name as printed by TeX.
    File(String),
    /// A file whose name is uncertain: it may have been cut by line
    /// wrapping, or it contains spaces or parentheses, which TeX prints
    /// without quoting (so the end of the name cannot be told apart from
    /// following output).
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FileStack {
    entries: Vec<Entry>,
    /// `(` seen beyond [`MAX_DEPTH`] and not closed yet.
    overflow: usize,
}

impl FileStack {
    /// Processes the parentheses of one log line.
    ///
    /// `may_continue` says that the line might have been wrapped by TeX, so a
    /// file name reaching its end might be incomplete.
    ///
    /// Non-file parentheses are assumed to close on the line they open on
    /// (as `(Font)`, `(see above)` or `(1 page, 123 bytes)` do). One that is
    /// still open at the end of the line (e.g. from `\typeout{a (b}`) is
    /// discarded, so that the next `)` closes the file it belongs to.
    pub fn feed(&mut self, line: &str, may_continue: bool) {
        // Entries at or above `low` were all pushed while reading this line.
        let mut low = self.entries.len();
        let mut rest = line;
        while let Some(pos) = rest.find(['(', ')']) {
            if rest.as_bytes()[pos] == b')' {
                if self.overflow > 0 {
                    self.overflow -= 1;
                } else {
                    self.entries.pop();
                    low = low.min(self.entries.len());
                }
                rest = &rest[pos + 1..];
                continue;
            }
            let after = &rest[pos + 1..];
            let (token, len) = file_token(after);
            let entry = if patterns::looks_like_file(token) {
                let truncated = may_continue && len == after.len();
                let tail = &after[len..];
                rest = tail;
                if truncated || !ends_file_name(tail) {
                    Entry::Unknown
                } else {
                    Entry::File(token.to_owned())
                }
            } else {
                rest = after;
                Entry::Paren
            };
            if self.entries.len() < MAX_DEPTH && self.overflow == 0 {
                self.entries.push(entry);
            } else {
                self.overflow += 1;
            }
        }
        if self.entries.len() > low {
            let mut index = 0;
            self.entries.retain(|entry| {
                let keep = index < low || *entry != Entry::Paren;
                index += 1;
                keep
            });
        }
    }

    /// The innermost open file, as printed by TeX. `None` when no file is
    /// open or the innermost one is not known.
    pub fn current(&self) -> Option<&str> {
        if self.overflow > 0 {
            return None;
        }
        for entry in self.entries.iter().rev() {
            match entry {
                Entry::Paren => {}
                Entry::File(name) => return Some(name),
                Entry::Unknown => return None,
            }
        }
        None
    }
}

/// Whether `tail`, the text right after a candidate file name, is something
/// TeX prints after a file name: the end of the line, `)` (the file is
/// closed), or spaces followed by the end of the line or the next `(`, `[`,
/// `{`, `<` or `)`. Anything else means the name was cut at a space or a
/// parenthesis that belongs to it.
fn ends_file_name(tail: &str) -> bool {
    if tail.is_empty() || tail.starts_with(')') {
        return true;
    }
    tail.starts_with(char::is_whitespace)
        && tail
            .trim_start()
            .chars()
            .next()
            .is_none_or(|c| "([{<)".contains(c))
}

/// Splits a candidate file name off the text following `(`. Returns the name
/// and the number of bytes it occupies (including quotes).
///
/// pdfTeX in TeX Live 2024 prints names unquoted even when they contain
/// spaces; the quoted form is handled for other engines / versions.
fn file_token(after: &str) -> (&str, usize) {
    if let Some(quoted) = after.strip_prefix('"') {
        return match quoted.find('"') {
            Some(end) => (&quoted[..end], end + 2),
            None => (quoted, after.len()),
        };
    }
    let len = after
        .find(|c: char| c.is_whitespace() || "()[]{}<>\"".contains(c))
        .unwrap_or(after.len());
    (&after[..len], len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(lines: &[&str]) -> FileStack {
        let mut s = FileStack::default();
        for line in lines {
            s.feed(line, false);
        }
        s
    }

    #[test]
    fn tracks_nested_files() {
        let s = fed(&["(./main.tex", "(/usr/share/texlive/article.cls", ")"]);
        assert_eq!(s.current(), Some("./main.tex"));
        let s = fed(&["(./main.tex", ") (./chapters/intro.tex [1]"]);
        assert_eq!(s.current(), Some("./chapters/intro.tex"));
        assert_eq!(fed(&["(./main.tex (./main.aux))"]).current(), None);
    }

    #[test]
    fn non_file_parentheses_are_transparent() {
        let s = fed(&["(./main.tex", "(Font)  size <7> (see above)", "(e.g. this"]);
        assert_eq!(s.current(), Some("./main.tex"));
        let s = fed(&["(./main.tex", "(Font)  size <7> (see above)", "(e.g. this)"]);
        assert_eq!(s.current(), Some("./main.tex"));
    }

    #[test]
    fn non_file_parentheses_do_not_span_lines() {
        // `\typeout{Note (unbalanced open}` inside chapters/c.tex: the `)`
        // on the next line closes c.tex, not the stray `(`.
        let s = fed(&[
            "(./main.tex",
            " (./chapters/c.tex",
            "Note (unbalanced open",
            ")",
        ]);
        assert_eq!(s.current(), Some("./main.tex"));
        // Closed on the same line: kept balanced.
        let s = fed(&["(./main.tex", "(./c.tex (a (b) c) d", ")"]);
        assert_eq!(s.current(), Some("./main.tex"));
        // Too many `)` stay a degradation to "no file".
        assert_eq!(fed(&["(./main.tex", "A stray ) close"]).current(), None);
    }

    #[test]
    fn names_with_spaces_or_parentheses_are_unknown() {
        // pdfTeX (TeX Live 2024) prints such names unquoted.
        let mut s = fed(&["(./main.tex", " (./my dir/chap one.tex"]);
        assert_eq!(s.current(), None);
        s.feed(")", false);
        assert_eq!(s.current(), Some("./main.tex"));

        let mut s = fed(&["(./main.tex", "(./a(1).tex"]);
        assert_eq!(s.current(), None);
        s.feed(")", false);
        assert_eq!(s.current(), Some("./main.tex"));

        // What TeX does print after a complete name.
        for line in [
            "(./a.tex",
            "(./a.tex [1]",
            "(./a.tex  (./b.sty)",
            "(./a.tex {x.map}",
        ] {
            assert_eq!(fed(&[line]).current(), Some("./a.tex"), "{line}");
        }
        assert_eq!(fed(&["(./a.tex text"]).current(), None);
    }

    #[test]
    fn quoted_names_may_contain_spaces() {
        // Not produced by pdfTeX in TeX Live 2024, but by some other
        // engines / versions.
        assert_eq!(
            fed(&["(\"./my chapter.tex\" [1]"]).current(),
            Some("./my chapter.tex")
        );
    }

    #[test]
    fn depth_is_bounded() {
        let mut s = fed(&["(./main.tex"]);
        let deep = "(./a.tex ".repeat(MAX_DEPTH + 10);
        s.feed(&deep, false);
        assert_eq!(s.entries.len(), MAX_DEPTH);
        assert_eq!(s.current(), None);
        s.feed(&")".repeat(MAX_DEPTH + 10), false);
        assert_eq!(s.current(), Some("./main.tex"));
    }

    #[test]
    fn possibly_wrapped_names_are_unknown() {
        let mut s = fed(&["(./main.tex"]);
        s.feed("(./some/long/na", true);
        assert_eq!(s.current(), None);
        s.feed(")", false);
        assert_eq!(s.current(), Some("./main.tex"));
        // Not at the end of the line: the name is complete.
        s.feed("(./a.tex [1]", true);
        assert_eq!(s.current(), Some("./a.tex"));
    }

    #[test]
    fn unbalanced_close_is_ignored() {
        assert_eq!(fed(&["))) (./x.tex"]).current(), Some("./x.tex"));
    }
}
