//! Errors of a supervised run.

use std::io;

/// The program could not be started, limited or waited for.
///
/// A program that runs and fails is not an error: its exit status is in
/// [`Finished::status`](crate::Finished::status).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// Spawning `program` failed (e.g. it does not exist).
    #[error("cannot start {program}: {source}")]
    Spawn {
        /// The program as given in the [`Spec`](crate::Spec).
        program: String,
        /// The error of the spawn.
        source: io::Error,
    },
    /// Another operation on the child failed (preparing its working
    /// directory, setting a limit, waiting for it).
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
}

impl RunError {
    pub(crate) fn io(context: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}
