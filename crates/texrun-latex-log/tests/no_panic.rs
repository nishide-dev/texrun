//! Property tests: the parser never panics and its output stays bounded,
//! whatever bytes it is given.

use std::path::Path;

use proptest::prelude::*;
use texrun_core::{WorkspacePath, WorkspaceRoot};
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
        LogParser::new().with_max_diagnostics(3),
    ]
}

fn check(log: &[u8]) {
    for parser in parsers() {
        let parsed = parser.parse(log);
        // `DEFAULT_MAX_DIAGNOSTICS` or 3, plus the "omitted" notice.
        assert!(parsed.diagnostics.len() <= texrun_latex_log::DEFAULT_MAX_DIAGNOSTICS + 1);
        for d in parsed.diagnostics {
            check_diagnostic(&d);
        }
    }
}

fn check_diagnostic(d: &Diagnostic) {
    assert!(d.message.len() <= 2048, "{d:?}");
    assert!(!d.message.chars().any(char::is_control), "{d:?}");
    // Only the "omitted" notice has no excerpt.
    assert!(
        d.raw_excerpt
            .as_ref()
            .map_or(d.message.contains("omitted"), |e| e.len() <= 4096),
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

/// Fragments of LaTeX source around a package request.
const SOURCE_FRAGMENTS: &[&str] = &[
    "",
    " ",
    "\t",
    "\n",
    "\r",
    "\r\n",
    "%",
    "\\",
    "{",
    "}",
    "[",
    "]",
    ",",
    "^^M",
    "\\usepackage",
    "\\RequirePackage",
    "\\documentclass",
    "{texrunnonexistentpackage}",
    "texrunnonexistentpackage",
    "\\begin{document}",
    "\\begin",
    "{amsmath}",
    "é",
];

fn source_text() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::sample::select(SOURCE_FRAGMENTS), 0..60)
        .prop_map(|pieces| pieces.concat().into_bytes())
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

    /// Missing-package logs with random source files (and random edits of
    /// the log): locating the request never panics, and a line it reports
    /// exists in the source.
    #[test]
    fn locating_requests_never_panics(
        name in prop::sample::select(&[
            "missing-package",
            "missing-package-options",
            "missing-package-same-line",
            "missing-class",
        ][..]),
        source in source_text(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..8),
    ) {
        let mut log = fixture(name);
        for (at, byte) in edits {
            let i = at.index(log.len());
            log[i] = byte;
        }
        let sources = |_: &WorkspacePath| Some(source.clone());
        let lines = source.split(|&b| b == b'\n').count();
        for parser in parsers() {
            for d in parser.parse_with_sources(&log, &sources).diagnostics {
                check_diagnostic(&d);
                // (An edit may turn the name into a non-package file, whose
                // line is the context line.)
                let request = [".sty'", ".cls'"].iter().any(|ext| d.message.contains(ext));
                if d.kind == texrun_latex_log::DiagnosticKind::MissingFile
                    && request
                    && let Some(line) = d.line
                {
                    prop_assert!(usize::try_from(line).unwrap() <= lines, "{d:?}");
                }
            }
        }
    }

    #[test]
    fn blg_never_panics_on_bytes(blg in prop::collection::vec(any::<u8>(), 0..2048)) {
        check_blg(&blg);
    }

    /// BibTeX logs assembled from real fragments.
    #[test]
    fn blg_never_panics_on_fragments(
        parts in prop::collection::vec(
            (0..BLG_FRAGMENTS.len(), prop::sample::select(&["\n", "\r\n", ""][..])),
            0..64,
        ),
    ) {
        let mut blg = String::new();
        for (part, sep) in parts {
            blg.push_str(BLG_FRAGMENTS[part]);
            blg.push_str(sep);
        }
        check_blg(blg.as_bytes());
    }
}

/// Fragments of BibTeX log syntax.
const BLG_FRAGMENTS: &[&str] = &[
    "",
    " : ",
    " : @book{x",
    "Database file #1: refs.bib",
    "The style file: plain.bst",
    "Repeated entry---line 3 of file refs.bib",
    "---line 4 of file main.aux",
    "---line 0 of file refs.bib",
    "--line 5 of file refs.bib",
    "I couldn't open database file x.bib",
    "I found no style file---while reading file main.aux",
    "(Error may have been on previous line)",
    "I'm skipping whatever remains of this entry",
    "Warning--string name \"x\" is undefined",
    "Warning--I didn't find a database entry for \"k\"",
    "Sorry---you've exceeded BibTeX's hash size",
    "(There were 3 error messages)",
    "(There was 1 error message)",
    "(That was a fatal error)",
    "\u{1b}[31m",
];

fn check_blg(blg: &[u8]) {
    let files = |name: &WorkspacePath| Some(name.clone());
    for parser in [
        texrun_latex_log::BlgParser::new(),
        texrun_latex_log::BlgParser::new().with_max_diagnostics(2),
    ] {
        let parsed = parser.parse_with_files(blg, &files);
        // The limit, the summary and the "omitted" notice.
        assert!(parsed.diagnostics.len() <= texrun_latex_log::DEFAULT_MAX_DIAGNOSTICS + 2);
        for d in &parsed.diagnostics {
            // The summary has no excerpt without BibTeX's summary line.
            if d.kind != texrun_latex_log::DiagnosticKind::BibtexFailed {
                check_diagnostic(d);
            }
            assert!(d.message.len() <= 2048, "{d:?}");
            assert!(!d.message.chars().any(char::is_control), "{d:?}");
            assert_ne!(d.line, Some(0), "{d:?}");
            assert!(d.line.is_none() || d.file.is_some(), "{d:?}");
        }
    }
}
