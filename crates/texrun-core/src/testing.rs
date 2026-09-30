//! Test doubles for [`TypesetEngine`].
//!
//! Available in this crate's tests and, for downstream crates, behind the
//! `test-util` feature. Nothing here touches the filesystem or spawns
//! processes.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::artifact::{Artifact, ArtifactKind};
use crate::diagnostic::Diagnostic;
use crate::engine::{EngineError, EngineInfo, TypesetEngine};
use crate::path::WorkspacePath;
use crate::request::CompileRequest;
use crate::result::{CompileOutcome, CompileResult, ProcessExit};

/// What a [`FakeEngine`] does when asked to compile.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum FakeBehavior {
    /// Succeed, producing a PDF and a log.
    Succeed,
    /// Fail with the given diagnostics, producing only a log.
    Fail(Vec<Diagnostic>),
    /// Time out, keeping the given partial diagnostics and the log.
    TimeOut(Vec<Diagnostic>),
    /// Report the engine as unavailable (an [`EngineError`]).
    Unavailable,
}

/// A scripted engine used to exercise the request → result flow without TeX.
///
/// Outputs are placed at `<output_dir>/<entrypoint stem>.{pdf,log}`, with
/// `output_dir` defaulting to [`FakeEngine::DEFAULT_OUTPUT_DIR`]. Every call is
/// recorded and can be inspected with [`FakeEngine::calls`].
#[derive(Debug)]
pub struct FakeEngine {
    behavior: FakeBehavior,
    calls: Mutex<Vec<(PathBuf, CompileRequest)>>,
}

impl FakeEngine {
    /// Engine identifier reported in [`EngineInfo::name`].
    pub const NAME: &'static str = "fake";
    /// Output directory used when the request does not specify one.
    pub const DEFAULT_OUTPUT_DIR: &'static str = "out";
    /// Elapsed time reported for completed compiles.
    pub const ELAPSED: Duration = Duration::from_millis(42);

    /// Creates an engine with the given behavior.
    pub fn new(behavior: FakeBehavior) -> Self {
        Self {
            behavior,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Every `(workspace, request)` passed to `compile`, in order.
    pub fn calls(&self) -> Vec<(PathBuf, CompileRequest)> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn output_path(request: &CompileRequest, extension: &str) -> WorkspacePath {
        let dir = request.options.output_dir.clone().unwrap_or_else(|| {
            WorkspacePath::new(Self::DEFAULT_OUTPUT_DIR).expect("valid default output dir")
        });
        let file = WorkspacePath::new(&format!("{}.{extension}", request.entrypoint.file_stem()))
            .expect("stem of a valid path is a valid file name");
        dir.join(&file)
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
            _ => Ok(self.info().with_version("0.0.0")),
        }
    }

    fn compile(
        &self,
        workspace: &Path,
        request: &CompileRequest,
    ) -> Result<CompileResult, EngineError> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((workspace.to_path_buf(), request.clone()));

        let info = self.probe()?;
        let log = Artifact::new(ArtifactKind::Log, Self::output_path(request, "log"));

        let result = match &self.behavior {
            FakeBehavior::Succeed => {
                let mut r = CompileResult::new(CompileOutcome::Succeeded, info, Self::ELAPSED);
                r.exit = Some(ProcessExit::with_code(0));
                r.artifacts.push(Artifact::new(
                    ArtifactKind::Pdf,
                    Self::output_path(request, "pdf"),
                ));
                r.artifacts.push(log);
                r
            }
            FakeBehavior::Fail(diagnostics) => {
                let mut r = CompileResult::new(CompileOutcome::Failed, info, Self::ELAPSED);
                r.exit = Some(ProcessExit::with_code(1));
                r.diagnostics.clone_from(diagnostics);
                r.artifacts.push(log);
                r
            }
            FakeBehavior::TimeOut(diagnostics) => {
                let elapsed = request.options.timeout.unwrap_or(Self::ELAPSED);
                let mut r = CompileResult::new(CompileOutcome::TimedOut, info, elapsed);
                r.exit = Some(ProcessExit::with_signal(9));
                r.diagnostics.clone_from(diagnostics);
                r.artifacts.push(log);
                r
            }
            FakeBehavior::Unavailable => unreachable!("rejected by probe"),
        };
        Ok(result)
    }
}
