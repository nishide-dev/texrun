//! Tests against real BibTeX logs produced by TeX Live (`fixtures/logs/
//! bibtex-*.blg`, see `fixtures/regenerate.sh`; the documents are in
//! `fixtures/src/bibtex-*/`).
//!
//! Files are resolved the way the TeX Live engine does: a database or style
//! name is a file of the document directory if it exists there.

use std::path::Path;

use texrun_core::WorkspacePath;
use texrun_latex_log::{BlgParser, Diagnostic, DiagnosticKind as K, ParsedBlg, Severity};

fn parse(name: &str) -> ParsedBlg {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let log = fixtures.join("logs").join(format!("{name}.blg"));
    let blg = std::fs::read(&log).unwrap_or_else(|e| panic!("{}: {e}", log.display()));
    let doc = fixtures.join("src").join(name);
    let files = |file: &WorkspacePath| doc.join(file.as_path()).is_file().then(|| file.clone());
    BlgParser::new().parse_with_files(&blg, &files)
}

fn summary(d: &[Diagnostic]) -> Vec<(Severity, K, Option<&str>, Option<u32>)> {
    d.iter()
        .map(|d| {
            (
                d.severity,
                d.kind,
                d.file.as_ref().map(WorkspacePath::as_str),
                d.line,
            )
        })
        .collect()
}

#[test]
fn syntax_error() {
    let parsed = parse("bibtex-syntax");
    assert!(parsed.failed);
    assert_eq!(parsed.error_messages, Some(1));
    let d = &parsed.diagnostics;
    assert_eq!(
        summary(d),
        [
            (Severity::Error, K::BibtexError, Some("refs.bib"), Some(5)),
            (Severity::Warning, K::Other, None, None),
            (Severity::Info, K::BibtexFailed, None, None),
        ]
    );
    assert_eq!(
        d[0].message,
        "BibTeX: I was expecting a `,' or a `}' (the error may be on the previous line)"
    );
    let excerpt = d[0].raw_excerpt.as_deref().unwrap();
    assert!(excerpt.contains("year      = {1984}"), "{excerpt}");
    assert!(excerpt.ends_with("I'm skipping whatever remains of this entry"));
    assert_eq!(d[1].message, "BibTeX warning: empty year in knuth");
    assert_eq!(
        d[2].message,
        "BibTeX failed with 1 error message; the bibliography is incomplete"
    );
}

#[test]
fn missing_database() {
    let parsed = parse("bibtex-missing-database");
    assert!(parsed.failed);
    let d = &parsed.diagnostics;
    assert_eq!(
        summary(d),
        [
            // The location is the `.aux` file: not reported.
            (Severity::Error, K::MissingFile, None, None),
            (Severity::Error, K::BibtexError, None, None),
            (Severity::Warning, K::UndefinedCitation, None, None),
            (Severity::Info, K::BibtexFailed, None, None),
        ]
    );
    assert_eq!(
        d[0].message,
        "BibTeX: I couldn't open database file texrun-no-such-database.bib"
    );
    assert!(
        d[0].raw_excerpt
            .as_deref()
            .unwrap()
            .contains("\\bibdata{texrun-no-such-database")
    );
    assert_eq!(d[1].message, "BibTeX: I found no database files");
    assert_eq!(
        d[2].message,
        "BibTeX warning: I didn't find a database entry for \"knuth\""
    );
    assert!(d[3].message.contains("2 error messages"));
}

#[test]
fn missing_entry() {
    let parsed = parse("bibtex-missing-entry");
    // Warnings only: BibTeX succeeded.
    assert!(!parsed.failed);
    assert_eq!(parsed.error_messages, None);
    let d = &parsed.diagnostics;
    assert_eq!(
        summary(d),
        [(Severity::Warning, K::UndefinedCitation, None, None)]
    );
    assert!(d[0].message.contains("\"texrun-no-such-key\""), "{d:#?}");
}

#[test]
fn missing_style() {
    let parsed = parse("bibtex-missing-style");
    assert!(parsed.failed);
    let d = &parsed.diagnostics;
    assert_eq!(
        summary(d),
        [
            (Severity::Error, K::MissingFile, None, None),
            (Severity::Error, K::BibtexError, None, None),
            (Severity::Info, K::BibtexFailed, None, None),
        ]
    );
    assert_eq!(
        d[0].message,
        "BibTeX: I couldn't open style file texrun-no-such-style.bst"
    );
    assert_eq!(d[1].message, "BibTeX: I found no style file");
}

#[test]
fn several_databases() {
    let parsed = parse("bibtex-multi-database");
    assert!(parsed.failed);
    assert_eq!(parsed.error_messages, Some(2));
    let d = &parsed.diagnostics;
    assert_eq!(
        summary(d),
        [
            // An undefined string is reported on its own line.
            (Severity::Warning, K::Other, Some("bib/more.bib"), Some(4)),
            (
                Severity::Error,
                K::BibtexError,
                Some("bib/more.bib"),
                Some(8)
            ),
            (
                Severity::Error,
                K::BibtexError,
                Some("bib/more.bib"),
                Some(12)
            ),
            (Severity::Warning, K::Other, None, None),
            (Severity::Info, K::BibtexFailed, None, None),
        ]
    );
    assert_eq!(
        d[0].message,
        "BibTeX warning: string name \"texrunundefinedstring\" is undefined"
    );
    assert_eq!(d[1].message, "BibTeX: Repeated entry");
    assert_eq!(d[2].message, "BibTeX: I was expecting a `,' or a `}'");
}

#[test]
fn without_files_no_location_is_guessed() {
    let blg = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/logs/bibtex-syntax.blg"),
    )
    .unwrap();
    let parsed = BlgParser::new().parse(&blg);
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|d| d.file.is_none() && d.line.is_none())
    );
    assert_eq!(parsed.diagnostics[0].kind, K::BibtexError);
}
