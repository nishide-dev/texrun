//! Compile results.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::artifact::{Artifact, ArtifactKind};
use crate::diagnostic::{Diagnostic, Severity};
use crate::engine::EngineInfo;
use crate::schema::duration_ms;

/// How a compile ended.
///
/// Every outcome is a *completed* compile attempt that may carry diagnostics,
/// logs and artifacts. Failures of texrun or the engine infrastructure itself
/// (engine missing, cannot spawn, ...) are reported as
/// [`EngineError`](crate::EngineError) instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompileOutcome {
    /// The document was compiled and the expected output was produced.
    Succeeded,
    /// The engine ran but the document failed to compile.
    Failed,
    /// The compile exceeded its time limit and was stopped. Diagnostics and
    /// artifacts collected up to that point are still reported.
    TimedOut,
}

/// Termination information of the engine process, when the engine is a
/// subprocess. Informational only: callers decide based on
/// [`CompileResult::outcome`], never on raw exit codes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProcessExit {
    /// Exit code, if the process exited normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// Terminating signal number (Unix only), if the process was killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

impl ProcessExit {
    /// A normal exit with `code`.
    pub fn with_code(code: i32) -> Self {
        Self {
            code: Some(code),
            signal: None,
        }
    }

    /// Termination by `signal`.
    pub fn with_signal(signal: i32) -> Self {
        Self {
            code: None,
            signal: Some(signal),
        }
    }
}

impl From<std::process::ExitStatus> for ProcessExit {
    fn from(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
        }
    }
}

/// The result of a completed compile attempt.
///
/// `#[non_exhaustive]`: construct with [`CompileResult::new`], then set or push
/// into the public fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompileResult {
    /// How the compile ended.
    pub outcome: CompileOutcome,
    /// Which engine produced this result.
    pub engine: EngineInfo,
    /// Engine process termination info, if applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ProcessExit>,
    /// Wall-clock time spent in the engine.
    #[serde(rename = "elapsed_ms", with = "duration_ms")]
    pub elapsed: Duration,
    /// Structured diagnostics (possibly empty even on failure, if the log
    /// could not be parsed; the raw log is still available as an artifact).
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    /// Produced files, including the raw log ([`ArtifactKind::Log`]).
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

impl CompileResult {
    /// Creates a result with no exit info, diagnostics or artifacts.
    pub fn new(outcome: CompileOutcome, engine: EngineInfo, elapsed: Duration) -> Self {
        Self {
            outcome,
            engine,
            exit: None,
            elapsed,
            diagnostics: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    /// `true` if the outcome is [`CompileOutcome::Succeeded`].
    pub fn is_success(&self) -> bool {
        self.outcome == CompileOutcome::Succeeded
    }

    /// Artifacts of the given kind, in order.
    pub fn artifacts_of(&self, kind: ArtifactKind) -> impl Iterator<Item = &Artifact> {
        self.artifacts.iter().filter(move |a| a.kind == kind)
    }

    /// The first PDF artifact, if any.
    pub fn pdf(&self) -> Option<&Artifact> {
        self.artifacts_of(ArtifactKind::Pdf).next()
    }

    /// The first log artifact (access to the raw engine log), if any.
    pub fn log(&self) -> Option<&Artifact> {
        self.artifacts_of(ArtifactKind::Log).next()
    }

    /// Diagnostics with [`Severity::Error`].
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::DiagnosticKind;
    use crate::path::WorkspacePath;
    use serde_json::json;

    fn sample() -> CompileResult {
        let mut r = CompileResult::new(
            CompileOutcome::Failed,
            EngineInfo::new("fake").with_version("1.0"),
            Duration::from_millis(1234),
        );
        r.exit = Some(ProcessExit::with_code(12));
        r.diagnostics.push(
            Diagnostic::new(Severity::Error, DiagnosticKind::LatexError, "boom")
                .with_file(WorkspacePath::new("main.tex").unwrap())
                .with_line(3),
        );
        r.diagnostics.push(Diagnostic::new(
            Severity::Warning,
            DiagnosticKind::OverfullBox,
            "Overfull \\hbox",
        ));
        r.artifacts.push(Artifact::new(
            ArtifactKind::Log,
            WorkspacePath::new("out/main.log").unwrap(),
        ));
        r
    }

    #[test]
    fn json_shape() {
        assert_eq!(
            serde_json::to_value(sample()).unwrap(),
            json!({
                "outcome": "failed",
                "engine": { "name": "fake", "version": "1.0" },
                "exit": { "code": 12 },
                "elapsed_ms": 1234,
                "diagnostics": [
                    { "severity": "error", "kind": "latex_error", "message": "boom",
                      "file": "main.tex", "line": 3 },
                    { "severity": "warning", "kind": "overfull_box",
                      "message": "Overfull \\hbox" }
                ],
                "artifacts": [ { "kind": "log", "path": "out/main.log" } ]
            })
        );
    }

    #[test]
    fn round_trip() {
        let r = sample();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<CompileResult>(&json).unwrap(), r);
    }

    #[test]
    fn outcome_is_a_snake_case_string() {
        for (outcome, text) in [
            (CompileOutcome::Succeeded, "succeeded"),
            (CompileOutcome::Failed, "failed"),
            (CompileOutcome::TimedOut, "timed_out"),
        ] {
            assert_eq!(serde_json::to_value(outcome).unwrap(), json!(text));
        }
    }

    #[test]
    fn accessors() {
        let r = sample();
        assert!(!r.is_success());
        assert_eq!(r.log().unwrap().path.as_str(), "out/main.log");
        assert!(r.pdf().is_none());
        assert_eq!(r.errors().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn process_exit_from_exit_status() {
        use std::os::unix::process::ExitStatusExt;
        let exited = ProcessExit::from(std::process::ExitStatus::from_raw(3 << 8));
        assert_eq!(exited, ProcessExit::with_code(3));
        let killed = ProcessExit::from(std::process::ExitStatus::from_raw(9));
        assert_eq!(killed, ProcessExit::with_signal(9));
    }
}
