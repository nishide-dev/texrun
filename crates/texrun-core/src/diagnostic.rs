//! Structured diagnostics.
//!
//! Only the data model lives here; extracting diagnostics from TeX logs is
//! implemented in the `texrun-latex-log` crate.

use serde::{Deserialize, Serialize};

use crate::path::WorkspacePath;

/// How serious a diagnostic is.
///
/// Variants are declared from least to most serious, and the derived `Ord`
/// follows declaration order; any variant added later is inserted at the
/// position matching its seriousness.
///
/// There is no fallback variant: the Rust `Deserialize` impl is meant for
/// round-tripping documents of the same schema version and rejects unknown
/// values. Non-Rust consumers must tolerate unknown values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Severity {
    /// Informational message (e.g. a rerun notice).
    Info,
    /// Something likely wrong in the output, but the document was produced.
    Warning,
    /// An error that caused or contributed to a failed compile.
    Error,
}

/// A stable, engine-independent classification of a diagnostic.
///
/// Consumers (e.g. AI agents) can branch on this without parsing messages.
/// New kinds may be added as the log parser learns more patterns without a
/// schema version bump; consumers must treat unknown values like
/// [`DiagnosticKind::Other`]. The Rust `Deserialize` impl does exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// A control sequence (macro) is not defined.
    UndefinedControlSequence,
    /// An input file, class or package could not be found.
    MissingFile,
    /// A generic LaTeX / package error without a more specific kind.
    LatexError,
    /// The engine stopped because it could not continue.
    EmergencyStop,
    /// Overfull box.
    OverfullBox,
    /// Underfull box.
    UnderfullBox,
    /// A `\ref` / `\pageref` target is undefined.
    UndefinedReference,
    /// A `\cite` key is undefined.
    UndefinedCitation,
    /// The engine asks for another run (e.g. changed labels).
    RerunRequired,
    /// An error BibTeX reported while reading a `.bib` database (e.g. a
    /// syntax error or a repeated entry), the style or the `.aux` file.
    /// A database or style file that cannot be opened is
    /// [`DiagnosticKind::MissingFile`] instead.
    BibtexError,
    /// BibTeX failed, or was not run, so the bibliography is incomplete or
    /// missing. Follows the individual BibTeX errors (then as
    /// [`Severity::Info`]), or stands alone when there are none to report.
    BibtexFailed,
    /// Recognized as a diagnostic but not classified further. Unknown values
    /// are deserialized as this variant.
    #[serde(other)]
    Other,
}

/// One structured diagnostic.
///
/// `#[non_exhaustive]`: construct with [`Diagnostic::new`] and the `with_*`
/// methods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Diagnostic {
    /// Severity.
    pub severity: Severity,
    /// Classification.
    pub kind: DiagnosticKind,
    /// Human-readable, single-paragraph message.
    pub message: String,
    /// Source file, relative to the workspace root (= the input project root),
    /// if it could be attributed to a file there. Files outside the workspace
    /// (e.g. installed packages) are left `None`; the original text is still
    /// available in [`Diagnostic::raw_excerpt`]. Mapping it back to the
    /// user's original host file is up to the caller (#6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<WorkspacePath>,
    /// 1-based line number in [`Diagnostic::file`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The relevant verbatim excerpt of the engine log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_excerpt: Option<String>,
}

impl Diagnostic {
    /// Creates a diagnostic without location or excerpt.
    pub fn new(severity: Severity, kind: DiagnosticKind, message: impl Into<String>) -> Self {
        Self {
            severity,
            kind,
            message: message.into(),
            file: None,
            line: None,
            raw_excerpt: None,
        }
    }

    /// Sets the source file.
    #[must_use]
    pub fn with_file(mut self, file: WorkspacePath) -> Self {
        self.file = Some(file);
        self
    }

    /// Sets the 1-based line number.
    #[must_use]
    pub fn with_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }

    /// Sets the raw log excerpt.
    #[must_use]
    pub fn with_raw_excerpt(mut self, excerpt: impl Into<String>) -> Self {
        self.raw_excerpt = Some(excerpt.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_shape_and_round_trip() {
        let d = Diagnostic::new(
            Severity::Error,
            DiagnosticKind::UndefinedControlSequence,
            "Undefined control sequence \\foo",
        )
        .with_file(WorkspacePath::new("chapters/intro.tex").unwrap())
        .with_line(12)
        .with_raw_excerpt("./chapters/intro.tex:12: Undefined control sequence.");
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(
            json,
            json!({
                "severity": "error",
                "kind": "undefined_control_sequence",
                "message": "Undefined control sequence \\foo",
                "file": "chapters/intro.tex",
                "line": 12,
                "raw_excerpt": "./chapters/intro.tex:12: Undefined control sequence."
            })
        );
        assert_eq!(serde_json::from_value::<Diagnostic>(json).unwrap(), d);
    }

    #[test]
    fn optional_fields_are_omitted() {
        let d = Diagnostic::new(Severity::Info, DiagnosticKind::RerunRequired, "rerun");
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({ "severity": "info", "kind": "rerun_required", "message": "rerun" })
        );
    }

    #[test]
    fn bibtex_kinds_are_snake_case() {
        for (kind, name) in [
            (DiagnosticKind::BibtexError, "bibtex_error"),
            (DiagnosticKind::BibtexFailed, "bibtex_failed"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<DiagnosticKind>(json!(name)).unwrap(),
                kind
            );
        }
    }

    #[test]
    fn unknown_kind_deserializes_as_other() {
        let d: Diagnostic = serde_json::from_value(json!({
            "severity": "warning", "kind": "missing_package_from_the_future", "message": "m"
        }))
        .unwrap();
        assert_eq!(d.kind, DiagnosticKind::Other);
        assert!(serde_json::from_value::<Severity>(json!("fatal")).is_err());
    }

    #[test]
    fn severity_orders_by_seriousness() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Error);
    }
}
