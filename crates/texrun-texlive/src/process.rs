//! Running latexmk under supervision (docs/security.md §3.2, §3.6).
//!
//! Process group, poll loop, group kill, reaping, output capture and
//! `prlimit(2)` are those of `texrun-process`. This module adds what is
//! specific to latexmk: the size [`Limits`], the output size check and the
//! start gate of the texrun rc ([`crate::rc::RcOptions::stdin_gate`]).

use std::path::Path;
use std::time::Duration;

use texrun_core::{CancelToken, EngineError};
pub use texrun_process::CapturedOutput;
use texrun_process::{
    Capture, Cwd, EnvAllowlist, Resource, Rlimits, RunError, Spec, StartMode, Stop, Watch,
};

use crate::layout;

/// Size limits (docs/security.md §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Maximum size of one file written by the engine. Enforced with
    /// `RLIMIT_FSIZE` on Linux and, on every platform, by the output size
    /// check.
    pub max_file_bytes: u64,
    /// Maximum total size of the output directory (and the engine's `HOME`).
    pub max_output_bytes: u64,
    /// How much of stdout and of stderr is kept (each). The rest is read and
    /// discarded.
    pub max_captured_bytes: usize,
    /// How often the output size is checked while the engine runs.
    pub size_check_interval: Duration,
}

impl Limits {
    /// Default [`Limits::max_file_bytes`]: 256 MiB.
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
    /// Default [`Limits::max_output_bytes`]: 1 GiB.
    pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;
    /// Default [`Limits::max_captured_bytes`]: 4 MiB.
    pub const DEFAULT_MAX_CAPTURED_BYTES: usize = 4 * 1024 * 1024;
    /// Default [`Limits::size_check_interval`]: 500 ms.
    pub const DEFAULT_SIZE_CHECK_INTERVAL: Duration = Duration::from_millis(500);

    /// Sets [`Limits::max_file_bytes`].
    #[must_use]
    pub fn with_max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// Sets [`Limits::max_output_bytes`].
    #[must_use]
    pub fn with_max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Sets [`Limits::max_captured_bytes`].
    #[must_use]
    pub fn with_max_captured_bytes(mut self, bytes: usize) -> Self {
        self.max_captured_bytes = bytes;
        self
    }

    /// Sets [`Limits::size_check_interval`].
    #[must_use]
    pub fn with_size_check_interval(mut self, interval: Duration) -> Self {
        self.size_check_interval = interval;
        self
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            max_output_bytes: Self::DEFAULT_MAX_OUTPUT_BYTES,
            max_captured_bytes: Self::DEFAULT_MAX_CAPTURED_BYTES,
            size_check_interval: Self::DEFAULT_SIZE_CHECK_INTERVAL,
        }
    }
}

/// Why the supervisor stopped latexmk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopReason {
    TimedOut,
    Cancelled,
    /// An output size limit was exceeded; the message says which.
    OutputLimit(String),
}

/// What to run and watch.
pub(crate) struct Job<'a> {
    pub(crate) program: &'a Path,
    pub(crate) args: Vec<std::ffi::OsString>,
    pub(crate) cwd: &'a Path,
    pub(crate) env: EnvAllowlist,
    pub(crate) timeout: Option<Duration>,
    pub(crate) cancel: Option<&'a CancelToken>,
    /// Directories whose size is limited (empty: no size checks).
    pub(crate) size_dirs: Vec<&'a Path>,
    pub(crate) limits: Limits,
    /// Hold latexmk at the start gate of the texrun rc until
    /// `RLIMIT_FSIZE` is set (requires [`FILE_SIZE_GATE_SUPPORTED`] and an
    /// rc rendered with [`crate::rc::RcOptions::stdin_gate`]).
    pub(crate) file_size_gate: bool,
}

/// Result of a supervised run (see [`texrun_process::Finished`]).
#[derive(Debug)]
pub(crate) struct Finished {
    pub(crate) pid: u32,
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stop: Option<StopReason>,
    pub(crate) elapsed: Duration,
    pub(crate) stdout: CapturedOutput,
    pub(crate) stderr: CapturedOutput,
}

/// Runs latexmk and supervises it until it exits or is stopped.
pub(crate) fn run(job: &Job<'_>) -> Result<Finished, EngineError> {
    let cap = job.limits.max_captured_bytes;
    let mut spec = Spec::new(job.program, Cwd::Path(job.cwd))
        .with_args(job.args.iter().cloned())
        .with_env(job.env.clone())
        .with_stdout(Capture::Keep(cap))
        .with_stderr(Capture::Keep(cap));
    if job.file_size_gate {
        spec = spec
            .with_rlimits(
                Rlimits::new()
                    .with(Resource::FileSize, job.limits.max_file_bytes)
                    // Hitting RLIMIT_FSIZE raises SIGXFSZ, whose default
                    // action dumps core into TeX's working directory,
                    // outside the size checks.
                    .with(Resource::Core, 0),
            )
            .with_start(StartMode::StdinGate {
                token: crate::rc::START_TOKEN.to_vec(),
            });
    }

    let mut watch = Watch::new();
    if let Some(timeout) = job.timeout {
        watch = watch.with_timeout(timeout);
    }
    if let Some(cancel) = job.cancel {
        watch = watch.with_cancel(cancel);
    }
    if !job.size_dirs.is_empty() {
        watch = watch.with_check(job.limits.size_check_interval, || {
            check_output_size(&job.size_dirs, &job.limits)
        });
    }

    let finished = texrun_process::run(&spec, watch).map_err(|e| match e {
        RunError::Spawn { program, source } => EngineError::Spawn { program, source },
        RunError::Io { context, source } => EngineError::Io { context, source },
        other => EngineError::Io {
            context: "running latexmk".to_owned(),
            source: std::io::Error::other(other.to_string()),
        },
    })?;
    Ok(Finished {
        pid: finished.pid,
        status: finished.status,
        stop: finished.stop.map(|stop| match stop {
            Stop::TimedOut => StopReason::TimedOut,
            Stop::Cancelled => StopReason::Cancelled,
            Stop::Check(reason) => reason,
        }),
        elapsed: finished.elapsed,
        stdout: finished.stdout,
        stderr: finished.stderr,
    })
}

/// Checks the output size limits once.
pub(crate) fn check_output_size(dirs: &[&Path], limits: &Limits) -> Option<StopReason> {
    let size = layout::tree_size(dirs);
    if size.largest >= limits.max_file_bytes {
        Some(StopReason::OutputLimit(format!(
            "output limit exceeded: a file reached the per-file limit of {} bytes",
            limits.max_file_bytes
        )))
    } else if size.total > limits.max_output_bytes {
        Some(StopReason::OutputLimit(format!(
            "output limit exceeded: the output directory exceeded {} bytes",
            limits.max_output_bytes
        )))
    } else {
        None
    }
}

/// Whether [`Job::file_size_gate`] is used on this platform: `prlimit(2)`
/// is needed to set `RLIMIT_FSIZE` on latexmk from the parent.
pub(crate) const FILE_SIZE_GATE_SUPPORTED: bool = texrun_process::PRLIMIT_SUPPORTED;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_limits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("b"), vec![0u8; 100]).unwrap();
        let dirs = [dir.path()];
        let limits = Limits::default()
            .with_max_file_bytes(1000)
            .with_max_output_bytes(1000);
        assert_eq!(check_output_size(&dirs, &limits), None);
        assert!(matches!(
            check_output_size(&dirs, &limits.with_max_file_bytes(100)),
            Some(StopReason::OutputLimit(m)) if m.contains("per-file")
        ));
        assert!(matches!(
            check_output_size(&dirs, &limits.with_max_output_bytes(150)),
            Some(StopReason::OutputLimit(m)) if m.contains("output directory")
        ));
    }

    #[test]
    fn engine_errors_keep_the_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let job = Job {
            program: Path::new("/nonexistent/texrun-test-program"),
            args: Vec::new(),
            cwd: dir.path(),
            env: EnvAllowlist::new(),
            timeout: None,
            cancel: None,
            size_dirs: Vec::new(),
            limits: Limits::default(),
            file_size_gate: false,
        };
        let err = run(&job).unwrap_err();
        assert!(matches!(err, EngineError::Spawn { .. }), "{err:?}");
    }

    #[test]
    fn an_exceeded_output_limit_stops_latexmk() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let job = Job {
            program: Path::new("/bin/sh"),
            args: vec![
                "-c".into(),
                "head -c 5000 /dev/zero > out/big; sleep 30".into(),
            ],
            cwd: dir.path(),
            env: EnvAllowlist::new().with("PATH", "/usr/bin:/bin"),
            timeout: Some(Duration::from_secs(20)),
            cancel: None,
            size_dirs: vec![out.as_path()],
            limits: Limits::default()
                .with_max_output_bytes(1000)
                .with_size_check_interval(Duration::from_millis(20)),
            file_size_gate: false,
        };
        let done = run(&job).unwrap();
        assert!(
            matches!(&done.stop, Some(StopReason::OutputLimit(m)) if m.contains("output directory")),
            "{:?}",
            done.stop
        );
    }
}
