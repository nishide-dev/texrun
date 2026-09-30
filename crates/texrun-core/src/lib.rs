//! Core library for texrun.
//!
//! This crate defines the backend-independent compile domain model and the
//! [`TypesetEngine`] interface. It knows nothing about TeX Live, latexmk or
//! any other concrete backend, and nothing about how results are displayed.
//!
//! Flow: a caller prepares a workspace directory ([`WorkspaceRoot`]), builds a
//! [`CompileRequest`] whose paths are [`WorkspacePath`]s relative to it, and
//! passes both (the root via a [`CompileContext`]) to
//! [`TypesetEngine::compile`]:
//!
//! - `Ok(CompileResult)` for every completed attempt, with a
//!   [`CompileOutcome`] of `Succeeded`, `Failed` (document errors),
//!   `TimedOut` or `Cancelled`, plus diagnostics and artifacts;
//! - `Err(EngineError)` only when the attempt could not be carried out
//!   (engine missing, cannot spawn, I/O failure).
//!
//! See [`schema`] for JSON serialization conventions and versioning.

mod artifact;
mod context;
mod diagnostic;
mod engine;
mod path;
mod request;
mod result;
pub mod schema;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;

pub use artifact::{Artifact, ArtifactKind};
pub use context::{CancelToken, CompileContext, PathMapping, WorkspaceRoot, WorkspaceRootError};
pub use diagnostic::{Diagnostic, DiagnosticKind, Severity};
pub use engine::{EngineError, EngineErrorKind, EngineInfo, TypesetEngine};
pub use path::{WorkspacePath, WorkspacePathError};
pub use request::{CompileOptions, CompileRequest};
pub use result::{CompileOutcome, CompileResult, ProcessExit, ResourceLimits};

/// The version of the texrun core library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::testing::{FakeBehavior, FakeEngine};
    use super::*;

    fn wp(s: &str) -> WorkspacePath {
        WorkspacePath::new(s).unwrap()
    }

    fn root() -> WorkspaceRoot {
        WorkspaceRoot::new(std::env::temp_dir().join("texrun-ws")).unwrap()
    }

    #[test]
    fn version_matches_package_version() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn successful_compile_yields_pdf_and_log_relative_to_output_root() {
        let engine = FakeEngine::new(FakeBehavior::Succeed);
        let root = root();
        let request = CompileRequest::new(wp("src/paper.tex"));
        let result = engine
            .compile(&CompileContext::new(&root), &request)
            .unwrap();

        assert!(result.is_success());
        assert_eq!(result.engine.name, FakeEngine::NAME);
        assert_eq!(result.pdf().unwrap().path.as_str(), "paper.pdf");
        assert_eq!(result.log().unwrap().path.as_str(), "paper.log");
        assert!(result.diagnostics.is_empty());
        assert_eq!(engine.calls(), vec![(root.path().to_path_buf(), request)]);
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
        let root = root();
        let result = engine
            .compile(
                &CompileContext::new(&root),
                &CompileRequest::new(wp("main.tex")),
            )
            .unwrap();

        assert_eq!(result.outcome, CompileOutcome::Failed);
        assert!(!result.is_success());
        assert!(result.pdf().is_none());
        assert!(result.log().is_some());
        assert_eq!(result.errors().collect::<Vec<_>>(), vec![&diag]);
    }

    #[test]
    fn timeout_is_an_outcome_with_partial_output() {
        let partial = Diagnostic::new(Severity::Warning, DiagnosticKind::OverfullBox, "Overfull");
        let engine = FakeEngine::new(FakeBehavior::TimeOut(vec![partial.clone()]));
        let root = root();
        let request = CompileRequest::new(wp("main.tex"))
            .with_options(CompileOptions::default().with_timeout(Duration::from_secs(5)));
        let result = engine
            .compile(&CompileContext::new(&root), &request)
            .unwrap();

        // Timeout is reported through `Ok`, and partial output survives.
        assert_eq!(result.outcome, CompileOutcome::TimedOut);
        assert!(!result.is_success());
        assert_eq!(result.warnings().collect::<Vec<_>>(), vec![&partial]);
        assert!(result.log().is_some());
    }

    #[test]
    fn cancellation_is_an_outcome() {
        let engine = FakeEngine::new(FakeBehavior::Succeed);
        let root = root();
        let cancel = CancelToken::new();
        let ctx = CompileContext::new(&root).with_cancel(cancel.clone());
        cancel.cancel();
        let result = engine
            .compile(&ctx, &CompileRequest::new(wp("main.tex")))
            .unwrap();
        assert_eq!(result.outcome, CompileOutcome::Cancelled);
        assert!(result.pdf().is_none());
    }

    #[test]
    fn unavailable_engine_is_an_error() {
        let engine = FakeEngine::new(FakeBehavior::Unavailable);
        let root = root();
        let err = engine
            .compile(
                &CompileContext::new(&root),
                &CompileRequest::new(wp("main.tex")),
            )
            .unwrap_err();
        assert_eq!(err.kind(), EngineErrorKind::Unavailable);
        assert_eq!(
            engine.probe().unwrap_err().kind(),
            EngineErrorKind::Unavailable
        );
    }

    #[test]
    fn invalid_request_is_rejected_before_running() {
        let engine = FakeEngine::new(FakeBehavior::Succeed);
        let root = root();
        let request = CompileRequest::new(wp("main.tex"))
            .with_options(CompileOptions::default().with_timeout(Duration::ZERO));
        let err = engine
            .compile(&CompileContext::new(&root), &request)
            .unwrap_err();
        assert_eq!(err.kind(), EngineErrorKind::InvalidRequest);
    }

    #[test]
    fn custom_behavior_can_return_any_result_or_error() {
        let spawn_failure = FakeEngine::new(FakeBehavior::custom(|_, _| {
            Err(EngineError::Spawn {
                program: "latexmk".to_owned(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            })
        }));
        let root = root();
        let ctx = CompileContext::new(&root);
        let request = CompileRequest::new(wp("main.tex"));
        assert_eq!(
            spawn_failure.compile(&ctx, &request).unwrap_err().kind(),
            EngineErrorKind::Spawn
        );

        let with_preview = FakeEngine::new(FakeBehavior::custom(|ctx, req| {
            assert!(ctx.workspace.path().is_absolute());
            let mut r = CompileResult::new(
                CompileOutcome::Succeeded,
                EngineInfo::new(FakeEngine::NAME),
                Duration::from_millis(1),
            );
            r.artifacts.push(
                Artifact::new(ArtifactKind::Preview, FakeEngine::output_file(req, "png"))
                    .with_page(1),
            );
            Ok(r)
        }));
        let result = with_preview.compile(&ctx, &request).unwrap();
        assert_eq!(result.artifacts_of(ArtifactKind::Preview).count(), 1);
        assert_eq!(with_preview.calls().len(), 1);
    }

    #[test]
    fn engine_is_object_safe_and_shareable() {
        let engine: Arc<dyn TypesetEngine> = Arc::new(FakeEngine::new(FakeBehavior::Succeed));
        let handle = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || {
                let root = root();
                engine
                    .compile(
                        &CompileContext::new(&root),
                        &CompileRequest::new(wp("main.tex")),
                    )
                    .unwrap()
                    .outcome
            })
        };
        assert_eq!(handle.join().unwrap(), CompileOutcome::Succeeded);
        assert_eq!(
            engine.probe().unwrap().version.as_deref(),
            Some(FakeEngine::VERSION)
        );
    }

    #[test]
    fn versioned_result_json_round_trips() {
        let engine = FakeEngine::new(FakeBehavior::Succeed);
        let root = root();
        let result = engine
            .compile(
                &CompileContext::new(&root),
                &CompileRequest::new(wp("main.tex")),
            )
            .unwrap();
        let doc = schema::Versioned::new(result);
        let json = serde_json::to_value(&doc).unwrap();

        assert_eq!(json["schema_version"], schema::SCHEMA_VERSION);
        assert_eq!(json["outcome"], "succeeded");
        assert_eq!(json["elapsed_ms"], 42);
        assert_eq!(json["artifacts"][0]["path"], "main.pdf");
        let back: schema::Versioned<CompileResult> = serde_json::from_value(json).unwrap();
        assert_eq!(back, doc);
    }
}
