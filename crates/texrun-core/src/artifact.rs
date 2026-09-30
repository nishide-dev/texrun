//! Files produced by a compile.

use serde::{Deserialize, Serialize};

use crate::path::WorkspacePath;

/// What an artifact is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ArtifactKind {
    /// The typeset PDF.
    Pdf,
    /// A rendered page image (see [`Artifact::page`]).
    Preview,
    /// The engine's main log file (the raw log behind the diagnostics).
    Log,
    /// Captured console output (stdout / stderr) of the engine process.
    Transcript,
    /// Anything else worth returning (e.g. a `SyncTeX` file).
    Other,
}

/// A file produced by a compile, located inside the workspace.
///
/// `#[non_exhaustive]`: construct with [`Artifact::new`] and the `with_*`
/// methods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Artifact {
    /// What the file is.
    pub kind: ArtifactKind,
    /// Location relative to the workspace root.
    pub path: WorkspacePath,
    /// 1-based page number, for per-page artifacts such as previews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    /// Size in bytes, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

impl Artifact {
    /// Creates an artifact.
    pub fn new(kind: ArtifactKind, path: WorkspacePath) -> Self {
        Self {
            kind,
            path,
            page: None,
            size_bytes: None,
        }
    }

    /// Sets the 1-based page number.
    #[must_use]
    pub fn with_page(mut self, page: u32) -> Self {
        self.page = Some(page);
        self
    }

    /// Sets the size in bytes.
    #[must_use]
    pub fn with_size_bytes(mut self, size: u64) -> Self {
        self.size_bytes = Some(size);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_shape_and_round_trip() {
        let a = Artifact::new(
            ArtifactKind::Preview,
            WorkspacePath::new("out/preview/page-1.png").unwrap(),
        )
        .with_page(1)
        .with_size_bytes(2048);
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(
            json,
            json!({
                "kind": "preview",
                "path": "out/preview/page-1.png",
                "page": 1,
                "size_bytes": 2048
            })
        );
        assert_eq!(serde_json::from_value::<Artifact>(json).unwrap(), a);
    }

    #[test]
    fn rejects_paths_outside_workspace_on_deserialize() {
        assert!(
            serde_json::from_value::<Artifact>(json!({ "kind": "pdf", "path": "/tmp/x.pdf" }))
                .is_err()
        );
    }
}
