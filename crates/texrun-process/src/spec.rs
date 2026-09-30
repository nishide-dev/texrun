//! What to run, and how it is started.

use std::ffi::OsString;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::env::EnvAllowlist;
use crate::error::RunError;
use crate::rlimit::Rlimits;

/// Working directory of the child.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Cwd<'a> {
    /// A path chosen and checked by the caller.
    Path(&'a Path),
    /// A directory held open by the caller, for a directory in a place that
    /// others can modify (e.g. a scratch directory in an output root shared
    /// with left-over engine processes).
    ///
    /// On Linux the child changes into the descriptor itself (through
    /// `/proc/self/fd`), so a directory renamed or replaced by a symlink in
    /// the meantime cannot redirect it. Where that is not available (no
    /// `/proc`, macOS), `path` is used after checking that it still names
    /// the same directory as `fd`; the check and the child's `chdir` are not
    /// atomic there.
    Dir {
        /// The directory (`O_DIRECTORY`).
        fd: BorrowedFd<'a>,
        /// A path of the same directory.
        path: &'a Path,
    },
}

/// What happens to stdout or stderr of the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Capture {
    /// Read to EOF, keeping the first `n` bytes.
    Keep(usize),
    /// Not connected (`/dev/null`).
    Discard,
}

/// How the child is released once it has been spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartMode {
    /// stdin is `/dev/null`; the limits are set right after spawning, while
    /// the child already runs. Only for programs that do nothing that needs
    /// the limits in their first instructions.
    Immediate,
    /// stdin is a pipe. The limits are set while the child waits for `token`
    /// on stdin (the program, or a wrapper such as the latexmk rc, must
    /// implement the gate), then `token` is written and stdin closed. A
    /// child that sees EOF without the token must exit without doing
    /// anything.
    StdinGate {
        /// Bytes written to stdin once the limits are in place.
        token: Vec<u8>,
    },
}

/// A program to run under supervision.
///
/// `#[non_exhaustive]`: construct with [`Spec::new`] and the `with_*`
/// methods.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Spec<'a> {
    /// The program. Should be absolute: a relative path would be resolved
    /// against the child's working directory.
    pub program: PathBuf,
    /// Arguments (without the program).
    pub args: Vec<OsString>,
    /// The complete environment.
    pub env: EnvAllowlist,
    /// Working directory.
    pub cwd: Cwd<'a>,
    /// stdout handling. Default: `Keep(1 MiB)`.
    pub stdout: Capture,
    /// stderr handling. Default: `Keep(1 MiB)`.
    pub stderr: Capture,
    /// Resource limits (Linux only, see [`PRLIMIT_SUPPORTED`](crate::PRLIMIT_SUPPORTED)).
    pub rlimits: Rlimits,
    /// How the child is started.
    pub start: StartMode,
}

impl<'a> Spec<'a> {
    /// Default [`Capture::Keep`] size of stdout and stderr: 1 MiB.
    pub const DEFAULT_CAPTURE: usize = 1024 * 1024;

    /// Runs `program` in `cwd` with an empty environment, no arguments and
    /// no limits.
    pub fn new(program: impl Into<PathBuf>, cwd: Cwd<'a>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: EnvAllowlist::new(),
            cwd,
            stdout: Capture::Keep(Self::DEFAULT_CAPTURE),
            stderr: Capture::Keep(Self::DEFAULT_CAPTURE),
            rlimits: Rlimits::new(),
            start: StartMode::Immediate,
        }
    }

    /// Sets [`Spec::args`].
    #[must_use]
    pub fn with_args<I, A>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets [`Spec::env`].
    #[must_use]
    pub fn with_env(mut self, env: EnvAllowlist) -> Self {
        self.env = env;
        self
    }

    /// Sets [`Spec::stdout`].
    #[must_use]
    pub fn with_stdout(mut self, capture: Capture) -> Self {
        self.stdout = capture;
        self
    }

    /// Sets [`Spec::stderr`].
    #[must_use]
    pub fn with_stderr(mut self, capture: Capture) -> Self {
        self.stderr = capture;
        self
    }

    /// Sets [`Spec::rlimits`].
    #[must_use]
    pub fn with_rlimits(mut self, rlimits: Rlimits) -> Self {
        self.rlimits = rlimits;
        self
    }

    /// Sets [`Spec::start`].
    #[must_use]
    pub fn with_start(mut self, start: StartMode) -> Self {
        self.start = start;
        self
    }

    /// The program for messages.
    pub(crate) fn program_name(&self) -> String {
        self.program.display().to_string()
    }
}

/// Turns a [`Spec`] into a started process.
///
/// The supervisor ([`run_with`](crate::run_with)) calls
/// [`Launcher::command`], then sets stdin / stdout / stderr and
/// `process_group(0)` itself, spawns, calls [`Launcher::on_spawn`], applies
/// [`Spec::rlimits`] (if [`Launcher::apply_rlimits`]) and releases the start
/// gate. Every time it kills the process group it also calls
/// [`Launcher::on_kill`].
pub trait Launcher {
    /// The command for `spec`: program, arguments, environment (after
    /// `env_clear()`) and working directory. Stdio and the process group are
    /// set by the supervisor.
    fn command(&self, spec: &Spec<'_>) -> Result<Command, RunError>;

    /// Whether the supervisor sets [`Spec::rlimits`] on the spawned process
    /// with `prlimit(2)`. A launcher that hands them to a container runtime
    /// instead returns `false`.
    fn apply_rlimits(&self) -> bool {
        true
    }

    /// Called right after spawning, before the limits are set and the start
    /// gate is released (e.g. to move the child into a cgroup). An error
    /// kills the child and fails the run.
    fn on_spawn(&self, _pid: u32) -> io::Result<()> {
        Ok(())
    }

    /// Called whenever the process group is killed, after `killpg` (e.g. to
    /// kill a whole cgroup, including processes that left the group).
    fn on_kill(&self) {}
}

/// Runs the [`Spec`] directly on the host.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostLauncher;

impl Launcher for HostLauncher {
    fn command(&self, spec: &Spec<'_>) -> Result<Command, RunError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .env_clear()
            .envs(spec.env.vars())
            .current_dir(resolve_cwd(spec.cwd)?);
        Ok(cmd)
    }
}

/// The path to pass as working directory for `cwd`.
pub(crate) fn resolve_cwd(cwd: Cwd<'_>) -> Result<PathBuf, RunError> {
    match cwd {
        Cwd::Path(path) => Ok(path.to_owned()),
        Cwd::Dir { fd, path } => {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                use std::os::fd::AsRawFd;
                // Resolved by the child, where the (close-on-exec)
                // descriptor is still open until `exec`.
                let proc_fd = Path::new("/proc/self/fd");
                if proc_fd.is_dir() {
                    return Ok(proc_fd.join(fd.as_raw_fd().to_string()));
                }
            }
            check_same_dir(fd, path)?;
            Ok(path.to_owned())
        }
    }
}

/// Fails unless `path` names the directory `fd`.
fn check_same_dir(fd: BorrowedFd<'_>, path: &Path) -> Result<(), RunError> {
    let context = || format!("checking the working directory {}", path.display());
    let held = rustix::fs::fstat(fd).map_err(|e| RunError::io(context())(e.into()))?;
    let named = rustix::fs::stat(path).map_err(|e| RunError::io(context())(e.into()))?;
    if (held.st_dev, held.st_ino) == (named.st_dev, named.st_ino) {
        Ok(())
    } else {
        Err(RunError::io(context())(io::Error::other(
            "the directory was replaced",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_same_dir_detects_a_replaced_directory() {
        use std::os::fd::AsFd;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        let fd = std::fs::File::open(&dir).unwrap();
        check_same_dir(fd.as_fd(), &dir).unwrap();
        std::fs::rename(&dir, root.path().join("moved")).unwrap();
        std::fs::create_dir(&dir).unwrap();
        assert!(check_same_dir(fd.as_fd(), &dir).is_err());
    }
}
