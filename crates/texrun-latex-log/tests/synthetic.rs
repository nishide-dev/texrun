//! Hand-written log excerpts for patterns that the real-log fixtures do not
//! cover (help text, package messages, absolute paths, malformed input).

use texrun_core::{WorkspacePath, WorkspaceRoot};
use texrun_latex_log::{Diagnostic, DiagnosticKind as K, LogParser, Severity, parse_log};

fn parse(log: &str) -> Vec<Diagnostic> {
    parse_log(log.as_bytes())
}

fn at(d: &Diagnostic) -> (K, Option<&str>, Option<u32>) {
    (d.kind, d.file.as_ref().map(WorkspacePath::as_str), d.line)
}

#[test]
fn package_error_continuation_and_help_text() {
    let d = parse(concat!(
        "(./main.tex\n",
        "./main.tex:3: Package babel Error: Unknown option `x'.\n",
        "(babel)                Either you misspelled it\n",
        "(babel)                or the file x.ldf was not found.\n",
        "\n",
        "See the babel package documentation for explanation.\n",
        "Type  H <return>  for immediate help.\n",
        " ...                                              \n",
        "                                                  \n",
        "l.3 \\begin{document}\n",
        "                    \n",
        "Valid options are: shorthands=, KeepShorthandsActive,\n",
        "activeacute, a closing paren) in the help text\n",
        "\n",
        "LaTeX Warning: Reference `a' on page 1 undefined on input line 9.\n",
        "\n",
    ));
    assert_eq!(
        d.iter().map(at).collect::<Vec<_>>(),
        [
            (K::LatexError, Some("main.tex"), Some(3)),
            // The `)` in the help text did not close main.tex.
            (K::UndefinedReference, Some("main.tex"), Some(9)),
        ]
    );
    assert_eq!(
        d[0].message,
        "Package babel Error: Unknown option `x'. Either you misspelled it or the file \
         x.ldf was not found."
    );
    let excerpt = d[0].raw_excerpt.as_deref().unwrap();
    assert!(excerpt.ends_with("l.3 \\begin{document}\n                    "));
    assert!(!excerpt.contains("Valid options"));
}

#[test]
fn absolute_paths_inside_and_outside_the_workspace() {
    let log = concat!(
        "(/work/space/main.tex\n",
        "(/usr/share/texlive/texmf-dist/tex/latex/foo/foo.sty\n",
        "/usr/share/texlive/texmf-dist/tex/latex/foo/foo.sty:12: Undefined control sequence.\n",
        "l.12 \\bar\n",
        "          \n",
        "\n",
        ")\n",
        "(/work/space/sub/ch.tex\n",
        "Overfull \\hbox (1.0pt too wide) in paragraph at lines 3--5\n",
        "[]\\OT1/cmr/m/n/10 text with an (unbalanced paren\n",
        " []\n",
        "\n",
        ")\n",
    );
    let root = WorkspaceRoot::new("/work/space").unwrap();
    let d = LogParser::new()
        .with_workspace_root(&root)
        .parse(log.as_bytes());
    assert_eq!(
        d.iter().map(at).collect::<Vec<_>>(),
        [
            // Installed package: line kept, file dropped.
            (K::UndefinedControlSequence, None, Some(12)),
            (K::OverfullBox, Some("sub/ch.tex"), Some(3)),
        ]
    );
    assert_eq!(d[0].message, "Undefined control sequence \\bar");

    // Without the root, absolute paths are never attributed.
    let d = parse(log);
    assert_eq!(at(&d[1]), (K::OverfullBox, None, Some(3)));
}

#[test]
fn missing_class_in_classic_format() {
    let d = parse(concat!(
        "**main.tex\n",
        "(./main.tex\n",
        "! LaTeX Error: File `nosuchclass.cls' not found.\n",
        "\n",
        "Type X to quit or <RETURN> to proceed,\n",
        "or enter new name. (Default extension: cls)\n",
        "\n",
        "Enter file name: \n",
        "! Emergency stop.\n",
        "<read *> \n",
        "         \n",
        "l.1 \\documentclass\n",
        "                  {nosuchclass}^^M\n",
        "*** (cannot \\read from terminal in nonstop modes)\n",
    ));
    assert_eq!(
        d.iter().map(at).collect::<Vec<_>>(),
        [
            (K::MissingFile, Some("main.tex"), Some(1)),
            (K::EmergencyStop, Some("main.tex"), Some(1)),
        ]
    );
    assert!(d.iter().all(|d| d.severity == Severity::Error));
}

#[test]
fn tex_primitive_errors_are_other_errors() {
    let d = parse(
        "(./main.tex\n./main.tex:8: Missing $ inserted.\n<inserted text> \n                $\nl.8 a^\n       2\n",
    );
    assert_eq!(at(&d[0]), (K::Other, Some("main.tex"), Some(8)));
    assert_eq!(d[0].severity, Severity::Error);
    assert_eq!(d[0].message, "Missing $ inserted.");
}

#[test]
fn undefined_control_sequence_inside_a_macro() {
    let d = parse(concat!(
        "(./main.tex\n",
        "./main.tex:7: Undefined control sequence.\n",
        "\\mymacro ->\\foo \n",
        "               bar\n",
        "l.7 \\mymacro\n",
        "             \n",
    ));
    assert_eq!(
        at(&d[0]),
        (K::UndefinedControlSequence, Some("main.tex"), Some(7))
    );
    assert_eq!(d[0].message, "Undefined control sequence \\foo");
}

#[test]
fn package_rerun_warning_spanning_lines() {
    let d = parse(concat!(
        "(./main.tex\n",
        "\n",
        "Package rerunfilecheck Warning: File `main.out' has changed.\n",
        "(rerunfilecheck)                Rerun to get outlines right\n",
        "(rerunfilecheck)                or use package `bookmark'.\n",
        "\n",
    ));
    assert_eq!(d.len(), 1);
    assert_eq!(
        (d[0].severity, d[0].kind),
        (Severity::Info, K::RerunRequired)
    );
    assert!(d[0].message.ends_with("or use package `bookmark'."));
}

#[test]
fn font_and_pdftex_warnings() {
    let d = parse(concat!(
        "(./main.tex\n",
        "LaTeX Font Warning: Font shape `OT1/cmr/bx/it' undefined\n",
        "(Font)              using `OT1/cmr/bx/n' instead on input line 7.\n",
        "\n",
        "pdfTeX warning (ext4): destination with the same identifier (name{page.1}) has been already used, duplicate ignored\n",
    ));
    assert_eq!(
        d.iter().map(at).collect::<Vec<_>>(),
        [
            (K::Other, Some("main.tex"), Some(7)),
            (K::Other, Some("main.tex"), None),
        ]
    );
    assert!(d.iter().all(|d| d.severity == Severity::Warning));
    assert!(d[0].message.contains("instead on input line 7."));
}

#[test]
fn undefined_summary_is_kept_when_alone() {
    let d = parse("(./main.tex\n\nLaTeX Warning: There were undefined references.\n\n)");
    assert_eq!(d.len(), 1);
    assert_eq!((d[0].kind, d[0].line), (K::UndefinedReference, None));
}

#[test]
fn invalid_utf8_and_control_characters() {
    let mut log =
        b"(./main.tex\n./main.tex:2: Undefined control sequence.\nl.2 caf\xE9 \x1b[31m\\foo\n"
            .to_vec();
    log.extend_from_slice(b"         \n");
    let d = parse_log(&log);
    assert_eq!(
        at(&d[0]),
        (K::UndefinedControlSequence, Some("main.tex"), Some(2))
    );
    assert_eq!(d[0].message, "Undefined control sequence \\foo");
    // The excerpt stays close to the log (lossy UTF-8) ...
    assert!(
        d[0].raw_excerpt
            .as_deref()
            .unwrap()
            .contains("caf\u{FFFD} \u{1b}[31m")
    );

    // ... while messages never contain control characters.
    let d = parse_log(b"./main.tex:1: Bad \x1b]0;title\x07 thing.\n");
    assert_eq!(d[0].message, "Bad \u{FFFD}]0;title\u{FFFD} thing.");
}

#[test]
fn unrecognized_or_empty_input_yields_nothing() {
    for log in [
        "",
        "\n\n",
        "!\n",
        "This is pdfTeX\n(./main.tex [1] (./main.aux) )\nOutput written on main.pdf\n",
        "Some random text: 12: without a path\n",
    ] {
        assert!(parse(log).is_empty(), "{log:?}");
    }
}

#[test]
fn huge_lines_are_bounded() {
    let long = "x".repeat(100_000);
    let d = parse(&format!("./main.tex:1: LaTeX Error: {long}\nl.1 {long}\n"));
    assert_eq!(d.len(), 1);
    assert!(d[0].message.len() <= 2048);
    assert!(d[0].raw_excerpt.as_deref().unwrap().len() <= 4096);
}

#[test]
fn diagnostics_serialize_with_the_core_schema() {
    let d = parse("(./sub/a.tex\n./sub/a.tex:3: Undefined control sequence.\nl.3 \\x\n   \n");
    let json = serde_json::to_value(&d[0]).unwrap();
    assert_eq!(json["kind"], "undefined_control_sequence");
    assert_eq!(json["severity"], "error");
    assert_eq!(json["file"], "sub/a.tex");
    assert_eq!(json["line"], 3);
    assert!(json["raw_excerpt"].as_str().unwrap().contains("l.3 \\x"));
}
