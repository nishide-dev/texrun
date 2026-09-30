//! Test doubles for [`TypesetEngine`].
//!
//! Available in this crate's tests and, for downstream crates, behind the
//! `test-util` feature. Nothing here touches the filesystem or spawns
//! processes.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::artifact::{Artifact, ArtifactKind};
use crate::context::CompileContext;
use crate::diagnostic::Diagnostic;
use crate::engine::{EngineError, EngineInfo, TypesetEngine};
use crate::path::WorkspacePath;
use crate::request::CompileRequest;
use crate::result::{CompileOutcome, CompileResult, ProcessExit};

/// Signature of a [`FakeBehavior::Custom`] handler.
pub type FakeHandler = dyn Fn(&CompileContext<'_>, &CompileRequest) -> Result<CompileResult, EngineError>
    + Send
    + Sync;

/// What a [`FakeEngine`] does when asked to compile.
#[derive(Clone)]
#[non_exhaustive]
pub enum FakeBehavior {
    /// Succeed, producing a PDF and a log.
    Succeed,
    /// Fail with the given diagnostics, producing only a log.
    Fail(Vec<Diagnostic>),
    /// Time out, keeping the given partial diagnostics and the log.
    TimeOut(Vec<Diagnostic>),
    /// Report the engine as unavailable (an [`EngineError`]) from both
    /// `probe` and `compile`.
    Unavailable,
    /// Delegate to a closure that may return any result or error. The fake
    /// still validates the request, honours cancellation and records the call
    /// before invoking it.
    Custom(Arc<FakeHandler>),
}

impl FakeBehavior {
    /// Convenience constructor for [`FakeBehavior::Custom`].
    pub fn custom<F>(handler: F) -> Self
    where
        F: Fn(&CompileContext<'_>, &CompileRequest) -> Result<CompileResult, EngineError>
            + Send
            + Sync
            + 'static,
    {
        Self::Custom(Arc::new(handler))
    }
}

impl fmt::Debug for FakeBehavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Succeed => f.write_str("Succeed"),
            Self::Fail(d) => f.debug_tuple("Fail").field(d).finish(),
            Self::TimeOut(d) => f.debug_tuple("TimeOut").field(d).finish(),
            Self::Unavailable => f.write_str("Unavailable"),
            Self::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

/// A scripted engine used to exercise the request → result flow without TeX.
///
/// Behaves like a well-behaved engine: it validates the request, returns a
/// [`CompileOutcome::Cancelled`] result if the context's cancel token is set,
/// and reports artifacts relative to the output root as
/// `<entrypoint stem>.{pdf,log}`. Every call is recorded and can be inspected
/// with [`FakeEngine::calls`].
#[derive(Debug)]
pub struct FakeEngine {
    behavior: FakeBehavior,
    calls: Mutex<Vec<(PathBuf, CompileRequest)>>,
}

impl FakeEngine {
    /// Engine identifier reported in [`EngineInfo::name`].
    pub const NAME: &'static str = "fake";
    /// Version reported by [`TypesetEngine::probe`].
    pub const VERSION: &'static str = "0.0.0";
    /// Elapsed time reported for results.
    pub const ELAPSED: Duration = Duration::from_millis(42);

    /// Creates an engine with the given behavior.
    pub fn new(behavior: FakeBehavior) -> Self {
        Self {
            behavior,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Every `(workspace root, request)` passed to `compile`, in order.
    pub fn calls(&self) -> Vec<(PathBuf, CompileRequest)> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Output-root-relative path `<entrypoint stem>.<extension>`.
    pub fn output_file(request: &CompileRequest, extension: &str) -> WorkspacePath {
        WorkspacePath::new(&format!("{}.{extension}", request.entrypoint.file_stem()))
            .expect("stem of a valid path is a valid file name")
    }

    fn with_log(
        info: EngineInfo,
        request: &CompileRequest,
        outcome: CompileOutcome,
        exit: ProcessExit,
        diagnostics: &[Diagnostic],
    ) -> CompileResult {
        let mut r = CompileResult::new(outcome, info, Self::ELAPSED);
        r.exit = Some(exit);
        r.diagnostics = diagnostics.to_vec();
        r.artifacts.push(Artifact::new(
            ArtifactKind::Log,
            Self::output_file(request, "log"),
        ));
        r
    }
}

impl TypesetEngine for FakeEngine {
    fn info(&self) -> EngineInfo {
        EngineInfo::new(Self::NAME)
    }

    fn probe(&self) -> Result<EngineInfo, EngineError> {
        match self.behavior {
            FakeBehavior::Unavailable => Err(EngineError::Unavailable {
                engine: Self::NAME.to_owned(),
                reason: "configured as unavailable".to_owned(),
            }),
            _ => Ok(self.info().with_version(Self::VERSION)),
        }
    }

    fn compile(
        &self,
        ctx: &CompileContext<'_>,
        request: &CompileRequest,
    ) -> Result<CompileResult, EngineError> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((ctx.workspace.path().to_path_buf(), request.clone()));

        request.validate()?;
        let info = self.probe()?;
        if ctx.cancel.is_cancelled() {
            return Ok(Self::with_log(
                info,
                request,
                CompileOutcome::Cancelled,
                ProcessExit::with_signal(9),
                &[],
            ));
        }

        Ok(match &self.behavior {
            FakeBehavior::Succeed => {
                let mut r = Self::with_log(
                    info,
                    request,
                    CompileOutcome::Succeeded,
                    ProcessExit::with_code(0),
                    &[],
                );
                r.artifacts.insert(
                    0,
                    Artifact::new(ArtifactKind::Pdf, Self::output_file(request, "pdf")),
                );
                r
            }
            FakeBehavior::Fail(diagnostics) => Self::with_log(
                info,
                request,
                CompileOutcome::Failed,
                ProcessExit::with_code(1),
                diagnostics,
            ),
            FakeBehavior::TimeOut(diagnostics) => Self::with_log(
                info,
                request,
                CompileOutcome::TimedOut,
                ProcessExit::with_signal(9),
                diagnostics,
            ),
            FakeBehavior::Custom(handler) => return handler(ctx, request),
            FakeBehavior::Unavailable => unreachable!("rejected by probe"),
        })
    }
}
