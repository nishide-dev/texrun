//! Tests against real logs produced by TeX Live (see `fixtures/regenerate.sh`
//! for how they were generated and `fixtures/src/` for the documents).
//!
//! The assertions check structured results (kind, severity, file, line, key
//! parts of the message), not the full log text, so that they survive minor
//! differences between TeX distributions.

use std::path::Path;

use texrun_core::WorkspacePath;
use texrun_latex_log::{Diagnostic, DiagnosticKind as K, LogParser, Severity, parse_log};

/// The `max_print_line` the TeX Live engine sets (#5).
const ENGINE_MAX_PRINT_LINE: usize = 10_000;

fn read(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/logs")
        .join(format!("{name}.log"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Parses a fixture with the configuration used by the engine.
fn parse(name: &str) -> Vec<Diagnostic> {
    LogParser::new()
        .with_max_print_line(ENGINE_MAX_PRINT_LINE)
        .parse(&read(name))
        .diagnostics
}

/// Like [`parse`], with the fixture's documents in `src/<doc>/` as the
/// source files (as the engine passes the workspace).
fn parse_with_sources(name: &str, doc: &str) -> Vec<Diagnostic> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/src")
        .join(doc);
    let sources = |file: &WorkspacePath| std::fs::read(dir.join(file.as_path())).ok();
    LogParser::new()
        .with_max_print_line(ENGINE_MAX_PRINT_LINE)
        .parse_with_sources(&read(name), &sources)
        .diagnostics
}

/// A missing package / class: the file error and the stop that follows it.
#[track_caller]
fn assert_missing(d: &[Diagnostic], name: &str, at: (Option<&str>, Option<u32>)) {
    assert_eq!(kinds(d), [K::MissingFile, K::EmergencyStop]);
    assert_at(&d[0], Severity::Error, K::MissingFile, at);
    assert!(
        d[0].message.contains(&format!("File `{name}' not found")),
        "{}",
        d[0].message
    );
    // The stop is a consequence of the error, and its context line is where
    // LaTeX looked ahead, not the request: no line.
    assert_at(&d[1], Severity::Info, K::EmergencyStop, (at.0, None));
    assert_eq!(d[1].message, "Emergency stop.");
}

fn kinds(diagnostics: &[Diagnostic]) -> Vec<K> {
    diagnostics.iter().map(|d| d.kind).collect()
}

fn file(d: &Diagnostic) -> Option<&str> {
    d.file.as_ref().map(WorkspacePath::as_str)
}

#[track_caller]
fn assert_at(d: &Diagnostic, severity: Severity, kind: K, at: (Option<&str>, Option<u32>)) {
    assert_eq!(
        (d.severity, d.kind, file(d), d.line),
        (severity, kind, at.0, at.1),
        "{d:#?}"
    );
    let excerpt = d.raw_excerpt.as_deref().unwrap_or_default();
    assert!(!excerpt.is_empty(), "{d:#?}");
}

#[test]
fn undefined_control_sequence() {
    let d = parse("undefined-control-sequence");
    assert_eq!(kinds(&d), [K::UndefinedControlSequence, K::EmergencyStop]);
    assert_at(
        &d[0],
        Severity::Error,
        K::UndefinedControlSequence,
        (Some("main.tex"), Some(5)),
    );
    assert_eq!(d[0].message, "Undefined control sequence \\foo");
    // The excerpt keeps the error line and TeX's context lines.
    let excerpt = d[0].raw_excerpt.as_deref().unwrap();
    assert!(excerpt.starts_with("./main.tex:5: Undefined control sequence."));
    assert!(excerpt.contains("l.5 \\foo"));
    assert!(excerpt.contains("{bar}"));
    // `-halt-on-error` then stops TeX: a consequence, not a second error.
    assert_at(
        &d[1],
        Severity::Info,
        K::EmergencyStop,
        (Some("main.tex"), Some(5)),
    );
    assert!(d[1].message.starts_with("Fatal error occurred"));
}

#[test]
fn classic_error_format_without_file_line_error() {
    let d = parse("traditional");
    assert_eq!(kinds(&d), [K::UndefinedControlSequence, K::EmergencyStop]);
    // File from the file stack, line from `l.5`.
    assert_at(
        &d[0],
        Severity::Error,
        K::UndefinedControlSequence,
        (Some("main.tex"), Some(5)),
    );
    assert_eq!(d[0].message, "Undefined control sequence \\foo");
    assert!(
        d[0].raw_excerpt
            .as_deref()
            .unwrap()
            .starts_with("! Undefined")
    );
}

#[test]
fn missing_package() {
    // `\usepackage{...}` is on line 3. TeX reports `l.4 \begin`, where LaTeX
    // looked ahead for an optional `[<date>]`; from the log alone line 3 is
    // unknown, so there is no line (the name is in the message).
    let pkg = "texrunnonexistentpackage.sty";
    assert_missing(&parse("missing-package"), pkg, (Some("main.tex"), None));
    // With the source, the `\usepackage` before `\begin` is found.
    assert_missing(
        &parse_with_sources("missing-package", "missing-package"),
        pkg,
        (Some("main.tex"), Some(3)),
    );
}

/// `l.3 \usepackage` shows the next `\usepackage{amsmath}`, not the request
/// on line 2.
#[test]
fn missing_package_before_another_usepackage() {
    let pkg = "texrunnonexistentpackage.sty";
    let doc = "missing-package-before-usepackage";
    assert_missing(&parse(doc), pkg, (Some("main.tex"), None));
    assert_missing(
        &parse_with_sources(doc, doc),
        pkg,
        (Some("main.tex"), Some(2)),
    );
    // The same in the classic format (the file comes from the file stack).
    assert_missing(
        &parse("missing-package-traditional"),
        pkg,
        (Some("main.tex"), None),
    );
    assert_missing(
        &parse_with_sources("missing-package-traditional", doc),
        pkg,
        (Some("main.tex"), Some(2)),
    );
}

/// `\usepackage[` on line 3, options with comments, `]{...}` on line 6, a
/// comment line and a blank line (`l.8 ^^M`: the look-ahead found `\par`).
#[test]
fn missing_package_with_options_over_several_lines() {
    let doc = "missing-package-options";
    let pkg = "texrunnonexistentpackage.sty";
    assert_missing(&parse(doc), pkg, (Some("main.tex"), None));
    assert_missing(
        &parse_with_sources(doc, doc),
        pkg,
        (Some("main.tex"), Some(3)),
    );
}

/// A package list over lines 2-3, then an indented comment line.
#[test]
fn missing_package_in_a_list() {
    let doc = "missing-package-list";
    let pkg = "texrunnonexistentpackage.sty";
    assert_missing(&parse(doc), pkg, (Some("main.tex"), None));
    assert_missing(
        &parse_with_sources(doc, doc),
        pkg,
        (Some("main.tex"), Some(2)),
    );
}

/// Three `\usepackage` on line 2: the context line is line 2 itself
/// (shortened by TeX to `l.2 ...ackage{texrunnonexistentpackage}\usepackage`).
#[test]
fn missing_package_among_others_on_one_line() {
    let doc = "missing-package-same-line";
    let pkg = "texrunnonexistentpackage.sty";
    // The shortened context cuts `\usepackage` off: unknown from the log.
    assert_missing(&parse(doc), pkg, (Some("main.tex"), None));
    assert_missing(
        &parse_with_sources(doc, doc),
        pkg,
        (Some("main.tex"), Some(2)),
    );
}

/// Indented lines: TeX prints the indentation (a tab as it is) in the
/// context line `l.3  <TAB>\begin`.
#[test]
fn missing_package_on_indented_lines() {
    let doc = "missing-package-indented";
    let pkg = "texrunnonexistentpackage.sty";
    assert_missing(&parse(doc), pkg, (Some("main.tex"), None));
    assert_missing(
        &parse_with_sources(doc, doc),
        pkg,
        (Some("main.tex"), Some(2)),
    );
}

/// `\RequirePackage` on line 4 of a package in the project.
#[test]
fn missing_package_required_by_a_local_package() {
    let doc = "missing-package-in-sty";
    let pkg = "texrunnonexistentpackage.sty";
    assert_missing(&parse(doc), pkg, (Some("texrunlocal.sty"), None));
    assert_missing(
        &parse_with_sources(doc, doc),
        pkg,
        (Some("texrunlocal.sty"), Some(4)),
    );
}

/// Sources that do not match the log are not used.
#[test]
fn mismatching_sources_are_ignored() {
    let pkg = "texrunnonexistentpackage.sty";
    // Another document (the `\usepackage` is on line 2 there, line 3 in the
    // log's document).
    assert_missing(
        &parse_with_sources("missing-package", "missing-package-before-usepackage"),
        pkg,
        (Some("main.tex"), None),
    );
    let log = read("missing-package");
    for source in [
        &b""[..],
        b"\\documentclass{article}\n\\usepackage{amsmath}\n% \\usepackage{texrunnonexistentpackage}\n\\begin{document}\n",
        b"\\documentclass{article}\n\\usepackage{amsmath}\n\\usepackage{texrunnonexistentpackage}\n\\begin{abstract}\n",
        b"\\documentclass{article}\r\\usepackage{amsmath}\n\\usepackage{texrunnonexistentpackage}\n\\begin{document}\n",
    ] {
        let sources = |_: &WorkspacePath| Some(source.to_vec());
        let d = LogParser::new()
            .with_max_print_line(ENGINE_MAX_PRINT_LINE)
            .parse_with_sources(&log, &sources)
            .diagnostics;
        assert_missing(&d, pkg, (Some("main.tex"), None));
    }
}

#[test]
fn missing_input_file() {
    let d = parse("missing-input");
    assert_eq!(kinds(&d), [K::MissingFile, K::EmergencyStop]);
    // For `\input` the position is where the file was requested.
    assert_at(
        &d[0],
        Severity::Error,
        K::MissingFile,
        (Some("main.tex"), Some(4)),
    );
    assert!(d[0].message.contains("chapters/nothere.tex"));
    assert_at(
        &d[1],
        Severity::Info,
        K::EmergencyStop,
        (Some("main.tex"), Some(4)),
    );
}

#[test]
fn latex_error() {
    let d = parse("latex-error");
    assert_eq!(kinds(&d), [K::LatexError, K::EmergencyStop]);
    assert_at(
        &d[0],
        Severity::Error,
        K::LatexError,
        (Some("main.tex"), Some(5)),
    );
    assert_eq!(
        d[0].message,
        "LaTeX Error: \\begin{itemize} on input line 3 ended by \\end{enumerate}."
    );
    let excerpt = d[0].raw_excerpt.as_deref().unwrap();
    assert!(excerpt.contains("l.5 \\end{enumerate}"), "{excerpt}");
}

#[test]
fn package_error_and_multi_line_warning() {
    let d = parse("package-error");
    assert_eq!(kinds(&d), [K::Other, K::LatexError, K::EmergencyStop]);
    // Continuation lines are joined into the message.
    assert_eq!(
        d[0].message,
        "LaTeX Warning: Unused global option(s): [nosuchoption]."
    );
    assert_eq!(d[0].severity, Severity::Warning);
    assert_at(
        &d[1],
        Severity::Error,
        K::LatexError,
        (Some("main.tex"), Some(6)),
    );
    assert!(d[1].message.starts_with("Package amsmath Error:"));
}

#[test]
fn multi_file_attribution() {
    let d = parse("multi-file");
    assert_eq!(
        kinds(&d),
        [
            K::UndefinedReference,
            K::OverfullBox,
            K::UndefinedControlSequence,
            K::EmergencyStop
        ]
    );
    // Warnings carry no file name; it comes from the `(./chapters/intro.tex`
    // file stack.
    assert_at(
        &d[0],
        Severity::Warning,
        K::UndefinedReference,
        (Some("chapters/intro.tex"), Some(3)),
    );
    assert!(d[0].message.contains("sec:missing"));
    assert_at(
        &d[1],
        Severity::Warning,
        K::OverfullBox,
        (Some("chapters/intro.tex"), Some(4)),
    );
    // After `)` closes intro.tex, body.tex is opened.
    assert_at(
        &d[2],
        Severity::Error,
        K::UndefinedControlSequence,
        (Some("chapters/body.tex"), Some(4)),
    );
    assert_eq!(d[2].message, "Undefined control sequence \\undefinedmacro");
}

#[test]
fn warnings_of_a_successful_compile() {
    let d = parse("warnings");
    assert_eq!(
        kinds(&d),
        [
            K::UndefinedReference,
            K::UndefinedCitation,
            K::OverfullBox,
            K::UnderfullBox,
            K::UnderfullBox
        ]
    );
    assert!(d.iter().all(|d| d.severity == Severity::Warning));
    assert!(d.iter().all(|d| file(d) == Some("main.tex")));
    let lines: Vec<_> = d.iter().map(|d| d.line).collect();
    assert_eq!(lines, [Some(4), Some(5), Some(7), Some(9), Some(10)]);
    assert!(d[0].message.contains("sec:nowhere"));
    assert!(d[1].message.contains("knuth1984"));
    assert!(d[2].message.contains("too wide"));
    // Box contents are part of the excerpt.
    assert!(
        d[2].raw_excerpt
            .as_deref()
            .unwrap()
            .contains("This line is much too wide")
    );
    // `There were undefined references.` repeats the warnings above.
}

#[test]
fn rerun_request() {
    let d = parse("rerun");
    assert_eq!(kinds(&d), [K::UndefinedReference, K::RerunRequired]);
    assert_eq!(d[1].severity, Severity::Info);
    assert!(d[1].message.contains("Rerun to get cross-references right"));
}

/// TeX's default 79-byte wrapping without telling the parser: file names are
/// dropped instead of guessed from fragments, the rest still works.
#[test]
fn wrapped_log_degrades_gracefully() {
    let d = parse_log(&read("wrapped"));
    assert_eq!(
        kinds(&d),
        [
            K::OverfullBox,
            K::UndefinedControlSequence,
            K::EmergencyStop
        ]
    );
    assert!(d.iter().all(|d| d.file.is_none()), "{d:#?}");
    let lines: Vec<_> = d.iter().map(|d| d.line).collect();
    assert_eq!(lines, [Some(2), Some(4), Some(4)]);
}

/// The same log, telling the parser it was wrapped at 79 bytes.
#[test]
fn wrapped_log_with_known_width_is_rejoined() {
    let d = LogParser::new()
        .with_max_print_line(79)
        .parse(&read("wrapped"))
        .diagnostics;
    let expected = "some-rather-long-directory-name/another-nested-directory-level/\
                    a-chapter-file-with-a-long-name.tex";
    assert_eq!(
        kinds(&d),
        [
            K::OverfullBox,
            K::UndefinedControlSequence,
            K::EmergencyStop
        ]
    );
    assert!(d.iter().all(|d| file(d) == Some(expected)), "{d:#?}");
    // TeX shortened the context line (`l.4 ...`), so the name of the
    // undefined control sequence is unknown.
    assert_eq!(d[1].message, "Undefined control sequence.");
    assert_eq!(
        d[2].message,
        "Fatal error occurred, no output PDF file produced!"
    );
}

/// Every configuration on every fixture: files are never outside the
/// workspace (installed packages under texmf-dist are opened in all logs).
#[test]
fn installed_files_are_never_attributed() {
    for name in [
        "undefined-control-sequence",
        "missing-package",
        "missing-input",
        "package-error",
        "latex-error",
        "multi-file",
        "warnings",
        "traditional",
        "wrapped",
        "rerun",
        "missing-class",
        "unusual-names",
        "unusual-names-traditional",
        "unbalanced-parens",
        "missing-package-before-usepackage",
        "missing-package-options",
        "missing-package-list",
        "missing-package-same-line",
        "missing-package-in-sty",
        "missing-package-traditional",
        "missing-package-indented",
    ] {
        let log = read(name);
        for parser in [
            LogParser::new(),
            LogParser::new().with_max_print_line(79),
            LogParser::new().with_max_print_line(ENGINE_MAX_PRINT_LINE),
        ] {
            for d in parser.parse(&log).diagnostics {
                let f = file(&d).unwrap_or_default();
                assert!(
                    !f.contains("texmf") && !f.contains("usr/"),
                    "{name}: {d:#?}"
                );
                assert!(!d.message.is_empty(), "{name}: {d:#?}");
            }
        }
    }
}

#[test]
fn missing_class() {
    // `l.2 \begin`: the class was requested on line 1.
    let cls = "texrunnonexistentclass.cls";
    assert_missing(&parse("missing-class"), cls, (Some("main.tex"), None));
    assert_missing(
        &parse_with_sources("missing-class", "missing-class"),
        cls,
        (Some("main.tex"), Some(1)),
    );
}

/// pdfTeX prints `(./my dir/chap one.tex` and `(./a(1).tex` unquoted, so the
/// end of such a name cannot be told from the following output. Diagnostics
/// inside those files get no file rather than a wrong one (`my`, `a`).
#[test]
fn file_names_with_spaces_and_parentheses() {
    let d = parse("unusual-names");
    let at: Vec<_> = d.iter().map(|d| (d.kind, file(d), d.line)).collect();
    assert_eq!(
        at,
        [
            (K::UndefinedReference, None, Some(2)),
            (K::OverfullBox, None, Some(3)),
            // `)` closed `my dir/chap one.tex`: back in main.tex.
            (K::UndefinedReference, Some("main.tex"), Some(4)),
            (K::UndefinedReference, None, Some(1)),
            (K::OverfullBox, None, Some(2)),
            // The file-line-error prefix names the file exactly.
            (K::UndefinedControlSequence, Some("a(1).tex"), Some(3)),
            (K::EmergencyStop, Some("a(1).tex"), Some(3)),
        ]
    );
    assert_eq!(
        d[5].message,
        "Undefined control sequence \\undefinedinparen"
    );
}

#[test]
fn file_names_with_parentheses_in_classic_format() {
    let d = parse("unusual-names-traditional");
    assert_eq!(
        kinds(&d),
        [
            K::UndefinedReference,
            K::OverfullBox,
            K::UndefinedReference,
            K::UndefinedReference,
            K::OverfullBox,
            K::UndefinedControlSequence,
            K::EmergencyStop
        ]
    );
    assert_eq!(file(&d[2]), Some("main.tex"));
    // No file-line-error prefix: the error is not attributed to `a`.
    assert_at(
        &d[5],
        Severity::Error,
        K::UndefinedControlSequence,
        (None, Some(3)),
    );
}

/// `\typeout{Note (unbalanced open}` in chapters/c.tex must not keep c.tex
/// open; `\typeout{A stray ) close}` in main.tex degrades to no file.
#[test]
fn unbalanced_parentheses_in_document_output() {
    let d = parse("unbalanced-parens");
    let at: Vec<_> = d.iter().map(|d| (d.kind, file(d), d.line)).collect();
    assert_eq!(
        at,
        [
            (K::UndefinedReference, Some("main.tex"), Some(4)),
            (K::OverfullBox, Some("main.tex"), Some(5)),
            (K::UndefinedReference, None, Some(8)),
        ]
    );
}

#[test]
fn max_diagnostics_applies_to_real_logs() {
    let parsed = LogParser::new()
        .with_max_print_line(ENGINE_MAX_PRINT_LINE)
        .with_max_diagnostics(2)
        .parse(&read("multi-file"));
    // The error is kept over the earlier warnings, then the first of the
    // rest (the stop after the error is info).
    assert_eq!(parsed.omitted, 2);
    assert_eq!(
        kinds(&parsed.diagnostics),
        [K::UndefinedReference, K::UndefinedControlSequence, K::Other]
    );
    assert_eq!(parsed.diagnostics[2].severity, Severity::Info);
}

/// pdfTeX's own fatal error (`!pdfTeX error:`, #66): a font it cannot load
/// while writing the PDF, after the input was read. No position is printed,
/// so none is reported.
#[test]
fn pdftex_error_for_a_missing_font() {
    let d = parse("pdftex-missing-font");
    assert_eq!(kinds(&d), [K::Other, K::EmergencyStop]);
    assert_at(&d[0], Severity::Error, K::Other, (None, None));
    assert_eq!(
        d[0].message,
        "pdfTeX error: pdflatex (file cmntt10): Font cmntt10 at 600 not found"
    );
    let excerpt = d[0].raw_excerpt.as_deref().unwrap();
    assert!(excerpt.ends_with(" ==> Fatal error occurred, no output PDF file produced!"));
    // The summary that follows is a consequence (#40).
    assert_at(&d[1], Severity::Info, K::EmergencyStop, (None, None));
    assert_eq!(
        d[1].message,
        "Fatal error occurred, no output PDF file produced!"
    );
    // The default configuration finds it too.
    assert_eq!(
        parse_log(&read("pdftex-missing-font"))[0].message,
        d[0].message
    );
}

/// The same while the input is read: the file being read is not reported
/// either, since pdfTeX does not say where the image was requested.
#[test]
fn pdftex_error_for_a_missing_image() {
    let d = parse("pdftex-missing-image");
    assert_eq!(kinds(&d), [K::Other, K::EmergencyStop]);
    assert_at(&d[0], Severity::Error, K::Other, (None, None));
    assert_eq!(
        d[0].message,
        "pdfTeX error: pdflatex: cannot find image file nosuch.png"
    );
    assert_eq!(d[1].severity, Severity::Info);
}

/// A pdfTeX error reported through TeX's error routine has the usual form.
#[test]
fn pdftex_error_with_a_position() {
    let d = parse("pdftex-error-ext");
    assert_eq!(kinds(&d), [K::Other, K::EmergencyStop]);
    assert_at(
        &d[0],
        Severity::Error,
        K::Other,
        (Some("main.tex"), Some(5)),
    );
    assert!(
        d[0].message
            .starts_with("pdfTeX error (ext4): pdf_link_stack empty"),
        "{}",
        d[0].message
    );
    assert_eq!(d[1].severity, Severity::Info);
}

/// Other fatal engine errors are TeX errors: an error, then the stop as info.
#[test]
fn fatal_engine_errors() {
    for (name, message, line) in [
        (
            "capacity-exceeded",
            "TeX capacity exceeded, sorry [input stack size=10000].",
            4,
        ),
        ("cannot-write", "I can't write on file `/tmp/out.txt'.", 3),
        ("interruption", "Interruption.", 4),
    ] {
        let d = parse(name);
        assert_eq!(kinds(&d), [K::Other, K::EmergencyStop], "{name}");
        assert_at(
            &d[0],
            Severity::Error,
            K::Other,
            (Some("main.tex"), Some(line)),
        );
        assert_eq!(d[0].message, message, "{name}");
        assert_eq!(d[1].severity, Severity::Info, "{name}");
    }
}
