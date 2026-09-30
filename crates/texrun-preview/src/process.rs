//! Running one external tool under the limits of docs/security.md §3.
//!
//! Supervision (cleared environment, own process group killed with
//! `SIGKILL` on timeout / cancellation / an outgrown image and after every
//! exit, kill-before-reap, bounded output, resource limits) is that of
//! `texrun-process`. This module adds the tool-specific values:
//!
//! - the environment allowlist (`PATH`, `HOME`, `LC_ALL`);
//! - the working directory, held as a descriptor ([`ToolEnv::work`]);
//! - `RLIMIT_AS` and `RLIMIT_FSIZE` ([`TOOL_ADDRESS_SPACE`],
//!   [`MIN_FILE_SIZE_LIMIT`]), set by the exec gate before the tool starts
//!   (or, without a gate, right after it was spawned on Linux);
//! - a watch on the size of the image being rendered.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use texrun_core::CancelToken;
use texrun_process::{
    Capture, CgroupLimits, CgroupOutcome, Cgroups, Cwd, EnvAllowlist, ExecGate, Resource, Rlimits,
    Spec, StartMode, Stop, Watch,
};

use crate::fsops;

/// Kept prefix of stdout (metadata output of up to 200 pages is far smaller).
pub(crate) const STDOUT_LIMIT: usize = 1024 * 1024;
/// Kept prefix of stderr.
pub(crate) const STDERR_LIMIT: usize = 64 * 1024;

/// Address space limit of one tool process (`RLIMIT_AS`, Linux only): 2 GiB.
/// A 4096 x 4096 px page needs about 64 MiB of pixels; the rest is room for
/// decoding embedded images.
///
/// With an exec gate ([`Previewer::with_exec_gate`](crate::Previewer::with_exec_gate),
/// which the texrun CLI uses) the limits are set before the tool starts.
/// Without one they are set with `prlimit(2)` after the tool was spawned
/// ([`StartMode::Immediate`]), which is best effort only: the gap until then
/// is not bounded in time, and what the tool allocates or writes in it is
/// not undone (the tools start no descendants).
pub(crate) const TOOL_ADDRESS_SPACE: u64 = 2 * 1024 * 1024 * 1024;

/// Smallest `RLIMIT_FSIZE` given to a tool: 16 MiB. The tools may
/// write caches (e.g. fontconfig) into their private `HOME`, and hitting the
/// limit kills them with `SIGXFSZ`, so it is never set below this even when
/// the remaining image budget is smaller; the poll loop enforces the budget.
pub(crate) const MIN_FILE_SIZE_LIMIT: u64 = 16 * 1024 * 1024;

/// Added to the preview timeout for the CPU time limit of each tool
/// (`RLIMIT_CPU` soft limit): 10 s. A single-threaded tool cannot reach it
/// before the timeout; it bounds a tool that runs on several CPUs or keeps
/// running after the preview (docs/security.md §3.10).
pub(crate) const CPU_TIME_MARGIN: Duration = Duration::from_secs(10);
/// Time from the CPU soft limit (`SIGXCPU`) to the hard limit (`SIGKILL`).
pub(crate) const CPU_KILL_GRACE: u64 = 5;
/// Memory of one tool run with all its processes (cgroup `memory.max`):
/// 2 GiB, like [`TOOL_ADDRESS_SPACE`].
pub(crate) const TOOL_MEMORY: u64 = 2 * 1024 * 1024 * 1024;
/// Processes and threads of one tool run (cgroup `pids.max`).
pub(crate) const TOOL_PROCESSES: u64 = 32;
/// CPUs one tool run may use at once (cgroup `cpu.max`).
pub(crate) const TOOL_CPUS: u32 = 2;

/// The CPU time soft limit of a tool, in whole seconds, for a preview with
/// `timeout`.
pub(crate) fn cpu_seconds(timeout: Duration) -> u64 {
    let cpu = timeout.saturating_add(CPU_TIME_MARGIN);
    cpu.as_secs()
        .saturating_add(u64::from(cpu.subsec_nanos() > 0))
        .clamp(1, u64::from(u32::MAX))
}

/// The environment of a tool process.
#[derive(Debug)]
pub(crate) struct ToolEnv {
    /// `PATH` for the tool: the absolute entries of the search path used for
    /// detection.
    pub(crate) path: Option<OsString>,
    /// A private, empty directory (the tools may write caches there).
    pub(crate) home: PathBuf,
    /// Working directory; the image is rendered to a relative name in it.
    /// The tool is started in this descriptor itself where possible
    /// ([`Cwd::Dir`]), and the image is inspected and moved through it.
    pub(crate) work: OwnedFd,
    /// A path of [`ToolEnv::work`], for where the descriptor cannot be used.
    pub(crate) work_path: PathBuf,
}

impl ToolEnv {
    fn vars(&self) -> EnvAllowlist {
        let env = EnvAllowlist::new()
            .with("HOME", self.home.as_os_str())
            // Stable, locale-independent number formatting and messages.
            .with("LC_ALL", "C");
        match &self.path {
            Some(path) => env.with_path(path),
            None => env,
        }
    }
}

/// What to watch while the tool runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits<'a> {
    /// Start the tool through this exec gate.
    pub(crate) gate: Option<&'a ExecGate>,
    /// Run the tool in a cgroup of its own.
    pub(crate) cgroups: Option<&'a Cgroups>,
    /// `RLIMIT_CPU` soft limit, in seconds ([`cpu_seconds`]).
    pub(crate) cpu_seconds: u64,
    pub(crate) deadline: Instant,
    pub(crate) cancel: &'a CancelToken,
    /// Kill the tool if this file in [`ToolEnv::work`] grows beyond this
    /// many bytes.
    pub(crate) watch: Option<(&'a str, u64)>,
}

/// How the tool run ended.
#[derive(Debug)]
pub(crate) enum RunEnd {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
    OutputTooLarge,
    /// The tool reached its CPU time, memory or process limit (the
    /// message says which).
    LimitExceeded(String),
    /// The tool was not started because its resource limits could not be
    /// put in place first (e.g. a required exec gate became unusable).
    LimitsUnavailable(String),
    /// Spawning, limiting or waiting failed.
    Failed(io::Error),
}

#[derive(Debug)]
pub(crate) struct RunOutput {
    pub(crate) end: RunEnd,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// Why the exec gate was not used for this run
    /// ([`texrun_process::Finished::gate_fallback`]).
    pub(crate) gate_fallback: Option<String>,
    /// Why the tool ran without its cgroup, if one was asked for.
    pub(crate) cgroup_unavailable: Option<String>,
}

impl RunOutput {
    fn empty(end: RunEnd) -> Self {
        Self {
            end,
            stdout: Vec::new(),
            stderr: Vec::new(),
            gate_fallback: None,
            cgroup_unavailable: None,
        }
    }

    pub(crate) fn succeeded(&self) -> bool {
        matches!(&self.end, RunEnd::Exited(status) if status.success())
    }
}

/// Runs `program args...` in `env` and waits for it within `limits`.
pub(crate) fn run(
    program: &Path,
    args: &[OsString],
    env: &ToolEnv,
    capture_stdout: bool,
    limits: Limits<'_>,
) -> RunOutput {
    if limits.cancel.is_cancelled() {
        return RunOutput::empty(RunEnd::Cancelled);
    }
    if Instant::now() >= limits.deadline {
        return RunOutput::empty(RunEnd::TimedOut);
    }

    let file_size = limits
        .watch
        .map_or(0, |(_, budget)| budget.saturating_add(1))
        .max(MIN_FILE_SIZE_LIMIT);
    let cwd = Cwd::Dir {
        fd: env.work.as_fd(),
        path: &env.work_path,
    };
    let spec = Spec::new(program, cwd)
        .with_args(args.iter().cloned())
        .with_env(env.vars())
        .with_stdout(if capture_stdout {
            Capture::Keep(STDOUT_LIMIT)
        } else {
            Capture::Discard
        })
        .with_stderr(Capture::Keep(STDERR_LIMIT))
        .with_rlimits(
            Rlimits::new()
                .with(Resource::AddressSpace, TOOL_ADDRESS_SPACE)
                .with(Resource::FileSize, file_size)
                .with_soft_hard(
                    Resource::Cpu,
                    limits.cpu_seconds,
                    limits.cpu_seconds.saturating_add(CPU_KILL_GRACE),
                )
                // `SIGXFSZ` and `SIGXCPU` dump core by default; like the TeX
                // engine, never leave core files behind.
                .with(Resource::Core, 0),
        )
        .with_start(limits.gate.map_or(StartMode::Immediate, |gate| {
            StartMode::ExecGate(gate.clone())
        }));
    let spec = match limits.cgroups {
        Some(cgroups) => spec.with_cgroup(
            cgroups,
            CgroupLimits::new()
                .with_memory_max(TOOL_MEMORY)
                .with_pids_max(TOOL_PROCESSES)
                .with_cpus(TOOL_CPUS),
        ),
        None => spec,
    };

    let mut watch = Watch::new()
        .with_deadline(limits.deadline)
        .with_cancel(limits.cancel);
    if let Some((name, max)) = limits.watch {
        // A single `fstatat`: cheap enough for every poll.
        watch = watch.with_check(Duration::ZERO, move || {
            fsops::regular_file_len(&env.work, name)
                .is_some_and(|len| len > max)
                .then_some(())
        });
    }
    match texrun_process::run(&spec, watch) {
        Ok(done) => RunOutput {
            end: match done.stop {
                None => limit_exceeded(done.status, &done.cgroup, limits.cpu_seconds)
                    .map_or(RunEnd::Exited(done.status), RunEnd::LimitExceeded),
                Some(Stop::TimedOut) => RunEnd::TimedOut,
                Some(Stop::Cancelled) => RunEnd::Cancelled,
                Some(Stop::Check(())) => RunEnd::OutputTooLarge,
            },
            stdout: done.stdout.bytes,
            stderr: done.stderr.bytes,
            gate_fallback: done.gate_fallback,
            cgroup_unavailable: match done.cgroup {
                CgroupOutcome::Unavailable(reason) => Some(reason),
                _ => None,
            },
        },
        // The notice names the program already.
        Err(texrun_process::RunError::Spawn { source, .. }) => {
            RunOutput::empty(RunEnd::Failed(source))
        }
        // Checked before spawning: a required gate or cgroup that became
        // unusable since the preview checked it. Nothing ran.
        Err(texrun_process::RunError::Unsupported(reason)) => {
            RunOutput::empty(RunEnd::LimitsUnavailable(reason))
        }
        Err(e) => RunOutput::empty(RunEnd::Failed(io::Error::other(e))),
    }
}

/// The limit a tool that ended on its own reached, if any: `SIGXCPU`, or
/// what its cgroup recorded.
fn limit_exceeded(status: ExitStatus, cgroup: &CgroupOutcome, cpu_seconds: u64) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;

    if let CgroupOutcome::Applied(usage) = cgroup {
        if usage.oom_kills > 0 {
            return Some(format!(
                "used more than {TOOL_MEMORY} bytes of memory and was stopped"
            ));
        }
        if usage.pids_max_hits > 0 {
            return Some(format!(
                "tried to run more than {TOOL_PROCESSES} processes and threads"
            ));
        }
    }
    (status.signal() == Some(rustix::process::Signal::XCPU.as_raw()))
        .then(|| format!("used more than {cpu_seconds} s of CPU time"))
}

/// A short, printable excerpt of tool output for a notice: lossy UTF-8,
/// trimmed, at most `max_chars` characters, control characters (other than
/// newline and tab) escaped so that the text is safe to print.
pub(crate) fn excerpt(bytes: &[u8], max_chars: usize) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i == max_chars {
            out.push_str("...");
            break;
        }
        if ch.is_control() && ch != '\n' && ch != '\t' {
            out.extend(ch.escape_unicode());
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reached_limits_are_told_apart() {
        use std::os::unix::process::ExitStatusExt;
        let status = ExitStatus::from_raw;
        let xcpu = rustix::process::Signal::XCPU.as_raw();
        let none = CgroupOutcome::NotRequested;
        assert_eq!(
            limit_exceeded(status(xcpu), &none, 40).as_deref(),
            Some("used more than 40 s of CPU time")
        );
        assert_eq!(limit_exceeded(status(0), &none, 40), None);
        assert_eq!(limit_exceeded(status(9), &none, 40), None);
        let mut usage = texrun_process::CgroupUsage::default();
        assert_eq!(
            limit_exceeded(status(9), &CgroupOutcome::Applied(usage), 40),
            None
        );
        usage.oom_kills = 1;
        assert!(
            limit_exceeded(status(9), &CgroupOutcome::Applied(usage), 40)
                .unwrap()
                .contains("memory")
        );
        usage.oom_kills = 0;
        usage.pids_max_hits = 1;
        assert!(
            limit_exceeded(status(1 << 8), &CgroupOutcome::Applied(usage), 40)
                .unwrap()
                .contains("processes")
        );
    }

    #[test]
    fn cpu_time_follows_the_timeout() {
        assert_eq!(cpu_seconds(Duration::from_secs(30)), 40);
        assert_eq!(cpu_seconds(Duration::from_millis(1)), 11);
        assert_eq!(cpu_seconds(Duration::MAX), u64::from(u32::MAX));
    }

    #[test]
    fn excerpt_escapes_control_characters_and_truncates() {
        assert_eq!(excerpt(b"  \n ", 10), None);
        assert_eq!(
            excerpt(b"bad\x1b[31m\nnext", 100).unwrap(),
            "bad\\u{1b}[31m\nnext"
        );
        assert_eq!(excerpt(b"abcdef", 3).unwrap(), "abc...");
        assert_eq!(excerpt(b"\xff ok", 10).unwrap(), "\u{fffd} ok");
    }
}
