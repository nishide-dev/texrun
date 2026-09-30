//! Best-effort tracking of the file TeX is currently reading.
//!
//! TeX writes `(` followed by the file name when it opens an input file and
//! `)` when it closes it, interleaved with arbitrary other output. Parentheses
//! that do not start a file name (e.g. `(Font)` or `(see above)`) are tracked
//! too, so that their `)` does not close a file.

use crate::patterns;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A `(` that does not start a file name. Transparent for
    /// [`FileStack::current`].
    Paren,
    /// A file opened with the name as printed by TeX.
    File(String),
    /// A file whose name may have been cut by line wrapping.
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FileStack {
    entries: Vec<Entry>,
}

impl FileStack {
    /// Processes the parentheses of one log line.
    ///
    /// `may_continue` says that the line might have been wrapped by TeX, so a
    /// file name reaching its end might be incomplete.
    pub fn feed(&mut self, line: &str, may_continue: bool) {
        let mut rest = line;
        while let Some(pos) = rest.find(['(', ')']) {
            if rest.as_bytes()[pos] == b')' {
                self.entries.pop();
                rest = &rest[pos + 1..];
                continue;
            }
            let after = &rest[pos + 1..];
            let (token, len) = file_token(after);
            if patterns::looks_like_file(token) {
                let truncated = may_continue && len == after.len();
                self.entries.push(if truncated {
                    Entry::Unknown
                } else {
                    Entry::File(token.to_owned())
                });
                rest = &after[len..];
            } else {
                self.entries.push(Entry::Paren);
                rest = after;
            }
        }
    }

    /// The innermost open file, as printed by TeX. `None` when no file is
    /// open or the innermost one is not known.
    pub fn current(&self) -> Option<&str> {
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

/// Splits a candidate file name off the text following `(`. Returns the name
/// and the number of bytes it occupies (including quotes).
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
    fn quoted_names_may_contain_spaces() {
        assert_eq!(
            fed(&["(\"./my chapter.tex\" [1]"]).current(),
            Some("./my chapter.tex")
        );
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
