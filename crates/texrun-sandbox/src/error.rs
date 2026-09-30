//! Errors of the container sandbox.

/// The sandbox cannot be used, or a runtime operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// No usable container runtime or image: not installed, the daemon is
    /// not reachable, too old, or the image does not exist.
    #[error("{0}")]
    Unavailable(String),
    /// The container cannot be set up as requested (e.g. a mount path with
    /// a character the runtime's option syntax cannot carry).
    #[error("{0}")]
    Invalid(String),
    /// A runtime command failed.
    #[error("`{command}` failed: {message}")]
    Runtime {
        /// The runtime subcommand, e.g. `docker create`.
        command: String,
        /// What went wrong (the runtime's message, if any).
        message: String,
    },
}
