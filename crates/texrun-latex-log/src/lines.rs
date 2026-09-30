//! Splitting a raw log into lines.

/// The logical lines of a log, decoded as UTF-8 (invalid sequences replaced
/// by U+FFFD), without line terminators.
///
/// All lines share one buffer, so a log of millions of short lines costs
/// 16 bytes of bookkeeping per line rather than one allocation each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Lines {
    /// Every line followed by `\n`.
    text: String,
    spans: Vec<Span>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    /// Length of the line in the raw log, in bytes. TeX wraps log lines by
    /// byte count, so this (not the decoded length) is compared against
    /// `max_print_line`.
    raw_len: usize,
}

impl Lines {
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Line `i`, or `""` past the end.
    pub fn text(&self, i: usize) -> &str {
        let Some(span) = self.spans.get(i) else {
            return "";
        };
        let end = self
            .spans
            .get(i + 1)
            .map_or(self.text.len(), |next| next.start)
            - 1;
        &self.text[span.start..end]
    }

    /// Raw byte length of line `i` (0 past the end).
    pub fn raw_len(&self, i: usize) -> usize {
        self.spans.get(i).map_or(0, |s| s.raw_len)
    }

    fn push(&mut self, raw: &[u8]) {
        self.spans.push(Span {
            start: self.text.len(),
            raw_len: raw.len(),
        });
        self.text.push_str(&String::from_utf8_lossy(raw));
        self.text.push('\n');
    }
}

/// Splits `log` at `\n` (stripping a trailing `\r`).
///
/// With `join_width = Some(n)`, a physical line of exactly `n` bytes is taken
/// to be a line that TeX wrapped at `max_print_line = n`, and is joined with
/// the following one. Joining happens on raw bytes, so a multi-byte UTF-8
/// character split by the wrap is restored.
pub(crate) fn split(log: &[u8], join_width: Option<usize>) -> Lines {
    let join_width = join_width.filter(|&w| w > 0);
    let mut out = Lines::default();
    let mut pending: Vec<u8> = Vec::new();
    for raw in log.split(|&b| b == b'\n') {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if join_width == Some(raw.len()) {
            pending.extend_from_slice(raw);
            continue;
        }
        if pending.is_empty() {
            out.push(raw);
        } else {
            pending.extend_from_slice(raw);
            out.push(&pending);
            pending.clear();
        }
    }
    if !pending.is_empty() {
        out.push(&pending);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &Lines) -> Vec<&str> {
        (0..lines.len()).map(|i| lines.text(i)).collect()
    }

    #[test]
    fn splits_lf_and_crlf() {
        let lines = split(b"a\r\nb\nc", None);
        assert_eq!(texts(&lines), ["a", "b", "c"]);
        assert_eq!(lines.raw_len(0), 1);
        assert_eq!(lines.text(3), "");
        assert_eq!(lines.raw_len(3), 0);
    }

    #[test]
    fn joins_lines_of_exactly_the_wrap_width() {
        let lines = split(b"abc\ndef\ngh\nijk", Some(3));
        assert_eq!(texts(&lines), ["abcdefgh", "ijk"]);
        assert_eq!(lines.raw_len(0), 8);
    }

    #[test]
    fn joining_restores_split_utf8() {
        // "é" is 0xC3 0xA9; TeX may wrap between the two bytes.
        let lines = split(b"ab\xC3\n\xA9x\n", Some(3));
        assert_eq!(texts(&lines)[0], "ab\u{e9}x");
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let lines = split(b"a\xFFb", None);
        assert_eq!(texts(&lines), ["a\u{FFFD}b"]);
        assert_eq!(lines.raw_len(0), 3);
    }

    #[test]
    fn zero_width_disables_joining() {
        assert_eq!(texts(&split(b"\n\n", Some(0))), ["", "", ""]);
    }
}
