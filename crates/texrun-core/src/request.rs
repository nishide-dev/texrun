//! Compile requests.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::engine::EngineError;
use crate::path::WorkspacePath;
use crate::schema::duration_ms;

/// A request to compile one document inside a prepared workspace.
///
/// The request only refers to files by [`WorkspacePath`] relative to the
/// workspace root (the input project root). Which host directory that is, is
/// supplied separately in the [`CompileContext`](crate::CompileContext).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompileRequest {
    /// The main source file, e.g. `main.tex`.
    pub entrypoint: WorkspacePath,
    /// Backend-independent options.
    #[serde(default)]
    pub options: CompileOptions,
}

impl CompileRequest {
    /// Creates a request with default options.
    pub fn new(entrypoint: WorkspacePath) -> Self {
        Self {
            entrypoint,
            options: CompileOptions::default(),
        }
    }

    /// Replaces the options.
    #[must_use]
    pub fn with_options(mut self, options: CompileOptions) -> Self {
        self.options = options;
        self
    }

    /// Checks backend-independent invariants that the type system does not
    /// enforce. Engines call this at the start of
    /// [`TypesetEngine::compile`](crate::TypesetEngine::compile).
    ///
    /// Currently rejects a zero timeout with [`EngineError::InvalidRequest`].
    pub fn validate(&self) -> Result<(), EngineError> {
        if self.options.timeout == Some(Duration::ZERO) {
            return Err(EngineError::InvalidRequest(
                "timeout must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

fn default_output_dir() -> WorkspacePath {
    WorkspacePath::new(CompileOptions::DEFAULT_OUTPUT_DIR).expect("valid default output dir")
}

/// Backend-independent compile options.
///
/// Only options that every engine can reasonably honour (or explicitly reject
/// with [`EngineError::Unsupported`]) belong here. Backend-specific knobs such
/// as latexmk flags are configured on the engine itself, not on the request.
///
/// `#[non_exhaustive]`: construct with [`CompileOptions::default`] and the
/// `with_*` methods so new options can be added without breaking callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompileOptions {
    /// Wall-clock limit for the whole compile. `None` means no limit at this
    /// level (the policy default is decided in #9 and filled in by the
    /// caller). `Some(Duration::ZERO)` is invalid and rejected by
    /// [`CompileRequest::validate`].
    ///
    /// Exceeding it yields a [`CompileOutcome::TimedOut`](crate::CompileOutcome::TimedOut)
    /// result, not an error.
    #[serde(
        rename = "timeout_ms",
        with = "duration_ms::option",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
    /// Directory **inside the workspace** where the engine writes its outputs
    /// (the *output root*). Defaults to [`CompileOptions::DEFAULT_OUTPUT_DIR`].
    ///
    /// This is not the CLI's `--output <dir>` (a host path, #6): the workspace
    /// layer (#4) collects the contents of this directory to the host output
    /// location, preserving relative paths. It is kept separate from the input
    /// files, so the workspace layer must not copy a same-named directory from
    /// the input project.
    #[serde(default = "default_output_dir")]
    pub output_dir: WorkspacePath,
}

impl CompileOptions {
    /// Default [`CompileOptions::output_dir`].
    pub const DEFAULT_OUTPUT_DIR: &'static str = ".texrun/out";

    /// Sets the wall-clock timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets the output directory (inside the workspace).
    #[must_use]
    pub fn with_output_dir(mut self, dir: WorkspacePath) -> Self {
        self.output_dir = dir;
        self
    }
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            timeout: None,
            output_dir: default_output_dir(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineErrorKind;
    use serde_json::json;

    fn wp(s: &str) -> WorkspacePath {
        WorkspacePath::new(s).unwrap()
    }

    #[test]
    fn default_request_json_shape() {
        let req = CompileRequest::new(wp("main.tex"));
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({ "entrypoint": "main.tex", "options": { "output_dir": ".texrun/out" } })
        );
        let back: CompileRequest = serde_json::from_value(json!({ "entrypoint": "main.tex" }))
            .expect("options default when omitted");
        assert_eq!(back, req);
    }

    #[test]
    fn options_round_trip_with_millisecond_timeout() {
        let req = CompileRequest::new(wp("src/main.tex")).with_options(
            CompileOptions::default()
                .with_timeout(Duration::from_millis(30_500))
                .with_output_dir(wp("build")),
        );
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(
            json,
            json!({
                "entrypoint": "src/main.tex",
                "options": { "timeout_ms": 30_500, "output_dir": "build" }
            })
        );
        let back: CompileRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn deserialization_rejects_escaping_entrypoint() {
        let err = serde_json::from_value::<CompileRequest>(json!({ "entrypoint": "../x.tex" }));
        assert!(err.is_err());
    }

    #[test]
    fn validate_rejects_zero_timeout() {
        let ok = CompileRequest::new(wp("main.tex"))
            .with_options(CompileOptions::default().with_timeout(Duration::from_millis(1)));
        assert!(ok.validate().is_ok());
        assert!(CompileRequest::new(wp("main.tex")).validate().is_ok());

        let zero = CompileRequest::new(wp("main.tex"))
            .with_options(CompileOptions::default().with_timeout(Duration::ZERO));
        assert_eq!(
            zero.validate().unwrap_err().kind(),
            EngineErrorKind::InvalidRequest
        );
        // `timeout_ms: 0` from JSON is rejected the same way.
        let from_json: CompileRequest = serde_json::from_value(
            json!({ "entrypoint": "main.tex", "options": { "timeout_ms": 0 } }),
        )
        .unwrap();
        assert!(from_json.validate().is_err());
    }
}
