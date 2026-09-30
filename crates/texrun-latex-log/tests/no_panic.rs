//! Property tests: the parser never panics and its output stays bounded,
//! whatever bytes it is given.

use std::path::Path;

use proptest::prelude::*;
use texrun_core::WorkspaceRoot;
use texrun_latex_log::{Diagnostic, LogParser};

fn parsers() -> Vec<LogParser> {
    let root = WorkspaceRoot::new("/work").unwrap();
    vec![
        LogParser::new(),
        LogParser::new().with_max_print_line(1),
        LogParser::new().with_max_print_line(79),
        LogParser::new()
            .with_max_print_line(10_000)
            .with_workspace_root(&root),
    ]
}

fn check(log: &[u8]) {
    for parser in parsers() {
        for d in parser.parse(log) {
            check_diagnostic(&d);
        }
    }
}

fn check_diagnostic(d: &Diagnostic) {
    assert!(d.message.len() <= 2048, "{d:?}");
    assert!(!d.message.chars().any(char::is_control), "{d:?}");
    assert!(
        d.raw_excerpt.as_ref().is_some_and(|e| e.len() <= 4096),
        "{d:?}"
    );
    assert_ne!(d.line, Some(0), "{d:?}");
    if let Some(file) = &d.file {
        assert!(!file.as_str().starts_with('/'), "{d:?}");
    }
}

/// Fragments of real log syntax, so that random logs reach deep branches.
const FRAGMENTS: &[&str] = &[
    "",
    " ",
    "(",
    ")",
    "(./main.tex",
    "(./chapters/a.tex",
    "(/usr/share/texlive/texmf-dist/tex/latex/base/article.cls",
    "(/work/sub/b.tex",
    "(\"./my file.tex\"",
    "(\"",
    "(Font)",
    "(babel)   continued",
    "! ",
    "!",
    "! Undefined control sequence.",
    "! LaTeX Error: File `x.sty' not found.",
    "! Emergency stop.",
    "!  ==> Fatal error occurred, no output PDF file produced!",
    "./main.tex:5: Undefined control sequence.",
    "./main.tex:4294967296: x",
    "./main.tex:0: x",
    "./a.tex:1: Package babel Error: bad",
    "./a.tex:1:  ==> Fatal error occurred",
    "l.5 \\foo",
    "l.",
    "l.99999999999 x",
    "        {bar}",
    "\\",
    "\\\\",
    "<recently read> \\foo",
    "LaTeX Warning: Reference `a' on page 1 undefined on input line 3.",
    "LaTeX Warning: Citation `k' undefined on input line ",
    "LaTeX Warning: There were undefined references.",
    "LaTeX Warning: Label(s) may have changed. Rerun",
    "LaTeX Font Warning: x",
    "Package ",
    "Package x Warning:",
    "Class  Error: ",
    "pdfTeX warning (ext4): x",
    "Overfull \\hbox (1pt too wide) in paragraph at lines 3--",
    "Underfull \\vbox (badness 10000) detected at line ",
    "Overfull \\",
    " []",
    "on input line 7.",
    "é",
    "\u{1b}[31m",
    "\r",
];

fn fragment_log() -> impl Strategy<Value = Vec<u8>> {
    let piece = (
        prop::sample::select(FRAGMENTS),
        prop::sample::select(&["", "\n", " ", "\n\n"][..]),
    );
    prop::collection::vec(piece, 0..80).prop_map(|pieces| {
        let mut out = Vec::new();
        for (fragment, sep) in pieces {
            out.extend_from_slice(fragment.as_bytes());
            out.extend_from_slice(sep.as_bytes());
        }
        out
    })
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/logs")
        .join(format!("{name}.log"));
    std::fs::read(path).unwrap()
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(log in prop::collection::vec(any::<u8>(), 0..4096)) {
        check(&log);
    }

    #[test]
    fn log_like_text_never_panics(log in fragment_log()) {
        check(&log);
    }

    /// Real logs with random byte edits and truncation.
    #[test]
    fn mutated_real_logs_never_panic(
        name in prop::sample::select(&["multi-file", "missing-package", "wrapped", "warnings"][..]),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..32),
        cut in any::<prop::sample::Index>(),
    ) {
        let mut log = fixture(name);
        for (at, byte) in edits {
            let i = at.index(log.len());
            log[i] = byte;
        }
        log.truncate(cut.index(log.len() + 1));
        check(&log);
    }
}
