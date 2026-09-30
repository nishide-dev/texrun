//! Locating the `\usepackage` / `\documentclass` / ... that requested a
//! missing package or class.
//!
//! LaTeX looks ahead for an optional `[<date>]` argument before it tries to
//! load a package or class, so when the file is missing TeX reports the
//! position of the *next* token (`l.<n>` of the `Emergency stop.`), usually
//! on a later line. The request is located by reading backwards from that
//! position, which the context line gives exactly:
//!
//! ```text
//! l.3 \usepackage          <- `before`: line 3 up to the looked-ahead token
//!                {amsmath}^^M   <- `after`: the rest of line 3
//! ```
//!
//! On line `n` itself, `before` shows what precedes the token. Earlier lines
//! are only known from the source file, if the caller provides it; it is
//! used only when its line `n` is exactly what the log shows. Between the
//! request and the looked-ahead token there can only be spaces, line ends and
//! comments (a blank line would itself be the token, `\par`), so the text
//! before the token must end with `\usepackage[<options>]{<list>}` whose list
//! names the missing package. Anything else yields `None`.

/// A missing package or class, from ``File `<name>.sty' not found.``.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Missing<'a> {
    stem: &'a str,
    commands: &'static [&'static str],
}

const PACKAGE_COMMANDS: &[&str] = &["usepackage", "RequirePackage", "RequirePackageWithOptions"];
const CLASS_COMMANDS: &[&str] = &["documentclass", "LoadClass", "LoadClassWithOptions"];

/// How many source lines before the context line are read at most.
const MAX_LINES_BACK: usize = 100;

impl<'a> Missing<'a> {
    /// `name` is the missing file name; `None` unless it is a `.sty` or
    /// `.cls` file.
    pub(crate) fn new(name: &'a str) -> Option<Self> {
        let (stem, ext) = name.rsplit_once('.')?;
        let commands = if ext.eq_ignore_ascii_case("sty") {
            PACKAGE_COMMANDS
        } else if ext.eq_ignore_ascii_case("cls") {
            CLASS_COMMANDS
        } else {
            return None;
        };
        (!stem.is_empty()).then_some(Self { stem, commands })
    }
}

/// TeX's context line of the stop.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Context<'a> {
    /// `n` of `l.<n>`.
    pub line: u32,
    /// The text after `l.<n> `, as printed by TeX (possibly starting with
    /// `...` when TeX shortened it).
    pub before: &'a str,
    /// The second context line without its alignment indentation.
    pub after: &'a str,
}

/// The line of the command that requested `missing`, if it can be told
/// for certain. `source` is the file TeX was reading (whose line
/// `context.line` the context shows).
pub(crate) fn locate(
    missing: Missing<'_>,
    context: Context<'_>,
    source: Option<&[u8]>,
) -> Option<u32> {
    let n = context.line;
    let (truncated, visible) = match context.before.strip_prefix("...") {
        Some(rest) => (true, rest),
        None => (false, context.before),
    };
    // The looked-ahead token is `\par` from a blank line.
    let par = !truncated && visible == "^^M";
    let visible = if par {
        String::new()
    } else {
        decode_carets(visible)
    };

    let mut items = Vec::new();
    match source {
        Some(source) => {
            let lines = source_lines(source)?;
            let index = usize::try_from(n).ok()?.checked_sub(1)?;
            // TeX drops trailing spaces of input lines.
            let text = lines.get(index)?.trim_end_matches([' ', '\t']);
            let token_end = if par {
                text.is_empty().then_some(0)?
            } else if truncated {
                let mut found = text.match_indices(visible.as_str());
                let (at, _) = found.next()?;
                found.next().is_none().then_some(at + visible.len())?
            } else {
                text.starts_with(visible.as_str())
                    .then_some(visible.len())?
            };
            // The rest of the line must match too (`^^M` is the line end).
            let after = context.after.trim_end();
            let rest = &text[token_end..];
            let consistent = match after.strip_suffix("...") {
                Some(shown) => rest.starts_with(decode_carets(shown).as_str()),
                None => rest == decode_carets(after.strip_suffix("^^M").unwrap_or(after)),
            };
            if !consistent {
                return None;
            }
            let first = index.saturating_sub(MAX_LINES_BACK);
            for (i, line) in lines.iter().enumerate().take(index).skip(first) {
                push_source_line(&mut items, line, line_number(i)?);
            }
            if !par {
                let before = strip_last_token(&text[..token_end])?;
                items.extend(before.chars().map(|c| Item::Char(c, n)));
            }
        }
        // Without the source only line `n` is known, and only its visible
        // part; that is enough when the request is on line `n` too.
        None if par => return None,
        None => {
            let before = strip_last_token(&visible)?;
            items.extend(before.chars().map(|c| Item::Char(c, n)));
        }
    }
    request_line(&items, missing)
}

/// One character of the text TeX read before the looked-ahead token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Char(char, u32),
    /// A blank line (`\par`).
    Par,
}

/// The source split at `\n` (TeX's line numbering), or `None` for a bare
/// `\r` (which TeX may also take as a line end).
fn source_lines(source: &[u8]) -> Option<Vec<String>> {
    let text = String::from_utf8_lossy(source);
    let lines: Vec<String> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned())
        .collect();
    (!lines.iter().any(|l| l.contains('\r'))).then_some(lines)
}

fn line_number(index: usize) -> Option<u32> {
    u32::try_from(index).ok()?.checked_add(1)
}

/// Context text with TeX's `^^X` notation for unprintable characters
/// turned back into the characters. (TeX Live prints tabs as they are, but
/// other control characters in this notation.)
fn decode_carets(printed: &str) -> String {
    let mut out = String::with_capacity(printed.len());
    let mut rest = printed;
    while let Some(c) = rest.chars().next() {
        let bytes = rest.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'^'
            && bytes[1] == b'^'
            && (0x3f..=0x5f).contains(&bytes[2])
        {
            let code = if bytes[2] == b'?' {
                0x7f
            } else {
                bytes[2] - 0x40
            };
            out.push(char::from(code));
            rest = &rest[3..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Appends a source line before the context line: its text up to a comment,
/// then the space the line end becomes (none after a comment), or `\par`
/// for a blank line.
fn push_source_line(items: &mut Vec<Item>, text: &str, line: u32) {
    let code = match unescaped(text, '%') {
        Some(at) => &text[..at],
        None if text.trim_matches([' ', '\t']).is_empty() => {
            items.push(Item::Par);
            return;
        }
        None => text,
    };
    items.extend(code.chars().map(|c| Item::Char(c, line)));
    if code.len() == text.len() {
        items.push(Item::Char(' ', line));
    }
}

/// The byte index of the first `c` in `text` that is not escaped by `\`.
fn unescaped(text: &str, c: char) -> Option<usize> {
    let mut backslashes = 0;
    for (i, ch) in text.char_indices() {
        if ch == c && backslashes % 2 == 0 {
            return Some(i);
        }
        backslashes = if ch == '\\' { backslashes + 1 } else { 0 };
    }
    None
}

/// `text` without its last token: a control word (`\begin`), a control
/// symbol (`\{`) or a single character.
fn strip_last_token(text: &str) -> Option<&str> {
    let letters = text
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '@')
        .count();
    let before = &text[..text.len() - letters];
    if letters > 0 {
        if ends_with_escape(before) {
            return Some(&before[..before.len() - 1]);
        }
        // Without a backslash the token is a single letter.
        return (letters == 1).then_some(before);
    }
    let last = text.chars().next_back()?;
    let before = &text[..text.len() - last.len_utf8()];
    Some(if ends_with_escape(before) {
        &before[..before.len() - 1]
    } else {
        before
    })
}

/// Whether `text` ends with an escape character (an odd number of `\`).
fn ends_with_escape(text: &str) -> bool {
    text.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

/// Reads `\cmd[<options>]{<list>}` backwards from the end of `items` and
/// returns the line of `\cmd` if it loads `missing`.
fn request_line(items: &[Item], missing: Missing<'_>) -> Option<u32> {
    let mut scan = Backwards {
        items,
        end: items.len(),
    };
    scan.skip_spaces();
    if scan.next_char()? != '}' || scan.escaped() {
        return None;
    }
    let list = scan.group('{', '}')?;
    if !list
        .split(',')
        .any(|name| name.trim_matches([' ', '\t']) == missing.stem)
    {
        return None;
    }
    scan.skip_spaces();
    if scan.peek_char() == Some(']') {
        scan.next_char();
        if scan.escaped() {
            return None;
        }
        scan.group('[', ']')?;
        scan.skip_spaces();
    }
    let mut name = Vec::new();
    while let Some(c) = scan.peek_char().filter(char::is_ascii_alphabetic) {
        name.push(c);
        scan.next_char();
    }
    name.reverse();
    let name: String = name.into_iter().collect();
    let Some(Item::Char('\\', line)) = scan.next() else {
        return None;
    };
    (!scan.escaped() && missing.commands.contains(&name.as_str())).then_some(line)
}

struct Backwards<'a> {
    items: &'a [Item],
    /// Items before this index are not read yet.
    end: usize,
}

impl Backwards<'_> {
    fn next(&mut self) -> Option<Item> {
        self.end = self.end.checked_sub(1)?;
        Some(self.items[self.end])
    }

    fn peek_char(&self) -> Option<char> {
        match self.items.get(self.end.checked_sub(1)?)? {
            Item::Char(c, _) => Some(*c),
            Item::Par => None,
        }
    }

    fn next_char(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.end -= 1;
        Some(c)
    }

    /// Whether the character just read is escaped by `\`.
    fn escaped(&self) -> bool {
        self.items[..self.end]
            .iter()
            .rev()
            .take_while(|i| matches!(i, Item::Char('\\', _)))
            .count()
            % 2
            == 1
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek_char(), Some(' ' | '\t')) {
            self.end -= 1;
        }
    }

    /// Reads back to the `open` matching a `close` just read (braces nest,
    /// and an optional argument ends only outside braces) and returns the
    /// text in between. `\par` inside fails.
    fn group(&mut self, open: char, close: char) -> Option<String> {
        let mut depth = 0usize;
        let mut braces = 0usize;
        let mut text = Vec::new();
        loop {
            let c = self.next_char()?;
            if self.escaped() {
                text.push(c);
                continue;
            }
            if open == '[' && c == '}' {
                braces += 1;
            } else if open == '[' && c == '{' {
                braces = braces.checked_sub(1)?;
            } else if braces == 0 && c == close {
                depth += 1;
            } else if braces == 0 && c == open {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            text.push(c);
        }
        text.reverse();
        Some(text.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PKG: &str = "nopkg.sty";

    fn ctx<'a>(line: u32, before: &'a str, after: &'a str) -> Context<'a> {
        Context {
            line,
            before,
            after,
        }
    }

    fn at(before: &str, after: &str, line: u32, source: Option<&str>) -> Option<u32> {
        locate(
            Missing::new(PKG).unwrap(),
            ctx(line, before, after),
            source.map(str::as_bytes),
        )
    }

    #[test]
    fn missing_names() {
        assert!(Missing::new("x.sty").is_some());
        assert!(Missing::new("x.CLS").is_some());
        assert!(Missing::new("x.tex").is_none());
        assert!(Missing::new(".sty").is_none());
        assert!(Missing::new("sty").is_none());
    }

    #[test]
    fn request_on_the_previous_line() {
        let src = "\\documentclass{article}\n\\usepackage{nopkg}\n\\begin{document}\n";
        assert_eq!(at("\\begin", "{document}^^M", 3, Some(src)), Some(2));
        // Without the source, line 2 is unknown.
        assert_eq!(at("\\begin", "{document}^^M", 3, None), None);
        // A log that does not match the source (e.g. another file).
        assert_eq!(at("\\begin", "{abstract}^^M", 3, Some(src)), None);
        assert_eq!(at("\\end", "{document}^^M", 3, Some(src)), None);
        assert_eq!(at("\\begin", "{document}^^M", 4, Some(src)), None);
        assert_eq!(at("\\begin", "{document}^^M", 99, Some(src)), None);
    }

    #[test]
    fn next_line_loading_another_package() {
        // `l.3 \usepackage` is the look-ahead, not the request.
        let src = "\\documentclass{article}\n\\usepackage{nopkg}\n\\usepackage{amsmath}\n";
        assert_eq!(at("\\usepackage", "{amsmath}^^M", 3, Some(src)), Some(2));
        assert_eq!(at("\\usepackage", "{amsmath}^^M", 3, None), None);
    }

    #[test]
    fn comments_and_blank_lines() {
        let src = "\\usepackage{nopkg} % why\n% note\n   % more\n\n\\begin{document}\n";
        // The blank line 4 is the looked-ahead `\par`.
        assert_eq!(at("^^M", "", 4, Some(src)), Some(1));
        // A blank line between the request and the token is impossible.
        let src = "\\usepackage{nopkg}\n\n\\begin{document}\n";
        assert_eq!(at("\\begin", "{document}^^M", 3, Some(src)), None);
        // A commented-out request is not the one.
        let src = "\\usepackage{nopkg}\n%\\usepackage{nopkg}\n\\begin{document}\n";
        assert_eq!(at("\\begin", "{document}^^M", 3, Some(src)), Some(1));
        let src = "% \\usepackage{nopkg}\n\\begin{document}\n";
        assert_eq!(at("\\begin", "{document}^^M", 2, Some(src)), None);
    }

    #[test]
    fn options_and_lists_over_several_lines() {
        let src = "\\usepackage[\n  a={x,]y},% c\n  b\n]{amsmath,\n nopkg ,x}\n\\begin{document}\n";
        assert_eq!(at("\\begin", "{document}^^M", 6, Some(src)), Some(1));
        let src = "\\RequirePackage\n  [a]\n  {nopkg}\n\\foo\n";
        assert_eq!(at("\\foo", "^^M", 4, Some(src)), Some(1));
        // Another package with a similar name.
        let src = "\\usepackage{nopkgs}\n\\foo\n";
        assert_eq!(at("\\foo", "^^M", 2, Some(src)), None);
        // Not a loading command.
        let src = "\\newcommand{nopkg}\n\\foo\n";
        assert_eq!(at("\\foo", "^^M", 2, Some(src)), None);
        let src = "\\documentclass{nopkg}\n\\foo\n";
        assert_eq!(at("\\foo", "^^M", 2, Some(src)), None);
        // `\\usepackage` is a line break followed by text.
        let src = "\\\\usepackage{nopkg}\n\\foo\n";
        assert_eq!(at("\\foo", "^^M", 2, Some(src)), None);
    }

    #[test]
    fn request_on_the_context_line() {
        let before = "\\usepackage{amsmath}\\usepackage{nopkg}\\usepackage";
        assert_eq!(at(before, "{x}^^M", 7, None), Some(7));
        let src = format!("\n\n\n\n\n\n{before}{{x}}\n");
        assert_eq!(at(before, "{x}^^M", 7, Some(&src)), Some(7));
        // Shortened by TeX: the command is cut off without the source.
        let short = "...ckage{nopkg}\\usepackage";
        assert_eq!(at(short, "{x}^^M", 7, None), None);
        assert_eq!(at(short, "{x}^^M", 7, Some(&src)), Some(7));
        // A single-letter token and a control symbol.
        assert_eq!(at("\\usepackage{nopkg}x", "^^M", 1, None), Some(1));
        assert_eq!(at("\\usepackage{nopkg} \\{", "^^M", 1, None), Some(1));
        assert_eq!(at("\\usepackage{nopkg}xy", "^^M", 1, None), None);
    }

    #[test]
    fn tabs_and_indentation() {
        // TeX Live prints tabs as they are; `^^I` is accepted too.
        let src = "\t\\usepackage{nopkg}\t\\foo\n";
        for before in [
            "\t\\usepackage{nopkg}\t\\foo",
            "^^I\\usepackage{nopkg}^^I\\foo",
        ] {
            assert_eq!(at(before, "^^M", 1, Some(src)), Some(1));
            assert_eq!(at(before, "^^M", 1, None), Some(1));
        }
        let src = "\\usepackage{nopkg}\n   \t\\begin{document}\n";
        assert_eq!(at("   \t\\begin", "{document}^^M", 2, Some(src)), Some(1));
        // The indentation is part of the context.
        assert_eq!(at("\\begin", "{document}^^M", 2, Some(src)), None);
    }

    #[test]
    fn caret_notation() {
        assert_eq!(decode_carets("a^^Ib^^?^^@^^M"), "a\tb\u{7f}\0\r");
        assert_eq!(decode_carets("^^"), "^^");
        assert_eq!(decode_carets("^^a é"), "^^a é");
    }

    #[test]
    fn a_bare_carriage_return_is_rejected() {
        let src = "\\usepackage{nopkg}\r\\foo\n\\foo\n";
        assert_eq!(at("\\foo", "^^M", 2, Some(src)), None);
        let src = "\\usepackage{nopkg}\r\n\\foo\r\n";
        assert_eq!(at("\\foo", "^^M", 2, Some(src)), Some(1));
    }

    #[test]
    fn classes() {
        let missing = Missing::new("nocls.cls").unwrap();
        let src = "\\documentclass[a4paper]{nocls}\n\\begin{document}\n";
        let c = ctx(2, "\\begin", "{document}^^M");
        assert_eq!(locate(missing, c, Some(src.as_bytes())), Some(1));
        let src = "\\usepackage{nocls}\n\\begin{document}\n";
        assert_eq!(locate(missing, c, Some(src.as_bytes())), None);
    }
}
