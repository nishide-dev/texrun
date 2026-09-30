//! Core library for texrun.
//!
//! This crate defines the backend-independent compile domain model and the
//! [`TypesetEngine`] interface. It knows nothing about TeX Live, latexmk or
//! any other concrete backend, and nothing about how results are displayed.
//!
//! Flow: a caller prepares a workspace directory, builds a
//! [`CompileRequest`] whose paths are [`WorkspacePath`]s relative to it, and
//! passes both to [`TypesetEngine::compile`]:
//!
//! - `Ok(CompileResult)` for every completed attempt, with a
//!   [`CompileOutcome`] of `Succeeded`, `Failed` (document errors) or
//!   `TimedOut`, plus diagnostics and artifacts;
//! - `Err(EngineError)` only when the attempt could not be carried out
//!   (engine missing, cannot spawn, I/O failure).
//!
//! See [`schema`] for JSON serialization conventions and versioning.

mod artifact;
mod diagnostic;
mod engine;
mod path;
mod request;
mod result;
pub mod schema;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;

pub use artifact::{Artifact, ArtifactKind};
pub use diagnostic::{Diagnostic, DiagnosticKind, Severity};
pub use engine::{EngineError, EngineErrorKind, EngineInfo, TypesetEngine};
pub use path::{WorkspacePath, WorkspacePathError};
pub use request::{CompileOptions, CompileRequest};
pub use result::{CompileOutcome, CompileResult, ProcessExit};

/// The version of the texrun core library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::testing::{FakeBehavior, FakeEngine};
    use super::*;

    fn wp(s: &str) -> WorkspacePath {
        WorkspacePath::new(s).unwrap()
    }

    #[test]
    fn version_matches_package_version() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn successful_compile_yields_pdf_and_log() {
        let engine = FakeEngine::new(FakeBehavior::Succeed);
        let request = CompileRequest::new(wp("src/paper.tex"));
        let result = engine.compile(Path::new("/ws"), &request).unwrap();

        assert!(result.is_success());
        assert_eq!(result.engine.name, FakeEngine::NAME);
        assert_eq!(result.pdf().unwrap().path.as_str(), "out/paper.pdf");
        assert_eq!(result.log().unwrap().path.as_str(), "out/paper.log");
        assert!(result.diagnostics.is_empty());
        assert_eq!(engine.calls(), vec![(Path::new("/ws").into(), request)]);
    }

    #[test]
    fn document_failure_is_a_result_not_an_error() {
        let diag = Diagnostic::new(
            Severity::Error,
            DiagnosticKind::UndefinedControlSequence,
            "Undefined control sequence",
        )
        .with_file(wp("main.tex"))
        .with_line(7);
        let engine = FakeEngine::new(FakeBehavior::Fail(vec![diag.clone()]));
        let request = CompileRequest::new(wp("main.tex"))
            .with_options(CompileOptions::default().with_output_dir(wp("build")));
        let result = engine.compile(Path::new("/ws"), &request).unwrap();

        assert_eq!(result.outcome, CompileOutcome::Failed);
        assert!(result.pdf().is_none());
        assert_eq!(result.log().unwrap().path.as_str(), "build/main.log");
        assert_eq!(result.errors().collect::<Vec<_>>(), vec![&diag]);
    }

    #[test]
    fn timeout_is_an_outcome_with_partial_output() {
        let partial = Diagnostic::new(Severity::Warning, DiagnosticKind::OverfullBox, "Overfull");
        let engine = FakeEngine::new(FakeBehavior::TimeOut(vec![partial]));
        let request = CompileRequest::new(wp("main.tex"))
            .with_options(CompileOptions::default().with_timeout(Duration::from_secs(5)));
        let result = engine.compile(Path::new("/ws"), &request).unwrap();

        assert_eq!(result.outcome, CompileOutcome::TimedOut);
        assert_eq!(result.elapsed, Duration::from_secs(5));
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.log().is_some());
    }

    #[test]
    fn unavailable_engine_is_an_error() {
        let engine = FakeEngine::new(FakeBehavior::Unavailable);
        let err = engine
            .compile(Path::new("/ws"), &CompileRequest::new(wp("main.tex")))
            .unwrap_err();
        assert_eq!(err.kind(), EngineErrorKind::Unavailable);
        assert_eq!(
            engine.probe().unwrap_err().kind(),
            EngineErrorKind::Unavailable
        );
    }

    #[test]
    fn engine_is_object_safe_and_shareable() {
        let engine: std::sync::Arc<dyn TypesetEngine> =
            std::sync::Arc::new(FakeEngine::new(FakeBehavior::Succeed));
        let handle = {
            let engine = std::sync::Arc::clone(&engine);
            std::thread::spawn(move || {
                engine
                    .compile(Path::new("/ws"), &CompileRequest::new(wp("main.tex")))
                    .unwrap()
                    .outcome
            })
        };
        assert_eq!(handle.join().unwrap(), CompileOutcome::Succeeded);
        assert_eq!(engine.probe().unwrap().version.as_deref(), Some("0.0.0"));
    }

    #[test]
    fn versioned_result_json_round_trips() {
        let engine = FakeEngine::new(FakeBehavior::Succeed);
        let result = engine
            .compile(Path::new("/ws"), &CompileRequest::new(wp("main.tex")))
            .unwrap();
        let doc = schema::Versioned::new(result);
        let json = serde_json::to_value(&doc).unwrap();

        assert_eq!(json["schema_version"], schema::SCHEMA_VERSION);
        assert_eq!(json["outcome"], "succeeded");
        assert_eq!(json["elapsed_ms"], 42);
        let back: schema::Versioned<CompileResult> = serde_json::from_value(json).unwrap();
        assert_eq!(back, doc);
    }
}
