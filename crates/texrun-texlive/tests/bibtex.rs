//! BibTeX failures reported as diagnostics (#27), with the real latexmk and
//! BibTeX on `tests/fixtures/bibtex/` (one project, one entrypoint per
//! scenario).
//!
//! See `tests/common/mod.rs` for how these tests are enabled.

mod common;

use common::{Compile, assert_location, assert_outcome, describe, file_of, find, require_texlive};
use texrun_core::{CompileOutcome, Diagnostic, DiagnosticKind, Severity};

fn of_kind(run: &texrun_texlive::LatexmkRun, kind: DiagnosticKind) -> Vec<&Diagnostic> {
    run.result
        .diagnostics
        .iter()
        .filter(|d| d.kind == kind)
        .collect()
}

#[test]
fn syntax_error_in_a_database() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("bibtex", "syntax.tex").run();
    // latexmk stops after BibTeX's error (the PDF of the first pass exists).
    assert_outcome(&run, CompileOutcome::Failed);
    let d = find(&run, DiagnosticKind::BibtexError);
    assert_eq!(d.severity, Severity::Error);
    // BibTeX reports the line of the unexpected token (after the missing
    // comma) and says the error may be on the previous line.
    assert_location(d, "bad.bib", 5);
    assert!(d.message.contains("expecting a `,' or a `}'"), "{d:#?}");
    assert!(d.message.contains("previous line"), "{d:#?}");
    let failed = find(&run, DiagnosticKind::BibtexFailed);
    assert_eq!(failed.severity, Severity::Info, "{failed:#?}");
    // The only error is the one to fix.
    assert_eq!(run.result.errors().count(), 1, "{}", describe(&run));
}

#[test]
fn missing_database() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("bibtex", "missing-database.tex").run();
    // latexmk does not run BibTeX and exits with 0.
    assert_outcome(&run, CompileOutcome::Succeeded);
    let missing: Vec<_> = of_kind(&run, DiagnosticKind::MissingFile);
    assert_eq!(missing.len(), 1, "{}", describe(&run));
    let d = missing[0];
    assert_eq!(d.severity, Severity::Warning);
    assert!(
        d.message.contains("texrun-no-such-database.bib") && d.message.contains("not run"),
        "{d:#?}"
    );
    assert_eq!(file_of(d), None);
    // The citation it leaves undefined is still reported.
    assert!(!of_kind(&run, DiagnosticKind::UndefinedCitation).is_empty());
}

#[test]
fn unreadable_database() {
    require_texlive!();
    // latexmk finds the file, but BibTeX may not open a dot directory
    // (kpathsea paranoid mode, docs/security.md §3.7).
    let (run, ws) = Compile::fixture("bibtex", "hidden-database.tex").run();
    assert!(
        ws.path().join(".hidden/refs.bib").is_file(),
        "fixture not copied"
    );
    assert_outcome(&run, CompileOutcome::Failed);
    let d = find(&run, DiagnosticKind::MissingFile);
    assert_eq!(d.severity, Severity::Error);
    assert!(d.message.contains(".hidden/refs.bib"), "{d:#?}");
    // The location BibTeX gives is in the generated `.aux` file.
    assert_eq!((file_of(d), d.line), (None, None));
    assert_eq!(
        find(&run, DiagnosticKind::BibtexFailed).severity,
        Severity::Info
    );
}

#[test]
fn missing_entry() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("bibtex", "missing-entry.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    let cites = of_kind(&run, DiagnosticKind::UndefinedCitation);
    // LaTeX's warning and BibTeX's, which tells that the database has no
    // such entry.
    let bibtex: Vec<_> = cites
        .iter()
        .filter(|d| d.message.starts_with("BibTeX warning:"))
        .collect();
    assert_eq!(bibtex.len(), 1, "{}", describe(&run));
    assert!(bibtex[0].message.contains("texrun-no-such-key"));
    assert!(cites.iter().any(|d| d.file.is_some()));
    assert!(!has_bibtex_failure(&run), "{}", describe(&run));
}

#[test]
fn missing_style() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("bibtex", "missing-style.tex").run();
    assert_outcome(&run, CompileOutcome::Failed);
    let d = find(&run, DiagnosticKind::MissingFile);
    assert_eq!(d.severity, Severity::Error);
    assert!(d.message.contains("texrun-no-such-style.bst"), "{d:#?}");
    assert_eq!(file_of(d), None);
    assert_eq!(
        find(&run, DiagnosticKind::BibtexFailed).severity,
        Severity::Info
    );
}

#[test]
fn several_databases_from_a_subdirectory_entrypoint() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("bibtex", "paper/main.tex").run();
    assert_outcome(&run, CompileOutcome::Failed);
    let errors = of_kind(&run, DiagnosticKind::BibtexError);
    let located: Vec<_> = errors.iter().map(|d| (file_of(d), d.line)).collect();
    assert_eq!(
        located,
        [
            (Some("paper/bib/more.bib"), Some(8)),
            (Some("paper/bib/more.bib"), Some(12))
        ],
        "{}",
        describe(&run)
    );
    let undefined = run
        .result
        .diagnostics
        .iter()
        .find(|d| d.message.contains("texrunundefinedstring"))
        .unwrap_or_else(|| panic!("{}", describe(&run)));
    assert_location(undefined, "paper/bib/more.bib", 4);
    assert_eq!(undefined.severity, Severity::Warning);
}

#[test]
fn a_working_bibliography_reports_no_bibtex_problem() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("references", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert!(!has_bibtex_failure(&run), "{}", describe(&run));
    assert!(
        !run.result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::MissingFile),
        "{}",
        describe(&run)
    );
}

fn has_bibtex_failure(run: &texrun_texlive::LatexmkRun) -> bool {
    run.result.diagnostics.iter().any(|d| {
        matches!(
            d.kind,
            DiagnosticKind::BibtexError | DiagnosticKind::BibtexFailed
        )
    })
}
