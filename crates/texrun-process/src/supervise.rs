//! The supervisor: spawn, poll loop, group kill and reaping
//! (docs/security.md §3.6).

use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use texrun_core::CancelToken;

use crate::capture::{CapturedOutput, Reader};
use crate::error::RunError;
use crate::rlimit;
use crate::spec::{Capture, HostLauncher, Launcher, Spec, StartMode};

/// How often the poll loop wakes up.
///
/// One interval for every program: checking the leader with `waitid` is a
/// cheap system call, and the preview runs up to a few hundred short tool
/// processes, whose total latency this bounds. Expensive checks set their
/// own interval ([`Watch::with_check`]).
pub const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long the pipe readers are waited for after the process group is
/// gone. They normally finish at once (every writer is dead, so the pipes
/// are at EOF); only a process that left the group can keep a pipe open,
/// and it is not waited for. What was read until then is returned.
pub const READER_GRACE: Duration = Duration::from_millis(500);

/// A check the poll loop runs every `interval`.
type CheckFn<'a, S> = Box<dyn FnMut() -> Option<S> + 'a>;

/// What to watch while the child runs.
///
/// `S` is the value a check hook returns to stop the child (e.g. which
/// output limit was exceeded).
pub struct Watch<'a, S = ()> {
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    cancel: Option<&'a CancelToken>,
    check: Option<(Duration, CheckFn<'a, S>)>,
}

impl<S> Default for Watch<'_, S> {
    fn default() -> Self {
        Self {
            timeout: None,
            deadline: None,
            cancel: None,
            check: None,
        }
    }
}

impl<S> std::fmt::Debug for Watch<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("timeout", &self.timeout)
            .field("deadline", &self.deadline)
            .field("cancel", &self.cancel)
            .field("check_interval", &self.check.as_ref().map(|(i, _)| i))
            .finish()
    }
}

impl<'a, S> Watch<'a, S> {
    /// Watches nothing: the child runs until it exits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stops the child ([`Stop::TimedOut`]) `timeout` after it was spawned.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Stops the child ([`Stop::TimedOut`]) at `deadline` (e.g. one budget
    /// shared by several runs). Combined with a timeout, whichever comes
    /// first applies.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Stops the child ([`Stop::Cancelled`]) once `cancel` is cancelled.
    #[must_use]
    pub fn with_cancel(mut self, cancel: &'a CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Runs `check` every `interval` (at most every [`POLL_INTERVAL`]; the
    /// first time one `interval` after the spawn) and stops the child
    /// ([`Stop::Check`]) when it returns a value.
    #[must_use]
    pub fn with_check(mut self, interval: Duration, check: impl FnMut() -> Option<S> + 'a) -> Self {
        self.check = Some((interval, Box::new(check)));
        self
    }
}

/// Why the supervisor stopped the process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop<S = ()> {
    /// The timeout or the deadline passed.
    TimedOut,
    /// The cancel token was cancelled.
    Cancelled,
    /// The check hook returned this value.
    Check(S),
}

/// Result of a supervised run. The process group has been killed and the
/// leader reaped.
#[derive(Debug)]
#[non_exhaustive]
pub struct Finished<S = ()> {
    /// PID of the leader, which was also the process group ID.
    ///
    /// For diagnostics only. The process has been reaped, so the PID may
    /// already belong to an unrelated process: never send signals to it.
    pub pid: u32,
    /// Exit status of the leader (after [`Finished::stop`], normally
    /// `SIGKILL`).
    pub status: ExitStatus,
    /// Why the supervisor stopped the child, or `None` if it exited on its
    /// own.
    pub stop: Option<Stop<S>>,
    /// Time from spawning until the exit or the stop.
    pub elapsed: Duration,
    /// Captured stdout (empty for [`Capture::Discard`]).
    pub stdout: CapturedOutput,
    /// Captured stderr (empty for [`Capture::Discard`]).
    pub stderr: CapturedOutput,
    /// Whether the supervisor set [`Spec::rlimits`] on the child with
    /// `prlimit(2)`. `false` if there were none, if the platform has no
    /// `prlimit` ([`PRLIMIT_SUPPORTED`](crate::PRLIMIT_SUPPORTED)) or if the
    /// launcher applies them itself ([`Launcher::apply_rlimits`]).
    pub rlimits_applied: bool,
}

/// Runs `spec` on the host ([`HostLauncher`]) and supervises it until it
/// exits or `watch` stops it.
pub fn run<S>(spec: &Spec<'_>, watch: Watch<'_, S>) -> Result<Finished<S>, RunError> {
    run_with(&HostLauncher, spec, watch)
}

/// Like [`run`], started through `launcher`.
pub fn run_with<S>(
    launcher: &dyn Launcher,
    spec: &Spec<'_>,
    mut watch: Watch<'_, S>,
) -> Result<Finished<S>, RunError> {
    let (gate, apply_rlimits) = check_spec(launcher, spec)?;
    let mut cmd = launcher.command(spec)?;
    cmd.stdin(if gate.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(stdio(spec.stdout))
    .stderr(stdio(spec.stderr))
    .process_group(0);

    let start = Instant::now();
    let child = cmd.spawn().map_err(|source| RunError::Spawn {
        program: spec.program_name(),
        source,
    })?;
    let leader_pid = child.id();
    let pgid = Pid::from_child(&child);
    let mut group = Group {
        child,
        pid: leader_pid,
        pgid,
        launcher,
        reaped: false,
    };

    let stdout = reader(group.child.stdout.take(), spec.stdout);
    let stderr = reader(group.child.stderr.take(), spec.stderr);

    let program = spec.program_name();
    launcher
        .on_spawn(leader_pid)
        .map_err(RunError::io(format!("preparing {program}")))?;
    let rlimits_applied = apply_rlimits && rlimit::PRLIMIT_SUPPORTED;
    if rlimits_applied {
        // `pgid` is the child's PID (it leads its own group).
        rlimit::apply(pgid, &spec.rlimits).map_err(RunError::io(format!(
            "setting resource limits on {program}"
        )))?;
    }
    if let Some(token) = gate
        && let Some(mut stdin) = group.child.stdin.take()
    {
        // An error means the child is already gone; the poll loop sees its
        // exit status. Dropping `stdin` closes the pipe.
        let _ = stdin.write_all(token);
    }

    // A timeout too large to represent never expires.
    let deadline = match (
        watch.timeout.and_then(|t| start.checked_add(t)),
        watch.deadline,
    ) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let mut last_check = start;
    let stop = loop {
        // Detect the exit without reaping: the leader stays a zombie, which
        // keeps its PID and PGID reserved until the group has been killed.
        if leader_exited(pgid).map_err(RunError::io(format!("waiting for {program}")))? {
            break None;
        }
        if watch.cancel.is_some_and(CancelToken::is_cancelled) {
            break Some(Stop::Cancelled);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break Some(Stop::TimedOut);
        }
        if let Some((interval, check)) = watch.check.as_mut()
            && last_check.elapsed() >= *interval
        {
            last_check = Instant::now();
            if let Some(value) = check() {
                break Some(Stop::Check(value));
            }
        }
        thread::sleep(POLL_INTERVAL);
    };
    let elapsed = start.elapsed();
    // Stop the group (or, after a normal exit, whatever the leader left
    // behind) while the leader is not yet reaped, then reap it.
    group.kill();
    let status = group
        .child
        .wait()
        .map_err(RunError::io(format!("waiting for {program}")))?;
    group.reaped = true;
    launcher.on_reaped(leader_pid);

    let readers_deadline = Instant::now() + READER_GRACE;
    Ok(Finished {
        pid: leader_pid,
        status,
        stop,
        elapsed,
        stdout: stdout.finish(readers_deadline),
        stderr: stderr.finish(readers_deadline),
        rlimits_applied,
    })
}

/// Checks `spec` before anything is started. Returns the start gate token
/// (if any) and whether the supervisor applies the rlimits.
fn check_spec<'s>(
    launcher: &dyn Launcher,
    spec: &'s Spec<'_>,
) -> Result<(Option<&'s [u8]>, bool), RunError> {
    let gate = match &spec.start {
        StartMode::Immediate => None,
        StartMode::StdinGate { token } => {
            if token.len() > StartMode::MAX_TOKEN_LEN {
                return Err(RunError::InvalidSpec(format!(
                    "the start gate token has {} bytes; at most {} are allowed",
                    token.len(),
                    StartMode::MAX_TOKEN_LEN
                )));
            }
            Some(token.as_slice())
        }
    };
    let apply_rlimits = launcher.apply_rlimits() && !spec.rlimits.is_empty();
    if apply_rlimits && !rlimit::PRLIMIT_SUPPORTED && spec.require_rlimits {
        return Err(RunError::Unsupported(
            "resource limits for a child process require prlimit(2) (Linux)".to_owned(),
        ));
    }
    Ok((gate, apply_rlimits))
}

fn stdio(capture: Capture) -> Stdio {
    match capture {
        Capture::Keep(_) => Stdio::piped(),
        Capture::Discard => Stdio::null(),
    }
}

fn reader<R: io::Read + Send + 'static>(pipe: Option<R>, capture: Capture) -> Reader {
    match (pipe, capture) {
        (Some(pipe), Capture::Keep(cap)) => Reader::spawn(pipe, cap),
        _ => Reader::none(),
    }
}

/// A spawned process group that is killed and reaped when dropped, so no
/// early return (or panic) can leave it running.
struct Group<'l> {
    child: Child,
    pid: u32,
    pgid: Pid,
    launcher: &'l dyn Launcher,
    reaped: bool,
}

impl Group<'_> {
    /// Sends `SIGKILL` to the process group. `ESRCH` (nobody left) and other
    /// errors are ignored: there is nothing more to do about them.
    ///
    /// Only called while the leader has not been reaped: the leader (alive
    /// or a zombie) keeps the PGID reserved, so the signal cannot reach an
    /// unrelated group.
    fn kill(&self) {
        debug_assert!(!self.reaped);
        let _ = kill_process_group(self.pgid, Signal::KILL);
        self.launcher.on_kill(self.pid);
    }
}

impl Drop for Group<'_> {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill();
            if self.child.wait().is_ok() {
                self.launcher.on_reaped(self.pid);
            }
        }
    }
}

/// Whether the leader has exited, without reaping it
/// (`waitid(P_PID, EXITED | NOHANG | NOWAIT)`).
fn leader_exited(pid: Pid) -> io::Result<bool> {
    use rustix::process::{WaitId, WaitIdOptions, waitid};

    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    loop {
        match waitid(WaitId::Pid(pid), options) {
            Ok(status) => return Ok(status.is_some()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}
