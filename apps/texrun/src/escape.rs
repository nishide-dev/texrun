//! Escaping of untrusted text for terminal output.
//!
//! File names, diagnostic messages and host paths may come from untrusted
//! documents (docs/security.md §2). Before they are written to a terminal,
//! characters that could change how the output looks are replaced with
//! visible escapes:
//!
//! - control characters (C0, DEL, C1), including newlines, so an ANSI escape
//!   sequence or a fake extra line cannot be injected;
//! - format characters (Unicode general category Cf): bidi controls
//!   (U+202A..U+202E, U+2066..U+2069, U+200E / U+200F, U+061C), zero-width
//!   characters (U+200B..U+200D, U+2060, U+FEFF) and the rest of Cf;
//! - the line and paragraph separators U+2028 / U+2029.
//!
//! JSON output is not escaped this way: it is valid JSON either way, and
//! handling the strings is up to the consumer (docs/security.md §2).

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;

/// `s` with every character that needs escaping replaced by `\t`, `\n`,
/// `\r` or `\u{XXXX}`. Borrows when nothing needs escaping.
pub fn escape(s: &str) -> Cow<'_, str> {
    if !s.chars().any(needs_escape) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if needs_escape(c) => {
                let _ = write!(out, "\\u{{{:04X}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// A host path for display: lossily converted to UTF-8, then escaped.
pub fn escape_path(path: &Path) -> String {
    escape(&path.to_string_lossy()).into_owned()
}

fn needs_escape(c: char) -> bool {
    c.is_control() || is_format(c) || matches!(c, '\u{2028}' | '\u{2029}')
}

/// Unicode general category Cf (format), as of Unicode 16.
fn is_format(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed() {
        let s = "chapters/第1章 intro.tex: Undefined control sequence \\foo";
        assert!(matches!(escape(s), Cow::Borrowed(_)));
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(escape("a\nb\tc\rd"), "a\\nb\\tc\\rd");
        assert_eq!(escape("\u{1b}[31mred"), "\\u{001B}[31mred");
        assert_eq!(escape("del\u{7f}"), "del\\u{007F}");
        assert_eq!(escape("c1\u{85}"), "c1\\u{0085}");
    }

    #[test]
    fn bidi_and_zero_width_characters_are_escaped() {
        assert_eq!(escape("evil\u{202E}fdp.tex"), "evil\\u{202E}fdp.tex");
        assert_eq!(escape("\u{2066}x\u{2069}"), "\\u{2066}x\\u{2069}");
        assert_eq!(escape("a\u{200B}b\u{200D}c"), "a\\u{200B}b\\u{200D}c");
        assert_eq!(escape("\u{FEFF}bom"), "\\u{FEFF}bom");
        assert_eq!(
            escape("\u{200E}\u{200F}\u{061C}"),
            "\\u{200E}\\u{200F}\\u{061C}"
        );
        assert_eq!(escape("tag\u{E0041}"), "tag\\u{E0041}");
        assert_eq!(escape("line\u{2028}sep"), "line\\u{2028}sep");
    }

    #[test]
    fn paths_are_escaped() {
        assert_eq!(
            escape_path(Path::new("out/\u{202E}x.pdf")),
            "out/\\u{202E}x.pdf"
        );
    }
}
