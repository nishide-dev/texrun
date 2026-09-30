//! Splitting a raw log into lines.

/// One logical log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    /// The line decoded as UTF-8 (invalid sequences replaced by U+FFFD),
    /// without the line terminator.
    pub text: String,
    /// Length of the line in the raw log, in bytes. TeX wraps log lines by
    /// byte count, so this (not `text.len()`) is compared against
    /// `max_print_line`.
    pub raw_len: usize,
}

/// Splits `log` at `\n` (stripping a trailing `\r`).
///
/// With `join_width = Some(n)`, a physical line of exactly `n` bytes is taken
/// to be a line that TeX wrapped at `max_print_line = n`, and is joined with
/// the following one. Joining happens on raw bytes, so a multi-byte UTF-8
/// character split by the wrap is restored.
pub(crate) fn split(log: &[u8], join_width: Option<usize>) -> Vec<Line> {
    let join_width = join_width.filter(|&w| w > 0);
    let mut out = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    for raw in log.split(|&b| b == b'\n') {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        pending.extend_from_slice(raw);
        if join_width == Some(raw.len()) {
            continue;
        }
        out.push(finish(&pending));
        pending.clear();
    }
    if !pending.is_empty() {
        out.push(finish(&pending));
    }
    out
}

fn finish(raw: &[u8]) -> Line {
    Line {
        text: String::from_utf8_lossy(raw).into_owned(),
        raw_len: raw.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn splits_lf_and_crlf() {
        let lines = split(b"a\r\nb\nc", None);
        assert_eq!(texts(&lines), ["a", "b", "c"]);
        assert_eq!(lines[0].raw_len, 1);
    }

    #[test]
    fn joins_lines_of_exactly_the_wrap_width() {
        let lines = split(b"abc\ndef\ngh\nijk", Some(3));
        assert_eq!(texts(&lines), ["abcdefgh", "ijk"]);
        assert_eq!(lines[0].raw_len, 8);
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
        assert_eq!(lines[0].raw_len, 3);
    }

    #[test]
    fn zero_width_disables_joining() {
        assert_eq!(texts(&split(b"\n\n", Some(0))), ["", "", ""]);
    }
}
