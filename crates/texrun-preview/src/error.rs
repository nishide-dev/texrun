//! Errors of the preview API itself.

/// A preview run that could not even start because the caller passed bad
/// arguments.
///
/// Everything that can go wrong *during* a run (tool missing, unreadable PDF,
/// tool failure, limits, timeout) is reported as a
/// [`PreviewNotice`](crate::PreviewNotice) in the
/// [`PreviewReport`](crate::PreviewReport) instead, so that a preview problem
/// never turns a successful compile into a failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PreviewError {
    /// [`PreviewOptions::validate`](crate::PreviewOptions::validate) failed.
    #[error("invalid preview options: {0}")]
    InvalidOptions(String),
}
