//! End-to-end fixtures for common LaTeX scenarios (#10): each project in
//! `tests/fixtures/` is copied into a workspace, compiled with the real
//! latexmk and checked on the structured result.
//!
//! See `tests/common/mod.rs` for how these tests are enabled.

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::{
    Compile, assert_location, assert_outcome, assert_page_count, assert_pdf, describe, find, has,
    live_group_members, require_texlive,
};
use texrun_core::{ArtifactKind, CompileOutcome, DiagnosticKind, Severity};
use texrun_preview::{PreviewOptions, PreviewStatus};
use texrun_workspace::OverwritePolicy;

#[test]
fn minimal() {
    require_texlive!();
    let (run, ws) = Compile::fixture("minimal", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    let result = &run.result;
    assert_eq!(result.exit.unwrap().code, Some(0));
    assert_eq!(result.errors().count(), 0, "{}", describe(&run));

    let pdf = assert_pdf(&run, &ws, "main.pdf");
    assert_page_count(&pdf, 1);
    assert_eq!(result.log().unwrap().path.as_str(), "main.log");
    // Only the PDF and the log are artifacts.
    let mut kinds: Vec<_> = result.artifacts.iter().map(|a| a.kind).collect();
    kinds.sort_by_key(|k| format!("{k:?}"));
    assert_eq!(kinds, [ArtifactKind::Log, ArtifactKind::Pdf]);

    // Collecting copies exactly the reported artifacts, with their sizes.
    let dest = tempfile::tempdir().unwrap();
    let collected = ws
        .collect_artifacts(&result.artifacts, dest.path(), OverwritePolicy::Refuse)
        .unwrap();
    assert_eq!(collected.len(), 2);
    for artifact in &collected {
        let len = fs::metadata(dest.path().join(artifact.path.as_path()))
            .unwrap()
            .len();
        assert_eq!(artifact.size_bytes, Some(len), "{artifact:?}");
    }
    assert_eq!(common::names_in(dest.path()), ["main.log", "main.pdf"]);
}

#[test]
fn syntax_error() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("syntax-error", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Failed);
    assert_ne!(run.result.exit.unwrap().code, Some(0));
    assert!(run.result.log().is_some());
    assert!(run.result.pdf().is_none(), "{}", describe(&run));
    let d = find(&run, DiagnosticKind::LatexError);
    assert_eq!(d.severity, Severity::Error);
    assert_location(d, "main.tex", 5);
}

#[test]
fn undefined_command() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("undefined-command", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Failed);
    let d = find(&run, DiagnosticKind::UndefinedControlSequence);
    assert_eq!(d.severity, Severity::Error);
    assert_location(d, "main.tex", 5);
    assert!(d.message.contains("undefinedcommand"), "{d:#?}");
}

#[test]
fn missing_package() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("missing-package", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Failed);
    let d = find(&run, DiagnosticKind::MissingFile);
    assert_eq!(d.severity, Severity::Error);
    // TeX reports where it stopped (the line after `\usepackage`); the
    // engine hands the workspace sources to the parser, which finds the
    // `\usepackage` on line 2.
    assert_location(d, "main.tex", 2);
    assert!(d.message.contains("texrun-no-such-package"), "{d:#?}");
    // The emergency stop that follows is not a second error.
    let stop = find(&run, DiagnosticKind::EmergencyStop);
    assert_eq!(stop.severity, Severity::Info, "{stop:#?}");
    assert_eq!(run.result.errors().count(), 1, "{}", describe(&run));
}

#[test]
fn references() {
    require_texlive!();
    let (run, ws) = Compile::fixture("references", "main.tex").run();
    // Undefined references and citations are warnings, not failures.
    assert_outcome(&run, CompileOutcome::Succeeded);
    let pdf = assert_pdf(&run, &ws, "main.pdf");
    assert_page_count(&pdf, 1);

    // bibtex ran and resolved the known key.
    let bbl = fs::read_to_string(ws.output_dir().join("main.bbl")).unwrap();
    assert!(bbl.contains("knuth"), "{bbl}");

    let refs: Vec<_> = run
        .result
        .warnings()
        .filter(|d| d.kind == DiagnosticKind::UndefinedReference)
        .collect();
    assert!(!refs.is_empty(), "{}", describe(&run));
    assert!(
        refs.iter().all(|d| d.message.contains("sec:missing")),
        "{refs:#?}"
    );
    let cites: Vec<_> = run
        .result
        .warnings()
        .filter(|d| d.kind == DiagnosticKind::UndefinedCitation)
        .collect();
    assert!(!cites.is_empty(), "{}", describe(&run));
    assert!(
        cites.iter().all(|d| d.message.contains("missing-key")),
        "{cites:#?}"
    );
    assert_location(refs[0], "main.tex", 5);
    assert_location(cites[0], "main.tex", 5);

    // kpsewhich is disabled; latexmk may mention that on stderr (the wording
    // depends on the latexmk version, so it is not checked) and carries on.
    // Such console output never becomes a diagnostic: only the .log is
    // parsed. The rc's own failure message must not appear.
    let stderr = String::from_utf8_lossy(&run.stderr.bytes);
    assert!(
        !stderr.contains("texrun: failed to run"),
        "{}",
        describe(&run)
    );
    assert!(
        !run.result
            .diagnostics
            .iter()
            .any(|d| d.message.to_ascii_lowercase().contains("kpsewhich")),
        "{}",
        describe(&run)
    );
}

#[test]
fn overfull_box() {
    require_texlive!();
    let (run, ws) = Compile::fixture("overfull-box", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert_pdf(&run, &ws, "main.pdf");
    let d = find(&run, DiagnosticKind::OverfullBox);
    assert_eq!(d.severity, Severity::Warning);
    assert_location(d, "main.tex", 5);
}

#[test]
fn timeout() {
    require_texlive!();
    let started = Instant::now();
    let (run, ws) = Compile::fixture("timeout", "main.tex")
        .timeout(Duration::from_secs(2))
        .run();
    assert_outcome(&run, CompileOutcome::TimedOut);
    assert_eq!(run.result.exit.unwrap().signal, Some(9));
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(run.result.pdf().is_none(), "{}", describe(&run));
    assert!(!ws.output_dir().join("main.pdf").exists());
    let left = live_group_members(run.pid);
    assert!(left.is_empty(), "processes left behind: {left:#?}");
}

#[test]
fn multi_file() {
    require_texlive!();
    let (run, ws) = Compile::fixture("multi-file", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);

    // The whole tree was copied, and TeX wrote the `\include` aux files
    // into the output directory, not next to the sources.
    for f in [
        "preamble/macros.tex",
        "chapters/intro.tex",
        "chapters/body.tex",
    ] {
        assert!(ws.path().join(f).is_file(), "{f} not copied");
    }
    let out = ws.output_dir();
    assert!(out.join("chapters/intro.aux").is_file());
    assert!(out.join("chapters/body.aux").is_file());
    assert!(!ws.path().join("chapters/intro.aux").exists());

    // The text before the first `\include`, then one page per `\include`.
    let pdf = assert_pdf(&run, &ws, "main.pdf");
    assert_page_count(&pdf, 3);

    // Cross-file references resolve; the missing one is attributed to the
    // included file in its subdirectory.
    let d = find(&run, DiagnosticKind::UndefinedReference);
    assert!(d.message.contains("sec:missing"), "{d:#?}");
    assert_location(d, "chapters/body.tex", 4);
    assert!(
        run.result
            .diagnostics
            .iter()
            .filter(|d| d.kind == DiagnosticKind::UndefinedReference)
            .all(|d| d.message.contains("sec:missing")),
        "{}",
        describe(&run)
    );
}

#[test]
fn multi_file_previews() {
    require_texlive!();
    let Some(previewer) = common::previewer() else {
        return;
    };
    let (mut run, ws) = Compile::fixture("multi-file", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    let pdf = assert_pdf(&run, &ws, "main.pdf");
    let report = previewer
        .render(&pdf, &ws.output_dir(), &PreviewOptions::default())
        .unwrap();
    assert_eq!(report.status, PreviewStatus::Rendered, "{report:#?}");
    assert_eq!(report.pdf.as_ref().unwrap().page_count, 3);
    report.attach_to(&mut run.result);

    // Previews are artifacts like the PDF and are collected with it.
    let previews: Vec<_> = run.result.artifacts_of(ArtifactKind::Preview).collect();
    let pages: Vec<_> = previews.iter().map(|a| a.page).collect();
    assert_eq!(pages, [Some(1), Some(2), Some(3)]);
    let dest = tempfile::tempdir().unwrap();
    let collected = ws
        .collect_artifacts(&run.result.artifacts, dest.path(), OverwritePolicy::Refuse)
        .unwrap();
    assert_eq!(collected.len(), 5, "{collected:#?}");
    for artifact in collected.iter().filter(|a| a.kind == ArtifactKind::Preview) {
        let png = fs::read(dest.path().join(artifact.path.as_path())).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "{artifact:?}");
    }
}

#[test]
fn multi_file_error_in_an_input_file() {
    require_texlive!();
    let (run, _ws) = Compile::fixture("multi-file", "broken.tex").run();
    assert_outcome(&run, CompileOutcome::Failed);
    let d = find(&run, DiagnosticKind::UndefinedControlSequence);
    assert_location(d, "chapters/broken.tex", 2);
}

#[test]
fn japanese_and_space_file_names() {
    require_texlive!();
    let (run, ws) = Compile::fixture("unicode-names", "論文 下書き.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    let pdf = assert_pdf(&run, &ws, "論文 下書き.pdf");
    assert_page_count(&pdf, 1);
    assert_eq!(run.result.log().unwrap().path.as_str(), "論文 下書き.log");
    // Diagnostics keep the non-ASCII path of the input file. (Warnings in
    // files whose names contain spaces get no file: TeX prints those names
    // unquoted, see texrun-latex-log.)
    let d = find(&run, DiagnosticKind::UndefinedReference);
    assert_location(d, "章/節1.tex", 3);

    // A full-width space (U+3000) passes the name check and survives
    // latexmk's argument splitting.
    let (wide, wide_ws) = Compile::fixture("unicode-names", "全角\u{3000}空白.tex").run();
    assert_outcome(&wide, CompileOutcome::Succeeded);
    assert_pdf(&wide, &wide_ws, "全角\u{3000}空白.pdf");
    assert!(!has(&wide, DiagnosticKind::UndefinedReference));
}
