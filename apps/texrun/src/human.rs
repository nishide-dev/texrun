//! Human-readable rendering of a compile report.
//!
//! Everything taken from the document or the filesystem is passed through
//! [`escape`](crate::escape::escape) before it is printed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use texrun_core::{
    ArtifactKind, CompileOutcome, CompileResult, Diagnostic, DiagnosticKind, Severity,
};
use texrun_preview::PreviewReport;
use texrun_workspace::{ExclusionReason, MaterializeReport};

use crate::duration::format_duration;
use crate::escape::{escape, escape_path};
use crate::report::ErrorInfo;

/// At most this many diagnostics of each severity are printed; the rest are
/// counted (`--json` has all of them).
pub const MAX_SHOWN_PER_SEVERITY: usize = 20;

/// At most this many notable workspace exclusions are listed.
const MAX_SHOWN_EXCLUSIONS: usize = 5;

/// How to show paths relative to what the user typed.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The entrypoint argument as given.
    pub entrypoint: PathBuf,
    /// The project root as the user would write it: `--root` as given, or
    /// the entrypoint's directory as given (empty for the current directory).
    pub root: PathBuf,
    /// The output directory as given, or the default next to the entrypoint.
    pub output: PathBuf,
}

impl Paths {
    fn source(&self, file: &str) -> String {
        escape_path(&self.root.join(file))
    }

    fn output_file(&self, rel: &str) -> String {
        escape_path(&self.output.join(rel))
    }
}

/// Renders the result of a compile that ran to an outcome (stdout).
pub fn render_result(
    result: &CompileResult,
    preview: Option<&PreviewReport>,
    paths: &Paths,
    timeout: Duration,
) -> String {
    let mut out = String::new();
    render_diagnostics(&mut out, &result.diagnostics, paths);

    let entry = escape_path(&paths.entrypoint);
    let elapsed = format_duration(result.elapsed);
    let counts = counts(result);
    let _ = match result.outcome {
        CompileOutcome::Succeeded => writeln!(out, "Compiled {entry} in {elapsed}{counts}"),
        CompileOutcome::Failed => writeln!(out, "Failed to compile {entry} in {elapsed}{counts}"),
        CompileOutcome::TimedOut => writeln!(
            out,
            "Timed out compiling {entry} after {elapsed} (limit {}; raise it with --timeout){counts}",
            format_duration(timeout)
        ),
        CompileOutcome::Cancelled => writeln!(out, "Cancelled compiling {entry} after {elapsed}"),
        _ => writeln!(out, "Compile of {entry} ended after {elapsed}{counts}"),
    };
    for artifact in &result.artifacts {
        let label = match artifact.kind {
            ArtifactKind::Pdf => "PDF",
            ArtifactKind::Log if result.is_success() => continue,
            ArtifactKind::Log => "log",
            ArtifactKind::Preview => continue,
            _ => "file",
        };
        let _ = writeln!(
            out,
            "  {label}: {}",
            paths.output_file(artifact.path.as_str())
        );
    }
    if result.is_success() && result.pdf().is_none() {
        let _ = writeln!(out, "  (no PDF was reported)");
    }
    if let Some(preview) = preview {
        render_preview(&mut out, preview, paths);
    }
    out
}

/// `previews: <first> .. <last> (N of M pages, backend)` and the notices.
fn render_preview(out: &mut String, preview: &PreviewReport, paths: &Paths) {
    let pages = &preview.pages;
    if let (Some(first), Some(last)) = (pages.first(), pages.last()) {
        let first_path = paths.output_file(first.artifact.path.as_str());
        let range = if pages.len() == 1 {
            first_path
        } else {
            format!("{first_path} .. {}", escape(last.artifact.path.file_name()))
        };
        let total = match &preview.pdf {
            Some(pdf) => format!("{} of {} pages", pages.len(), pdf.page_count),
            None => format!("{} pages", pages.len()),
        };
        let backend = preview
            .backend
            .map(|b| format!(", {}", b.name()))
            .unwrap_or_default();
        let label = if pages.len() == 1 {
            "preview"
        } else {
            "previews"
        };
        let _ = writeln!(out, "  {label}: {range} ({total}{backend})");
    }
    for notice in &preview.notices {
        let label = if notice.severity >= Severity::Warning {
            "warning"
        } else {
            "note"
        };
        let page = notice
            .page
            .map(|p| format!(" (page {p})"))
            .unwrap_or_default();
        let _ = writeln!(out, "{label}: preview: {}{page}", escape(&notice.message));
    }
}

fn counts(result: &CompileResult) -> String {
    let errors = result.errors().count();
    let warnings = result.warnings().count();
    if errors == 0 && warnings == 0 {
        return String::new();
    }
    format!(
        " ({errors} error{}, {warnings} warning{})",
        plural(errors),
        plural(warnings)
    )
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Prints errors, then warnings, then notes, each group in log order with
/// identical diagnostics merged (`(xN)`), and at most
/// [`MAX_SHOWN_PER_SEVERITY`] entries per group.
fn render_diagnostics(out: &mut String, diagnostics: &[Diagnostic], paths: &Paths) {
    for (severity, label) in [
        (Severity::Error, "error"),
        (Severity::Warning, "warning"),
        (Severity::Info, "note"),
    ] {
        let mut groups: Vec<(&Diagnostic, usize)> = Vec::new();
        for d in diagnostics.iter().filter(|d| d.severity == severity) {
            // Rerun requests are handled by latexmk; not worth a line.
            if d.kind == DiagnosticKind::RerunRequired {
                continue;
            }
            match groups.iter_mut().find(|(g, _)| same(g, d)) {
                Some((_, n)) => *n += 1,
                None => groups.push((d, 1)),
            }
        }
        for (d, n) in groups.iter().take(MAX_SHOWN_PER_SEVERITY) {
            let location = match (&d.file, d.line) {
                (Some(file), Some(line)) => format!("{}:{line}: ", paths.source(file.as_str())),
                (Some(file), None) => format!("{}: ", paths.source(file.as_str())),
                (None, _) => String::new(),
            };
            let repeat = if *n > 1 {
                format!(" (x{n})")
            } else {
                String::new()
            };
            let _ = writeln!(out, "{location}{label}: {}{repeat}", escape(&d.message));
        }
        if groups.len() > MAX_SHOWN_PER_SEVERITY {
            let _ = writeln!(
                out,
                "... and {} more {label}s (use --json for all diagnostics)",
                groups.len() - MAX_SHOWN_PER_SEVERITY
            );
        }
    }
    if diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::MissingFile && d.message.contains("../"))
    {
        let _ = writeln!(
            out,
            "note: TeX cannot read files above the entrypoint's directory; put the entrypoint \
             in the project root and pass that directory as --root"
        );
    }
}

fn same(a: &Diagnostic, b: &Diagnostic) -> bool {
    a.kind == b.kind && a.file == b.file && a.line == b.line && a.message == b.message
}

/// Warnings about the workspace copy (stderr): tool configuration that was
/// not copied (docs/security.md §3.3) and other entries a user might miss.
pub fn render_workspace_warnings(report: &MaterializeReport) -> String {
    let mut out = String::new();
    let notable: Vec<_> = report
        .excluded
        .iter()
        .filter_map(|e| {
            let why = match e.reason {
                ExclusionReason::ToolConfig => {
                    "tool configuration is never copied; texrun does not read latexmkrc / biber.conf"
                }
                ExclusionReason::ExcludedExtension => "precompiled formats are not copied",
                ExclusionReason::UnresolvableSymlink => "dangling symlink",
                ExclusionReason::SymlinkToExcluded => "symlink to an excluded location",
                ExclusionReason::SpecialFile => "not a regular file, directory or symlink",
                _ => return None,
            };
            Some((&e.path, why))
        })
        .collect();
    for (path, why) in notable.iter().take(MAX_SHOWN_EXCLUSIONS) {
        let _ = writeln!(
            out,
            "warning: not copied into the workspace: {} ({why})",
            escape_path(path)
        );
    }
    if notable.len() > MAX_SHOWN_EXCLUSIONS {
        let _ = writeln!(
            out,
            "warning: ... and {} more entries not copied (see --json)",
            notable.len() - MAX_SHOWN_EXCLUSIONS
        );
    }
    if report.vanished > 0 {
        let _ = writeln!(
            out,
            "warning: {} entries disappeared while the project was being copied",
            report.vanished
        );
    }
    out
}

/// A texrun error (stderr).
pub fn render_error(error: &ErrorInfo) -> String {
    let mut out = format!("error: {}\n", escape(&error.message));
    if let Some(hint) = &error.hint {
        let _ = writeln!(out, "hint: {}", escape(hint));
    }
    out
}

/// `path` for a stderr message.
pub fn show(path: &Path) -> String {
    escape_path(path)
}

#[cfg(test)]
mod tests {
    use texrun_core::{Artifact, EngineInfo, WorkspacePath};

    use super::*;

    fn paths() -> Paths {
        Paths {
            entrypoint: "paper/main.tex".into(),
            root: "paper".into(),
            output: "paper/texrun-out".into(),
        }
    }

    fn diag(
        severity: Severity,
        message: &str,
        file: Option<&str>,
        line: Option<u32>,
    ) -> Diagnostic {
        let mut d = Diagnostic::new(severity, DiagnosticKind::LatexError, message);
        if let Some(f) = file {
            d = d.with_file(WorkspacePath::new(f).unwrap());
        }
        if let Some(l) = line {
            d = d.with_line(l);
        }
        d
    }

    #[test]
    fn failure_lists_located_errors_and_the_log() {
        let mut r = CompileResult::new(
            CompileOutcome::Failed,
            EngineInfo::new("texlive"),
            Duration::from_millis(840),
        );
        r.diagnostics.push(diag(
            Severity::Warning,
            "Overfull \\hbox",
            Some("main.tex"),
            Some(9),
        ));
        r.diagnostics.push(diag(
            Severity::Error,
            "Undefined control sequence",
            Some("chapters/intro.tex"),
            Some(3),
        ));
        r.artifacts.push(Artifact::new(
            ArtifactKind::Log,
            WorkspacePath::new("main.log").unwrap(),
        ));
        let text = render_result(&r, None, &paths(), Duration::from_secs(60));
        assert_eq!(
            text,
            "paper/chapters/intro.tex:3: error: Undefined control sequence\n\
             paper/main.tex:9: warning: Overfull \\hbox\n\
             Failed to compile paper/main.tex in 840ms (1 error, 1 warning)\n  \
             log: paper/texrun-out/main.log\n"
        );
    }

    #[test]
    fn success_shows_the_pdf_only() {
        let mut r = CompileResult::new(
            CompileOutcome::Succeeded,
            EngineInfo::new("texlive"),
            Duration::from_millis(1234),
        );
        r.artifacts.push(Artifact::new(
            ArtifactKind::Pdf,
            WorkspacePath::new("main.pdf").unwrap(),
        ));
        r.artifacts.push(Artifact::new(
            ArtifactKind::Log,
            WorkspacePath::new("main.log").unwrap(),
        ));
        assert_eq!(
            render_result(&r, None, &paths(), Duration::from_secs(60)),
            "Compiled paper/main.tex in 1.23s\n  PDF: paper/texrun-out/main.pdf\n"
        );
    }

    #[test]
    fn repeated_diagnostics_are_merged_and_capped() {
        let mut diagnostics =
            vec![diag(Severity::Error, "Unicode char", Some("main.tex"), Some(1)); 3];
        for i in 0..30 {
            diagnostics.push(diag(Severity::Warning, &format!("w{i}"), None, None));
        }
        let mut out = String::new();
        render_diagnostics(&mut out, &diagnostics, &paths());
        assert!(out.starts_with("paper/main.tex:1: error: Unicode char (x3)\nwarning: w0\n"));
        assert!(out.contains("warning: w19\n"));
        assert!(!out.contains("warning: w20\n"));
        assert!(out.ends_with("... and 10 more warnings (use --json for all diagnostics)\n"));
    }

    #[test]
    fn untrusted_text_is_escaped() {
        let diagnostics = vec![diag(
            Severity::Error,
            "bad\u{1b}[2J\nfake: ok",
            Some("a\u{202E}xet.tex"),
            Some(2),
        )];
        let mut out = String::new();
        render_diagnostics(&mut out, &diagnostics, &paths());
        assert_eq!(
            out,
            "paper/a\\u{202E}xet.tex:2: error: bad\\u{001B}[2J\\nfake: ok\n"
        );
    }

    #[test]
    fn timeout_mentions_the_limit() {
        let r = CompileResult::new(
            CompileOutcome::TimedOut,
            EngineInfo::new("texlive"),
            Duration::from_secs(5),
        );
        let text = render_result(&r, None, &paths(), Duration::from_secs(5));
        assert!(text.contains("Timed out compiling paper/main.tex after 5s (limit 5s"));
    }

    #[test]
    fn previews_and_their_notices_are_listed_and_escaped() {
        let preview: PreviewReport = serde_json::from_value(serde_json::json!({
            "status": "partial",
            "backend": "mupdf",
            "pdf": { "page_count": 30 },
            "pages": [
                { "kind": "preview", "path": "preview/page-001.png", "page": 1,
                  "width_px": 10, "height_px": 10, "dpi": 144 },
                { "kind": "preview", "path": "preview/page-002.png", "page": 2,
                  "width_px": 10, "height_px": 10, "dpi": 144 }
            ],
            "notices": [
                { "severity": "info", "kind": "page_limit",
                  "message": "only the first 20 pages" },
                { "severity": "warning", "kind": "render_failed",
                  "message": "bad \u{202E}page", "page": 3 }
            ]
        }))
        .unwrap();
        let mut out = String::new();
        render_preview(&mut out, &preview, &paths());
        assert_eq!(
            out,
            "  previews: paper/texrun-out/preview/page-001.png .. page-002.png (2 of 30 pages, mupdf)\n\
             note: preview: only the first 20 pages\n\
             warning: preview: bad \\u{202E}page (page 3)\n"
        );
    }
}
