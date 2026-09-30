//! Structured diagnostics.
//!
//! Only the data model lives here; extracting diagnostics from engine logs is
//! implemented separately (#7).

use serde::{Deserialize, Serialize};

use crate::path::WorkspacePath;

/// How serious a diagnostic is.
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
/// New kinds may be added as the log parser learns more patterns; consumers
/// must treat unknown values like [`DiagnosticKind::Other`].
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
    /// Recognized as a diagnostic but not classified further.
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
    /// Source file, if it could be attributed to a file inside the workspace.
    /// Files outside the workspace (e.g. installed packages) are left `None`;
    /// the original text is still available in [`Diagnostic::raw_excerpt`].
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
    fn severity_orders_by_seriousness() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Error);
    }
}
