//! The result of `texrun compile`: its JSON document and exit code.
//!
//! JSON (`--json`) is `Versioned<CompileReport>`:
//!
//! ```text
//! {
//!   "schema_version": 1,
//!   // Exit code of the texrun process itself (see `exit`):
//!   "texrun_exit_code": 0,
//!   // Present when the compile ran to an outcome (the fields of CompileResult):
//!   "outcome": "succeeded" | "failed" | "timed_out" | "cancelled",
//!   "engine": { "name": "texlive", "version": "latexmk 4.86" },
//!   // How the latexmk *process* ended (not texrun's exit code):
//!   "exit": { "code": 0 } | { "signal": 9 },
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
//!   // Produced but not copied, when copying stopped with an error (paths as in `artifacts`):
//!   "artifacts_not_copied": [ { "kind": "pdf", "path": "main.pdf" } ],
//!   // Advice and warnings about the run (not document diagnostics):
//!   "notes": [ { "severity": "warning" | "info", "kind": "parent_directory_input", "message": "..." } ],
//!   // Once the entrypoint was resolved; diagnostic files are relative to `root`:
//!   "project": { "root": "/abs/project", "entrypoint": "main.tex" },
//!   // Once the workspace was created:
//!   "workspace": { "excluded": [ { "path": "latexmkrc", "reason": "tool_config" } ],
//!                  "excluded_total": 1, "vanished": 0, "kept_path"?: "/tmp/texrun-ws-..." },
//!   // Present when texrun could not finish (exit code 2 or 3):
//!   "error": { "stage": "probe", "kind": "unavailable", "category": "runtime",
//!              "message": "...", "hint"?: "..." }
//! }
//! ```
//!
//! Consumers check `error` first, then `outcome`. `error` may appear together
//! with the compile fields (e.g. the compile ran, but copying its artifacts
//! failed). New optional fields and enum values may be added without a
//! schema version bump.

use std::error::Error as StdError;
use std::path::Path;

use serde::Serialize;
use texrun_core::{Artifact, CompileOutcome, CompileResult, EngineError, Severity};
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

/// Error kinds produced by the CLI itself (the other kinds come from
/// `EngineErrorKind` and `WorkspaceErrorKind`). Listed in the README.
pub mod kind {
    /// Invalid command line (clap).
    pub const USAGE: &str = "usage";
    /// `--pages` / `--preview-dpi` / `--preview-backend` rejected.
    pub const INVALID_PREVIEW_OPTIONS: &str = "invalid_preview_options";
    /// A path argument is not valid UTF-8.
    pub const NON_UTF8_PATH: &str = "non_utf8_path";
    /// The project root would be `/`, `$HOME` or a temporary directory.
    pub const UNSAFE_ROOT: &str = "unsafe_root";
    /// The output directory crosses a symlink inside the project, or a
    /// non-directory is in the way.
    pub const UNSAFE_OUTPUT_PATH: &str = "unsafe_output_path";
    /// I/O error while preparing the output directory.
    pub const IO: &str = "io";
    /// Signal handlers could not be installed.
    pub const SIGNAL_SETUP: &str = "signal_setup";
    /// Something required is not available on this host (`--cgroup
    /// required` without a usable cgroup); the same code as
    /// `EngineErrorKind::Unsupported`.
    pub const UNSUPPORTED: &str = "unsupported";
}

/// The payload of the `--json` document (wrapped in `Versioned`).
#[derive(Debug, Default, Serialize)]
pub struct CompileReport {
    /// The exit code of the texrun process, filled in just before the
    /// report is printed. Not to be confused with `exit`, the latexmk
    /// process status inside the flattened result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub texrun_exit_code: Option<u8>,
    /// The compile result, when the engine ran to an outcome. Artifact
    /// paths are relative to [`CompileReport::output_dir`] once collected.
    #[serde(flatten)]
    pub result: Option<CompileResult>,
    /// Absolute host directory that holds the collected artifacts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    /// Artifacts that were produced but not copied because copying stopped
    /// with an error (paths relative to the output root, like `artifacts`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts_not_copied: Vec<Artifact>,
    /// Advice and warnings about the run itself.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
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

/// Advice or a warning about the run (not a document diagnostic).
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    /// `warning` or `info`.
    pub severity: Severity,
    /// Stable code: `parent_directory_input`, `broad_project_root`,
    /// `output_contains_entrypoint`.
    pub kind: &'static str,
    pub message: String,
    /// Already printed on stderr when it happened (human mode).
    #[serde(skip)]
    pub printed: bool,
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
        ExclusionReason::ExcludedPath => "excluded_path",
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
    /// Checking or creating the output directory.
    Output,
    /// Copying artifacts to the output directory.
    Collect,
    /// Setting up texrun itself (signal handling, `--cgroup required`).
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
    /// `collect`) or one of [`kind`] (the CLI's own codes).
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
        let info = Self::new(stage, snake_case_name(&kind), category, error_chain(err));
        match err {
            // The runtime created the container without a limit or
            // restriction texrun asked for (docs/security.md §2, §4).
            EngineError::Unavailable { engine, reason }
                if engine == texrun_texlive::CONTAINER_ENGINE_NAME
                    && reason.contains(texrun_texlive::RESTRICTIONS_NOT_APPLIED) =>
            {
                info.with_hint(
                    "the container runtime cannot enforce every limit of --backend container on \
                     this host (e.g. cgroup v1 without swap accounting drops the memory+swap \
                     limit); use a host with cgroup v2, or enable swap accounting \
                     (docs/security.md §2)",
                )
            }
            EngineError::Unavailable { engine, .. }
                if engine == texrun_texlive::CONTAINER_ENGINE_NAME =>
            {
                info.with_hint(format!(
                    "--backend container needs Docker (or Podman) and the engine image, which \
                     texrun never pulls: pull the image of this version with `docker pull {}` \
                     (or build it from docker/engine in the texrun repository), or pass \
                     --container-image",
                    texrun_texlive::DEFAULT_CONTAINER_IMAGE
                ))
            }
            EngineError::Unavailable { .. } => info.with_hint(
                "install TeX Live with latexmk and make sure `latexmk` is on PATH, or use the \
                 Docker development environment (docs/development.md), or --backend container",
            ),
            _ => info,
        }
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
    fn readme_lists_the_cli_error_kinds() {
        let readme = include_str!("../../../README.md");
        for code in [
            kind::USAGE,
            kind::INVALID_PREVIEW_OPTIONS,
            kind::NON_UTF8_PATH,
            kind::UNSAFE_ROOT,
            kind::UNSAFE_OUTPUT_PATH,
            kind::IO,
            kind::SIGNAL_SETUP,
            "parent_directory_input",
            "broad_project_root",
            "output_contains_entrypoint",
        ] {
            assert!(
                readme.contains(&format!("`{code}`")),
                "{code} is not in the README"
            );
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
    fn a_container_without_its_limits_gets_its_own_hint() {
        let unavailable = |reason: &str| EngineError::Unavailable {
            engine: texrun_texlive::CONTAINER_ENGINE_NAME.to_owned(),
            reason: reason.to_owned(),
        };
        let refused = ErrorInfo::from_engine(
            Stage::Probe,
            &unavailable(&format!(
                "docker {}: memory+swap limit Some(-1) instead of 4096",
                texrun_texlive::RESTRICTIONS_NOT_APPLIED
            )),
        );
        let hint = refused.hint.unwrap();
        assert!(hint.contains("swap accounting"), "{hint}");
        let missing = ErrorInfo::from_engine(Stage::Probe, &unavailable("no image"));
        assert!(missing.hint.unwrap().contains("docker pull"));
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
