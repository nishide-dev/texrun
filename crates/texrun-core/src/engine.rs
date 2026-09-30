//! The typesetting engine interface.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::request::CompileRequest;
use crate::result::CompileResult;

/// Identifies the engine that produced a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EngineInfo {
    /// Stable backend identifier, e.g. `"texlive"`.
    pub name: String,
    /// Backend version string, if detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl EngineInfo {
    /// Creates engine info without a version.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: None,
        }
    }

    /// Sets the version string.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }
}

/// A failure to *run* a compile, as opposed to a document that failed to
/// compile.
///
/// A document error (the engine ran and reported errors) or a timeout is a
/// [`CompileResult`] with outcome
/// [`Failed`](crate::CompileOutcome::Failed) /
/// [`TimedOut`](crate::CompileOutcome::TimedOut). An `EngineError` means no
/// meaningful result could be produced at all.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// The engine is not installed or not usable on this host
    /// (e.g. the executable was not found).
    #[error("engine `{engine}` is unavailable: {reason}")]
    Unavailable {
        /// Engine identifier.
        engine: String,
        /// Why it is unavailable.
        reason: String,
    },
    /// The engine process could not be started.
    #[error("failed to start `{program}`")]
    Spawn {
        /// Program that failed to start.
        program: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// An I/O error while driving the engine (reading output, collecting
    /// artifacts, stopping the process, ...).
    #[error("I/O error while {context}")]
    Io {
        /// What was being done, e.g. `"reading the log file"`.
        context: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The request is not valid for this workspace (e.g. the entrypoint does
    /// not exist).
    #[error("invalid compile request: {0}")]
    InvalidRequest(String),
    /// The request uses an option this engine does not support.
    #[error("unsupported by this engine: {0}")]
    Unsupported(String),
}

impl EngineError {
    /// The error's classification.
    pub fn kind(&self) -> EngineErrorKind {
        match self {
            Self::Unavailable { .. } => EngineErrorKind::Unavailable,
            Self::Spawn { .. } => EngineErrorKind::Spawn,
            Self::Io { .. } => EngineErrorKind::Io,
            Self::InvalidRequest(_) => EngineErrorKind::InvalidRequest,
            Self::Unsupported(_) => EngineErrorKind::Unsupported,
        }
    }
}

/// Serializable classification of an [`EngineError`], for JSON error objects
/// and exit-code mapping. `EngineError` itself is not serializable because it
/// carries `io::Error` sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EngineErrorKind {
    /// See [`EngineError::Unavailable`].
    Unavailable,
    /// See [`EngineError::Spawn`].
    Spawn,
    /// See [`EngineError::Io`].
    Io,
    /// See [`EngineError::InvalidRequest`].
    InvalidRequest,
    /// See [`EngineError::Unsupported`].
    Unsupported,
}

/// A typesetting backend (TeX Live + latexmk, and later others).
///
/// The API is synchronous: an engine blocks the calling thread until the
/// compile finishes or times out. Async callers (a future daemon) can run it
/// on a blocking thread pool; the `Send + Sync` bound makes an engine
/// shareable across such threads. The trait is object safe
/// (`Box<dyn TypesetEngine>`).
pub trait TypesetEngine: Send + Sync {
    /// Static information about this engine. Must be cheap; must not spawn
    /// processes.
    fn info(&self) -> EngineInfo;

    /// Checks that the engine can run on this host and returns its info,
    /// including the detected version where possible. May spawn processes.
    ///
    /// The default implementation assumes availability and returns
    /// [`TypesetEngine::info`].
    fn probe(&self) -> Result<EngineInfo, EngineError> {
        Ok(self.info())
    }

    /// Compiles `request` inside `workspace`.
    ///
    /// `workspace` is the host directory of a workspace prepared by the
    /// caller; every path in `request` and in the returned result is relative
    /// to it. The engine must honour
    /// [`CompileOptions::timeout`](crate::CompileOptions::timeout) and report
    /// expiry as [`CompileOutcome::TimedOut`](crate::CompileOutcome::TimedOut).
    ///
    /// Returns `Ok` for every completed attempt — including document errors
    /// and timeouts — and `Err` only when the attempt could not be carried out.
    fn compile(
        &self,
        workspace: &Path,
        request: &CompileRequest,
    ) -> Result<CompileResult, EngineError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn error_kinds_classify_every_variant() {
        let io = || io::Error::new(io::ErrorKind::NotFound, "nope");
        let cases = [
            (
                EngineError::Unavailable {
                    engine: "x".into(),
                    reason: "latexmk not found".into(),
                },
                EngineErrorKind::Unavailable,
            ),
            (
                EngineError::Spawn {
                    program: "latexmk".into(),
                    source: io(),
                },
                EngineErrorKind::Spawn,
            ),
            (
                EngineError::Io {
                    context: "reading the log".into(),
                    source: io(),
                },
                EngineErrorKind::Io,
            ),
            (
                EngineError::InvalidRequest("missing entrypoint".into()),
                EngineErrorKind::InvalidRequest,
            ),
            (
                EngineError::Unsupported("output_dir".into()),
                EngineErrorKind::Unsupported,
            ),
        ];
        for (err, kind) in cases {
            assert_eq!(err.kind(), kind, "{err}");
        }
    }

    #[test]
    fn io_errors_are_exposed_as_source() {
        let err = EngineError::Spawn {
            program: "latexmk".into(),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        };
        assert_eq!(err.to_string(), "failed to start `latexmk`");
        assert_eq!(err.source().unwrap().to_string(), "denied");
    }

    #[test]
    fn error_kind_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_value(EngineErrorKind::InvalidRequest).unwrap(),
            serde_json::json!("invalid_request")
        );
    }

    #[test]
    fn engine_info_omits_missing_version() {
        assert_eq!(
            serde_json::to_value(EngineInfo::new("texlive")).unwrap(),
            serde_json::json!({ "name": "texlive" })
        );
    }
}
