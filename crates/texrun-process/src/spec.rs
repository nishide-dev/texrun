//! What to run, and how it is started.

use std::ffi::OsString;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cgroup::{CgroupLimits, Cgroups};
use crate::env::EnvAllowlist;
use crate::error::RunError;
use crate::gate::ExecGate;
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
    /// stdin is `/dev/null`; the limits are set with `prlimit(2)` from the
    /// parent after `spawn()` returned, i.e. after the child has already
    /// `exec`ed and runs as the program.
    ///
    /// What this does and does not guarantee:
    ///
    /// - The gap until the limits are set is **not bounded in time**: it
    ///   depends on the parent's scheduling and can be long under load.
    /// - Memory allocated and files written during the gap are not undone
    ///   when the limits arrive.
    /// - Descendants started during the gap do not inherit the limits and
    ///   stay unlimited.
    /// - The leader itself gets the limits eventually (as long as it runs).
    ///
    /// So use it only for a single-process program that starts no children,
    /// and treat the limits as a best-effort layer: the primary bounds must
    /// come from elsewhere (e.g. the caller's check hook and input limits).
    /// For a program that cannot wait on stdin itself, prefer
    /// [`StartMode::ExecGate`].
    Immediate,
    /// stdin is a pipe. The limits are set while the child waits for `token`
    /// on stdin (the program, or a wrapper such as the latexmk rc, must
    /// implement the gate), then `token` is written and stdin closed. A
    /// child that sees EOF without the token must exit without doing
    /// anything.
    ///
    /// The token is written before the poll loop starts, so it must fit
    /// into the pipe buffer without blocking: at most
    /// [`StartMode::MAX_TOKEN_LEN`] bytes ([`run`](crate::run) refuses a
    /// longer one).
    StdinGate {
        /// Bytes written to stdin once the limits are in place.
        token: Vec<u8>,
    },
    /// The program is started through an exec gate
    /// ([`run_gate`](crate::run_gate)): the gate waits until
    /// [`Launcher::on_spawn`] has run, sets [`Spec::rlimits`] on itself with
    /// `setrlimit(2)` and then `exec`s the program, which therefore runs
    /// limited from its first instruction, and so does every descendant.
    /// This works on macOS too (except [`Resource::AddressSpace`](crate::Resource::AddressSpace)).
    /// The program's stdin is `/dev/null`, as with [`StartMode::Immediate`].
    ///
    /// If the gate cannot be used (see [`ExecGate`]; checked before
    /// spawning), the run fails with [`RunError::Unsupported`] when
    /// [`Spec::require_rlimits`] is set, and otherwise falls back to
    /// [`StartMode::Immediate`], recorded in
    /// [`Finished::gate_fallback`](crate::Finished::gate_fallback).
    ExecGate(ExecGate),
}

impl StartMode {
    /// Longest [`StartMode::StdinGate`] token: 512 bytes, POSIX's minimum
    /// `PIPE_BUF`, so writing it to an empty pipe never blocks.
    pub const MAX_TOKEN_LEN: usize = 512;
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
    /// Resource limits.
    ///
    /// With [`StartMode::ExecGate`] the gate sets them on every Unix
    /// platform. Otherwise they are only applied where `prlimit(2)` exists
    /// ([`PRLIMIT_SUPPORTED`](crate::PRLIMIT_SUPPORTED), Linux). A limit
    /// that cannot be applied is skipped, and
    /// [`Finished::rlimits_applied`](crate::Finished::rlimits_applied) is
    /// `false`, unless [`Spec::require_rlimits`] is set.
    pub rlimits: Rlimits,
    /// Fail with [`RunError::Unsupported`] (before spawning) instead of
    /// running without the [`Spec::rlimits`] where they cannot be applied
    /// (including an unusable [`StartMode::ExecGate`]).
    /// Default: `false` (best effort).
    pub require_rlimits: bool,
    /// Run the program and all its descendants in a cgroup of their own
    /// with these limits (Linux, docs/security.md §3.10; see [`Cgroups`]).
    /// The child is moved into it right after the spawn, so with
    /// [`StartMode::ExecGate`] or [`StartMode::StdinGate`] every descendant
    /// is inside. Where the cgroups cannot be used the run goes on without
    /// (recorded in [`Finished::cgroup`](crate::Finished::cgroup)), unless
    /// they are [required](Cgroups::with_required). Ignored (not requested)
    /// when [`Launcher::apply_rlimits`] is `false`.
    pub cgroup: Option<(&'a Cgroups, CgroupLimits)>,
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
            require_rlimits: false,
            cgroup: None,
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

    /// Sets [`Spec::require_rlimits`].
    #[must_use]
    pub fn with_require_rlimits(mut self, required: bool) -> Self {
        self.require_rlimits = required;
        self
    }

    /// Sets [`Spec::cgroup`].
    #[must_use]
    pub fn with_cgroup(mut self, cgroups: &'a Cgroups, limits: CgroupLimits) -> Self {
        self.cgroup = Some((cgroups, limits));
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
/// gate. With [`StartMode::ExecGate`], [`Launcher::command`] is given a
/// spec for the gate: its program is the gate's (a path on the host, e.g.
/// `/proc/self/exe` for the texrun CLI) and its arguments are the gate's,
/// everything else is unchanged. The limits are passed to the gate instead
/// of being set with `prlimit`. A launcher that runs the program elsewhere
/// (a container runtime, #26) or applies the limits itself
/// ([`Launcher::apply_rlimits`] `== false`, in which case the gate gets no
/// limits) should be used without an exec gate. Every time it kills the process group it also calls
/// [`Launcher::on_kill`], and after reaping the leader
/// [`Launcher::on_reaped`].
///
/// The hooks receive the leader's PID so that one launcher can serve
/// several runs at once. Their exact shape (e.g. a per-run handle returned
/// by `on_spawn`) will be revisited with the first real implementation
/// (#26 containers). The cgroups of #25 are not a launcher: the supervisor
/// handles them itself ([`Spec::cgroup`]), after the spawn and before
/// [`Launcher::on_spawn`], and kills them with every group kill before
/// [`Launcher::on_kill`].
pub trait Launcher {
    /// The command for `spec`: program, arguments, environment (after
    /// `env_clear()`) and working directory. Stdio and the process group are
    /// set by the supervisor.
    fn command(&self, spec: &Spec<'_>) -> Result<Command, RunError>;

    /// Whether the supervisor applies [`Spec::rlimits`] (with `prlimit(2)`,
    /// or through the exec gate). A launcher that hands them to a container
    /// runtime instead returns `false`.
    fn apply_rlimits(&self) -> bool {
        true
    }

    /// Called right after spawning, before the limits are set and the start
    /// gate is released (after moving the child into its cgroup, if
    /// [`Spec::cgroup`]). An error kills the child and fails the run.
    ///
    /// With [`StartMode::Immediate`] the child already runs when this is
    /// called, so descendants it starts before this hook finishes are not
    /// covered (the same holds for [`Spec::cgroup`]). To include every
    /// descendant, combine it with [`StartMode::StdinGate`] or
    /// [`StartMode::ExecGate`]: then `pid` is still waiting at the gate.
    fn on_spawn(&self, _pid: u32) -> io::Result<()> {
        Ok(())
    }

    /// Called whenever the process group of leader `pid` is killed, after
    /// `killpg` (and `cgroup.kill`, [`Spec::cgroup`]) and before the leader
    /// is reaped (e.g. to stop a container).
    ///
    /// Also called from a drop guard while unwinding, so it must not panic
    /// (a panic there aborts the process).
    fn on_kill(&self, _pid: u32) {}

    /// Called once after the leader `pid` has been reaped (e.g. to remove a
    /// container). Not called when spawning failed. Must not panic, like
    /// [`Launcher::on_kill`].
    fn on_reaped(&self, _pid: u32) {}
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
                // descriptor is still open until `exec`. Not for 0..=2:
                // the child may have replaced those by its stdio already
                // when it changes directory.
                let raw = fd.as_raw_fd();
                let proc_fd = Path::new("/proc/self/fd");
                if raw > 2 && proc_fd.is_dir() {
                    return Ok(proc_fd.join(raw.to_string()));
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
