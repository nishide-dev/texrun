//! The result of `texrun compile`: its JSON document and exit code.
//!
//! JSON (`--json`) is `Versioned<CompileReport>`:
//!
//! ```text
//! {
//!   "schema_version": 1,
//!   // Present when the compile ran to an outcome (the fields of CompileResult):
//!   "outcome": "succeeded" | "failed" | "timed_out" | "cancelled",
//!   "engine": { "name": "texlive", "version": "latexmk 4.86" },
//!   "exit": { "code": 0 },
//!   "elapsed_ms": 1234,
//!   "diagnostics": [ { "severity", "kind", "message", "file"?, "line"?, "raw_excerpt"? } ],
//!   "artifacts": [ { "kind": "pdf", "path": "main.pdf", "size_bytes": 1234 } ],
//!   // After a successful compile, unless --no-preview (texrun_preview::PreviewReport):
//!   "preview": { "status": "rendered", "backend": "mupdf", "format": "png",
//!                "pdf": { "page_count": 3, "pages": [ { "page": 1, "width_pt", "height_pt", "rotation" } ] },
//!                "pages": [ { "kind": "preview", "path": "preview/page-001.png", "page": 1,
//!                             "size_bytes", "width_px", "height_px", "dpi" } ],
//!                "notices": [ { "severity", "kind", "message", "page"?, "detail"? } ] },
//!   // Absolute host directory the artifact paths are relative to (when collected):
//!   "output_dir": "/abs/texrun-out",
//!   // Once the entrypoint was resolved; diagnostic files are relative to `root`:
//!   "project": { "root": "/abs/project", "entrypoint": "main.tex" },
//!   // Once the workspace was created:
//!   "workspace": { "excluded": [ { "path": "latexmkrc", "reason": "tool_config" } ],
//!                  "excluded_total": 1, "vanished": 0, "kept_path"?: "/tmp/texrun-ws-..." },
//!   // Present when texrun could not finish (exit code 2 or 3):
//!   "error": { "stage": "probe", "kind": "unavailable", "category": "runtime", "message": "..." }
//! }
//! ```
//!
//! Consumers check `error` first, then `outcome`. `error` may appear together
//! with the compile fields (e.g. the compile ran, but copying its artifacts
//! failed). New optional fields and enum values may be added without a
//! schema version bump.

use std::error::Error as StdError;
use std::path::Path;
use std::process::ExitCode;

use serde::Serialize;
use texrun_core::{CompileOutcome, CompileResult, EngineError};
use texrun_preview::PreviewReport;
use texrun_workspace::{ExclusionReason, MaterializeReport, WorkspaceError};

/// Exit codes of `texrun` (documented in the help and README).
pub mod exit {
    /// The document compiled and a PDF was produced.
    pub const SUCCESS: u8 = 0;
    /// The document failed to compile.
    pub const COMPILE_FAILED: u8 = 1;
    /// Usage or input error (same as clap's usage error code).
    pub const USAGE: u8 = 2;
    /// texrun runtime or configuration error.
    pub const RUNTIME: u8 = 3;
    /// The compile timed out.
    pub const TIMED_OUT: u8 = 4;
    /// Cancelled, when the signal is unknown (otherwise 128 + signal).
    pub const CANCELLED: u8 = 130;
}

/// The payload of the `--json` document (wrapped in `Versioned`).
#[derive(Debug, Default, Serialize)]
pub struct CompileReport {
    /// The compile result, when the engine ran to an outcome. Artifact
    /// paths are relative to [`CompileReport::output_dir`] once collected.
    #[serde(flatten)]
    pub result: Option<CompileResult>,
    /// Absolute host directory that holds the collected artifacts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    /// Page previews, when they were attempted (after a successful compile,
    /// unless `--no-preview`). Their images are also in `artifacts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<PreviewReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

/// The resolved project.
#[derive(Debug, Serialize)]
pub struct ProjectInfo {
    /// Canonical host path of the project root.
    pub root: String,
    /// Entrypoint relative to the root.
    pub entrypoint: String,
}

/// What happened while preparing the workspace.
#[derive(Debug, Default, Serialize)]
pub struct WorkspaceInfo {
    /// Entries left out of the workspace (at most
    /// [`texrun_workspace::MAX_RECORDED_ENTRIES`]), paths relative to the
    /// project root.
    pub excluded: Vec<ExcludedInfo>,
    /// Total number of excluded entries.
    pub excluded_total: u64,
    /// Entries that disappeared while the project was being copied.
    pub vanished: u64,
    /// The workspace directory, if it was kept (`--keep-workspace`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_path: Option<String>,
}

/// One excluded entry.
#[derive(Debug, Serialize)]
pub struct ExcludedInfo {
    pub path: String,
    pub reason: &'static str,
}

impl WorkspaceInfo {
    pub fn from_report(report: &MaterializeReport) -> Self {
        Self {
            excluded: report
                .excluded
                .iter()
                .map(|e| ExcludedInfo {
                    path: e.path.to_string_lossy().into_owned(),
                    reason: exclusion_reason(e.reason),
                })
                .collect(),
            excluded_total: report.excluded_total,
            vanished: report.vanished,
            kept_path: None,
        }
    }
}

/// Stable `snake_case` name of an exclusion reason.
pub fn exclusion_reason(reason: ExclusionReason) -> &'static str {
    match reason {
        ExclusionReason::ExcludedName => "excluded_name",
        ExclusionReason::ExcludedExtension => "excluded_extension",
        ExclusionReason::ToolConfig => "tool_config",
        ExclusionReason::OutputDirectory => "output_directory",
        ExclusionReason::SymlinkToExcluded => "symlink_to_excluded",
        ExclusionReason::UnresolvableSymlink => "unresolvable_symlink",
        ExclusionReason::SpecialFile => "special_file",
        ExclusionReason::WorkspaceDirectory => "workspace_directory",
        _ => "other",
    }
}

/// Which step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Command-line arguments.
    Args,
    /// Resolving the entrypoint and project root.
    Project,
    /// Locating and probing latexmk.
    Probe,
    /// Creating the workspace.
    Workspace,
    /// Running the engine.
    Compile,
    /// Copying artifacts to the output directory.
    Collect,
    /// Setting up texrun itself (signal handling).
    Setup,
}

/// Broad class of an error; decides the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Invalid command line (exit 2).
    Usage,
    /// A problem with the given project / entrypoint (exit 2).
    Input,
    /// texrun or its environment failed (exit 3).
    Runtime,
}

/// A failure of texrun (as opposed to a document that failed to compile).
#[derive(Debug, Clone, Serialize)]
pub struct ErrorInfo {
    pub stage: Stage,
    /// Stable `snake_case` code: an `EngineErrorKind` (stages `probe`,
    /// `compile`), a `WorkspaceErrorKind` (`project`, `workspace`,
    /// `collect`) or a CLI code (`usage`, `unsupported_option`,
    /// `unsafe_root`, `non_utf8_path`, `signal_setup`).
    pub kind: String,
    pub category: Category,
    /// Human-readable message including the error's causes.
    pub message: String,
    /// A suggestion for fixing it, if texrun has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ErrorInfo {
    pub fn new(
        stage: Stage,
        kind: impl Into<String>,
        category: Category,
        message: impl Into<String>,
    ) -> Self {
        Self {
            stage,
            kind: kind.into(),
            category,
            message: message.into(),
            hint: None,
        }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn from_engine(stage: Stage, err: &EngineError) -> Self {
        use texrun_core::EngineErrorKind as K;
        let kind = err.kind();
        let category = match kind {
            K::InvalidRequest => Category::Input,
            _ => Category::Runtime,
        };
        Self::new(stage, snake_case_name(&kind), category, error_chain(err))
    }

    pub fn from_workspace(stage: Stage, err: &WorkspaceError) -> Self {
        use texrun_workspace::WorkspaceErrorKind as K;
        let kind = err.kind();
        let category = if kind.is_input_error() {
            Category::Input
        } else {
            Category::Runtime
        };
        let info = Self::new(stage, snake_case_name(&kind), category, error_chain(err));
        match kind {
            K::EntrypointOutsideRoot => {
                info.with_hint("pass a --root that contains the entrypoint")
            }
            K::LimitExceeded => info.with_hint(
                "the whole project root is copied into the workspace; move the document into \
                 its own directory or pass a narrower --root",
            ),
            K::SymlinkOutsideRoot => info.with_hint(
                "copy the linked file into the project, or pass a --root that contains its target",
            ),
            _ => info,
        }
    }

    /// The exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self.category {
            Category::Usage | Category::Input => exit::USAGE,
            Category::Runtime => exit::RUNTIME,
        }
    }
}

/// `"kind"` of a serde-`snake_case` unit enum value.
fn snake_case_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => "unknown".to_owned(),
    }
}

/// `err: cause: cause ...`.
pub fn error_chain(err: &dyn StdError) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.ends_with(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

/// Host paths in JSON are strings; non-UTF-8 bytes (only possible in paths
/// texrun did not reject up front) are replaced with U+FFFD.
pub fn host_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl CompileReport {
    /// The process exit code. `signal` is the first termination signal
    /// received, if any: an interrupted run exits with `128 + signal` even if
    /// the compile finished (e.g. Ctrl-C while previews were rendered).
    pub fn exit_code(&self, signal: Option<i32>) -> u8 {
        if let Some(error) = &self.error {
            return error.exit_code();
        }
        if let Some(code) = signal.and_then(|s| u8::try_from(128 + s).ok()) {
            return code;
        }
        match self.result.as_ref().map(|r| r.outcome) {
            Some(CompileOutcome::Succeeded) => exit::SUCCESS,
            Some(CompileOutcome::TimedOut) => exit::TIMED_OUT,
            Some(CompileOutcome::Cancelled) => exit::CANCELLED,
            // Failed, and any outcome added later.
            Some(_) => exit::COMPILE_FAILED,
            None => exit::RUNTIME,
        }
    }

    pub fn exit(&self, signal: Option<i32>) -> ExitCode {
        ExitCode::from(self.exit_code(signal))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use texrun_core::EngineInfo;
    use texrun_core::schema::Versioned;

    use super::*;

    fn with_outcome(outcome: CompileOutcome) -> CompileReport {
        CompileReport {
            result: Some(CompileResult::new(
                outcome,
                EngineInfo::new("texlive"),
                Duration::from_millis(5),
            )),
            ..CompileReport::default()
        }
    }

    #[test]
    fn exit_codes() {
        assert_eq!(with_outcome(CompileOutcome::Succeeded).exit_code(None), 0);
        assert_eq!(with_outcome(CompileOutcome::Failed).exit_code(None), 1);
        assert_eq!(with_outcome(CompileOutcome::TimedOut).exit_code(None), 4);
        assert_eq!(with_outcome(CompileOutcome::Cancelled).exit_code(None), 130);
        assert_eq!(
            with_outcome(CompileOutcome::Cancelled).exit_code(Some(15)),
            143
        );
        assert_eq!(
            with_outcome(CompileOutcome::Succeeded).exit_code(Some(2)),
            130
        );
        let mut report = with_outcome(CompileOutcome::Succeeded);
        report.error = Some(ErrorInfo::new(
            Stage::Collect,
            "io",
            Category::Runtime,
            "boom",
        ));
        assert_eq!(report.exit_code(None), 3);
        report.error = Some(ErrorInfo::new(
            Stage::Project,
            "entrypoint_not_found",
            Category::Input,
            "missing",
        ));
        assert_eq!(report.exit_code(None), 2);
    }

    #[test]
    fn engine_error_kinds_map_to_codes() {
        let err = EngineError::Unavailable {
            engine: "texlive".to_owned(),
            reason: "not found".to_owned(),
        };
        let info = ErrorInfo::from_engine(Stage::Probe, &err);
        assert_eq!(info.kind, "unavailable");
        assert_eq!(info.exit_code(), 3);
        let info =
            ErrorInfo::from_engine(Stage::Compile, &EngineError::InvalidRequest("x".to_owned()));
        assert_eq!(info.kind, "invalid_request");
        assert_eq!(info.exit_code(), 2);
    }

    #[test]
    fn error_chain_includes_sources_once() {
        let err = EngineError::Spawn {
            program: "latexmk".to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        assert_eq!(error_chain(&err), "failed to start `latexmk`: no such file");
    }

    #[test]
    fn error_only_document_shape() {
        let report = CompileReport {
            error: Some(ErrorInfo::new(
                Stage::Probe,
                "unavailable",
                Category::Runtime,
                "no latexmk",
            )),
            ..CompileReport::default()
        };
        assert_eq!(
            serde_json::to_value(Versioned::new(report)).unwrap(),
            serde_json::json!({
                "schema_version": 1,
                "error": {
                    "stage": "probe",
                    "kind": "unavailable",
                    "category": "runtime",
                    "message": "no latexmk"
                }
            })
        );
    }

    #[test]
    fn result_fields_are_flattened() {
        let json =
            serde_json::to_value(Versioned::new(with_outcome(CompileOutcome::Failed))).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["outcome"], "failed");
        assert_eq!(json["engine"]["name"], "texlive");
        assert_eq!(json["elapsed_ms"], 5);
        assert!(json.get("error").is_none());
    }
}
