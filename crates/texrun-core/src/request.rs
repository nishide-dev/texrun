//! Compile requests.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::path::WorkspacePath;
use crate::schema::duration_ms;

/// A request to compile one document inside a prepared workspace.
///
/// The request only refers to files by [`WorkspacePath`]; which host directory
/// the workspace lives in is supplied separately to
/// [`TypesetEngine::compile`](crate::TypesetEngine::compile).
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
}

/// Backend-independent compile options.
///
/// Only options that every engine can reasonably honour (or explicitly reject
/// with [`EngineError::Unsupported`](crate::EngineError::Unsupported)) belong
/// here. Backend-specific knobs such as latexmk flags are configured on the
/// engine itself, not on the request.
///
/// `#[non_exhaustive]`: construct with [`CompileOptions::default`] and the
/// `with_*` methods so new options can be added without breaking callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompileOptions {
    /// Wall-clock limit for the whole compile. `None` means the caller applies
    /// no limit (policy defaults are decided in #9 and filled in by the caller).
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
    /// Directory (inside the workspace) where outputs are written. `None`
    /// lets the engine choose its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<WorkspacePath>,
}

impl CompileOptions {
    /// Sets the wall-clock timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets the output directory.
    #[must_use]
    pub fn with_output_dir(mut self, dir: WorkspacePath) -> Self {
        self.output_dir = Some(dir);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wp(s: &str) -> WorkspacePath {
        WorkspacePath::new(s).unwrap()
    }

    #[test]
    fn default_request_serializes_minimally() {
        let req = CompileRequest::new(wp("main.tex"));
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({ "entrypoint": "main.tex", "options": {} })
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
                .with_output_dir(wp("out")),
        );
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(
            json,
            json!({
                "entrypoint": "src/main.tex",
                "options": { "timeout_ms": 30_500, "output_dir": "out" }
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
}
