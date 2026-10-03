//! Running latexmk under supervision (docs/security.md §3.2, §3.6, §3.10).
//!
//! Process group, poll loop, group kill, reaping, output capture, the
//! resource limits and the cgroup are those of `texrun-process`. This
//! module adds what is specific to latexmk: the [`Limits`], the output size
//! check, how latexmk is started with its limits in place ([`Start`]: the
//! exec gate, or the start gate of the texrun rc,
//! [`crate::rc::RcOptions::stdin_gate`]) and how a reached limit is told
//! apart ([`limit_reached`]).

use std::path::Path;
use std::time::Duration;

use texrun_core::{CancelToken, EngineError};
pub use texrun_process::CapturedOutput;
use texrun_process::{
    Capture, CgroupLimits, CgroupOutcome, Cgroups, Cwd, EnvAllowlist, ExecGate, Resource, Rlimits,
    RunError, Spec, StartMode, Stop, Watch,
};
use texrun_sandbox::Container;

use crate::layout;

/// Size and resource limits (docs/security.md §3.2, §3.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Maximum size of one file written by the engine. Enforced with
    /// `RLIMIT_FSIZE` (where latexmk starts with its limits in place) and,
    /// on every platform, by the output size check.
    pub max_file_bytes: u64,
    /// Maximum total size of the output directory (and the engine's `HOME`).
    pub max_output_bytes: u64,
    /// How much of stdout and of stderr is kept (each). The rest is read and
    /// discarded.
    pub max_captured_bytes: usize,
    /// How often the output size is checked while the engine runs.
    pub size_check_interval: Duration,
    /// CPU time of each process (latexmk, pdflatex, bibtex, makeindex;
    /// `RLIMIT_CPU` soft limit). `None`: the compile timeout plus
    /// [`Limits::CPU_TIME_MARGIN`], so that a process can never reach it
    /// before the timeout unless it runs on several CPUs at once, or keeps
    /// running after the compile (it bounds processes that escaped the
    /// process group). The hard limit, at which the kernel sends `SIGKILL`,
    /// is [`Limits::CPU_KILL_GRACE`] later.
    pub max_cpu_time: Option<Duration>,
    /// Address space of each process (`RLIMIT_AS`, Linux only).
    pub max_address_space: u64,
    /// Memory of latexmk and all its descendants together (cgroup
    /// `memory.max`, where a cgroup is used).
    pub max_memory_bytes: u64,
    /// Processes and threads of latexmk and all its descendants together
    /// (cgroup `pids.max`, where a cgroup is used).
    pub max_processes: u64,
    /// CPUs latexmk and all its descendants may use at once (cgroup
    /// `cpu.max`, where a cgroup is used).
    pub max_cpus: u32,
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
    /// Added to the compile timeout for the default [`Limits::max_cpu_time`]:
    /// 10 s.
    pub const CPU_TIME_MARGIN: Duration = Duration::from_secs(10);
    /// Time between the CPU soft limit (`SIGXCPU`, which tells the cause)
    /// and the hard limit (`SIGKILL`, for a process that ignores
    /// `SIGXCPU`): 5 s.
    pub const CPU_KILL_GRACE: Duration = Duration::from_secs(5);
    /// Default [`Limits::max_address_space`]: 4 GiB.
    pub const DEFAULT_MAX_ADDRESS_SPACE: u64 = 4 * 1024 * 1024 * 1024;
    /// Default [`Limits::max_memory_bytes`]: 4 GiB.
    pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
    /// Default [`Limits::max_processes`]: 64.
    pub const DEFAULT_MAX_PROCESSES: u64 = 64;
    /// Default [`Limits::max_cpus`]: 2.
    pub const DEFAULT_MAX_CPUS: u32 = 2;

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

    /// Sets [`Limits::max_cpu_time`].
    #[must_use]
    pub fn with_max_cpu_time(mut self, cpu_time: Option<Duration>) -> Self {
        self.max_cpu_time = cpu_time;
        self
    }

    /// Sets [`Limits::max_address_space`].
    #[must_use]
    pub fn with_max_address_space(mut self, bytes: u64) -> Self {
        self.max_address_space = bytes;
        self
    }

    /// Sets [`Limits::max_memory_bytes`].
    #[must_use]
    pub fn with_max_memory_bytes(mut self, bytes: u64) -> Self {
        self.max_memory_bytes = bytes;
        self
    }

    /// Sets [`Limits::max_processes`].
    #[must_use]
    pub fn with_max_processes(mut self, count: u64) -> Self {
        self.max_processes = count;
        self
    }

    /// Sets [`Limits::max_cpus`].
    #[must_use]
    pub fn with_max_cpus(mut self, cpus: u32) -> Self {
        self.max_cpus = cpus;
        self
    }

    /// The CPU time soft limit, in whole seconds, for a compile with
    /// `timeout`.
    pub(crate) fn cpu_seconds(&self, timeout: Duration) -> u64 {
        let cpu = self
            .max_cpu_time
            .unwrap_or_else(|| timeout.saturating_add(Self::CPU_TIME_MARGIN));
        // Whole seconds, rounded up, at least 1; at most `u32::MAX` (136
        // years), far below what any platform reads as "unlimited".
        cpu.as_secs()
            .saturating_add(u64::from(cpu.subsec_nanos() > 0))
            .clamp(1, u64::from(u32::MAX))
    }

    /// The limits set on latexmk (inherited by every process it starts)
    /// for a compile with `timeout` on this host.
    pub(crate) fn rlimits(&self, timeout: Duration) -> Rlimits {
        self.rlimits_for(
            timeout,
            cfg!(any(target_os = "linux", target_os = "android")),
        )
    }

    /// [`Limits::rlimits`], with `RLIMIT_AS` if `address_space` (on Linux,
    /// including a Linux container on any host).
    pub(crate) fn rlimits_for(&self, timeout: Duration, address_space: bool) -> Rlimits {
        let cpu = self.cpu_seconds(timeout);
        let limits = Rlimits::new()
            .with(Resource::FileSize, self.max_file_bytes)
            // `SIGXFSZ` and `SIGXCPU` dump core by default, into TeX's
            // working directory (outside the size checks).
            .with(Resource::Core, 0)
            .with_soft_hard(
                Resource::Cpu,
                cpu,
                cpu.saturating_add(Self::CPU_KILL_GRACE.as_secs()),
            );
        if address_space {
            limits.with(Resource::AddressSpace, self.max_address_space)
        } else {
            limits
        }
    }

    /// The limits of latexmk's cgroup.
    pub(crate) fn cgroup(&self) -> CgroupLimits {
        CgroupLimits::new()
            .with_memory_max(self.max_memory_bytes)
            .with_pids_max(self.max_processes)
            .with_cpus(self.max_cpus)
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            max_output_bytes: Self::DEFAULT_MAX_OUTPUT_BYTES,
            max_captured_bytes: Self::DEFAULT_MAX_CAPTURED_BYTES,
            size_check_interval: Self::DEFAULT_SIZE_CHECK_INTERVAL,
            max_cpu_time: None,
            max_address_space: Self::DEFAULT_MAX_ADDRESS_SPACE,
            max_memory_bytes: Self::DEFAULT_MAX_MEMORY_BYTES,
            max_processes: Self::DEFAULT_MAX_PROCESSES,
            max_cpus: Self::DEFAULT_MAX_CPUS,
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

/// How latexmk is started with its limits in place.
#[derive(Debug, Clone)]
pub(crate) enum Start<'a> {
    /// Without limits (`latexmk -v`).
    Plain,
    /// Through the exec gate: the limits are set before latexmk starts
    /// (Linux and macOS). The rc has no start gate.
    ExecGate(&'a ExecGate),
    /// latexmk waits at the start gate of the texrun rc until the limits
    /// are set with `prlimit(2)` (Linux only; the rc needs
    /// [`crate::rc::RcOptions::stdin_gate`]).
    StdinGate,
    /// No exec gate and no `prlimit(2)` (macOS without a gate): latexmk
    /// runs without rlimits; only the output size check applies.
    Unlimited,
    /// In a container (`texrun-sandbox`): the runtime sets the rlimits
    /// (including `RLIMIT_AS`, the container is Linux) and the limits of
    /// the container's cgroup before latexmk starts. The rc has no start
    /// gate, and no host cgroup is used.
    Container(&'a Container<'a>),
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
    pub(crate) start: Start<'a>,
    /// Run latexmk in a cgroup of its own (unless [`Start::Plain`]).
    pub(crate) cgroups: Option<&'a Cgroups>,
}

/// Result of a supervised run (see [`texrun_process::Finished`]).
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts about one finished run, not states"
)]
pub(crate) struct Finished {
    pub(crate) pid: u32,
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stop: Option<StopReason>,
    pub(crate) elapsed: Duration,
    pub(crate) stdout: CapturedOutput,
    pub(crate) stderr: CapturedOutput,
    /// Whether all rlimits were set before latexmk started.
    pub(crate) rlimits_applied: bool,
    pub(crate) cgroup: CgroupOutcome,
    /// Whether `RLIMIT_AS` was in place (Linux, or a container), so that an
    /// allocation failure may be that limit.
    pub(crate) address_space_limited: bool,
    /// The container's memory limit stopped a process (OOM kill).
    pub(crate) container_oom: bool,
    /// The container's process limit refused a new process (as reported
    /// from inside the container,
    /// `texrun_sandbox::ContainerSpec::report_pids`).
    pub(crate) container_pids_limit: bool,
}

/// Runs latexmk and supervises it until it exits or is stopped.
pub(crate) fn run(job: &Job<'_>) -> Result<Finished, RunError> {
    let cap = job.limits.max_captured_bytes;
    let mut spec = Spec::new(job.program, Cwd::Path(job.cwd))
        .with_args(job.args.iter().cloned())
        .with_env(job.env.clone())
        .with_stdout(Capture::Keep(cap))
        .with_stderr(Capture::Keep(cap));
    let timeout = job.timeout.unwrap_or(Duration::MAX);
    match &job.start {
        Start::Plain | Start::Unlimited => {}
        Start::Container(_) => {
            spec = spec.with_rlimits(job.limits.rlimits_for(timeout, true));
        }
        Start::ExecGate(gate) => {
            // A gate that became unusable since it was checked fails the
            // run (nothing is started) instead of falling back to setting
            // the limits after the start; the engine then uses the rc's
            // start gate or reports it.
            spec = spec
                .with_rlimits(job.limits.rlimits(timeout))
                .with_start(StartMode::ExecGate((*gate).clone().with_required(true)));
        }
        Start::StdinGate => {
            spec = spec
                .with_rlimits(job.limits.rlimits(timeout))
                // The gate is only used where the limits can be applied;
                // never release it without them.
                .with_require_rlimits(true)
                .with_start(StartMode::StdinGate {
                    token: crate::rc::START_TOKEN.to_vec(),
                });
        }
    }
    if !matches!(job.start, Start::Plain | Start::Container(_))
        && let Some(cgroups) = job.cgroups
    {
        spec = spec.with_cgroup(cgroups, job.limits.cgroup());
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

    let (finished, address_space_limited, container_oom, container_pids_limit) =
        if let Start::Container(container) = &job.start {
            let mut finished = texrun_process::run_with(*container, &spec, watch)?;
            let outcome = container.outcome();
            let oom = outcome.is_some_and(|o| o.oom_killed);
            // Stopped by texrun: the container told when it was stopped;
            // ended on its own: in the last line of its stderr, which is
            // removed from the output either way.
            let reported = container.take_pids_report(&mut finished.stderr);
            let pids = outcome
                .and_then(|o| o.pids_limit_reached)
                .or(reported)
                .unwrap_or(false);
            (finished, true, oom, pids)
        } else {
            let finished = texrun_process::run(&spec, watch)?;
            let linux = cfg!(any(target_os = "linux", target_os = "android"));
            let limited = linux && finished.rlimits_applied;
            (finished, limited, false, false)
        };
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
        rlimits_applied: finished.rlimits_applied,
        cgroup: finished.cgroup,
        address_space_limited,
        container_oom,
        container_pids_limit,
    })
}

/// The engine error for a run that could not be started or supervised.
pub(crate) fn engine_error(e: RunError) -> EngineError {
    match e {
        RunError::Spawn { program, source } => EngineError::Spawn { program, source },
        RunError::Io { context, source } => EngineError::Io { context, source },
        // E.g. the start gate required without `prlimit(2)`, or a required
        // cgroup that cannot be used.
        RunError::Unsupported(reason) => EngineError::Unsupported(reason),
        other => EngineError::Io {
            context: "running latexmk".to_owned(),
            source: std::io::Error::other(other.to_string()),
        },
    }
}

/// Checks the output size limits once.
pub(crate) fn check_output_size(dirs: &[&Path], limits: &Limits) -> Option<StopReason> {
    let size = layout::tree_size(dirs);
    if size.largest >= limits.max_file_bytes {
        Some(per_file_limit(limits))
    } else if size.total > limits.max_output_bytes {
        Some(StopReason::OutputLimit(format!(
            "output limit exceeded: the output directory exceeded {} bytes",
            limits.max_output_bytes
        )))
    } else {
        None
    }
}

fn per_file_limit(limits: &Limits) -> StopReason {
    StopReason::OutputLimit(format!(
        "output limit exceeded: a file reached the per-file limit of {} bytes",
        limits.max_file_bytes
    ))
}

/// The resource limit latexmk or one of its processes reached, as a
/// message, judging from how latexmk ended and what its cgroup recorded.
///
/// - `SIGXCPU` ends a process at the CPU soft limit. The texrun rc makes
///   latexmk exit with `128 + signal` when a program it ran ends with
///   `SIGXCPU` or `SIGXFSZ` (a status TeX cannot produce: latexmk's own
///   statuses are below 128);
/// - `SIGXFSZ` ends a process that writes past `RLIMIT_FSIZE`; reported
///   like the output size check;
/// - the cgroup counts OOM kills (`memory.max`) and refused new processes
///   (`pids.max`).
pub(crate) fn limit_reached(
    finished: &Finished,
    log: &[u8],
    limits: &Limits,
    timeout: Duration,
) -> Option<LimitReached> {
    use std::os::unix::process::ExitStatusExt;

    let signal = finished
        .status
        .signal()
        .or_else(|| finished.status.code().filter(|&c| c > 128).map(|c| c - 128));
    if finished.container_oom {
        return Some(LimitReached::fatal(format!(
            "resource limit exceeded: the compile needed more than {} bytes of memory (all its \
             processes together), so a process was stopped",
            limits.max_memory_bytes
        )));
    }
    let pids_limit = || {
        // A refused `fork` may have been retried successfully.
        Some(LimitReached::Resource {
            message: format!(
                "resource limit exceeded: the compile tried to run more than {} processes and \
                 threads at once",
                limits.max_processes
            ),
            transient: true,
        })
    };
    if finished.container_pids_limit {
        return pids_limit();
    }
    if let CgroupOutcome::Applied(usage) = &finished.cgroup {
        if usage.oom_kills > 0 {
            return Some(LimitReached::fatal(format!(
                "resource limit exceeded: the compile needed more than {} bytes of memory \
                 (all its processes together), so it was stopped",
                limits.max_memory_bytes
            )));
        }
        if usage.pids_max_hits > 0 {
            return pids_limit();
        }
    }
    if signal == Some(SIGXCPU) {
        return Some(LimitReached::fatal(format!(
            "resource limit exceeded: a process of the compile used more than {} s of CPU time",
            limits.cpu_seconds(timeout)
        )));
    }
    if signal == Some(SIGXFSZ) {
        return Some(LimitReached::Output(per_file_limit(limits)));
    }
    if finished.address_space_limited
        && !finished.status.success()
        && ran_out_of_memory(&finished.stderr.bytes, log)
    {
        return Some(LimitReached::fatal(format!(
            "resource limit exceeded: a process of the compile ran out of memory (at most {} \
             bytes of address space per process)",
            limits.max_address_space
        )));
    }
    None
}

/// Whether `stderr` has the message of a program whose allocation failed
/// (`RLIMIT_AS`), as a whole line: kpathsea's `xmalloc` (the TeX
/// programs) or perl (latexmk). A line that also appears in the main `log`
/// does not count: those messages go to stderr only, while text a document
/// makes TeX or latexmk print (e.g. a label) also ends up in the log. So a
/// document cannot make an ordinary failure look like this limit.
pub(crate) fn ran_out_of_memory(stderr: &[u8], log: &[u8]) -> bool {
    let log = String::from_utf8_lossy(log);
    String::from_utf8_lossy(stderr).lines().any(|line| {
        let line = line.trim_end_matches('\r');
        let message = (line.starts_with("fatal: memory exhausted (xmalloc of ")
            && line.ends_with(" bytes)."))
            || line == "Out of memory!"
            || line.starts_with("Out of memory in perl:")
            || line.starts_with("Out of memory during ");
        message && !log.lines().any(|l| l.trim_end_matches('\r') == line)
    })
}

/// A limit reached, from [`limit_reached`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LimitReached {
    /// The per-file output limit (reported like the output size check).
    Output(StopReason),
    /// CPU time, memory or processes, with the message. `transient`: the
    /// compile may still have succeeded (a refused process start that was
    /// retried); it then stays successful, with a warning.
    Resource { message: String, transient: bool },
}

impl LimitReached {
    /// A limit that stopped a process of the compile.
    fn fatal(message: String) -> Self {
        Self::Resource {
            message,
            transient: false,
        }
    }
}

const SIGXCPU: i32 = rustix::process::Signal::XCPU.as_raw();
const SIGXFSZ: i32 = rustix::process::Signal::XFSZ.as_raw();

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
    fn cpu_time_follows_the_timeout_by_default() {
        let limits = Limits::default();
        assert_eq!(limits.cpu_seconds(Duration::from_secs(60)), 70);
        assert_eq!(limits.cpu_seconds(Duration::from_millis(500)), 11);
        assert_eq!(limits.cpu_seconds(Duration::MAX), u64::from(u32::MAX));
        let fixed = limits.with_max_cpu_time(Some(Duration::from_millis(1500)));
        assert_eq!(fixed.cpu_seconds(Duration::from_secs(60)), 2);
        assert_eq!(
            limits
                .with_max_cpu_time(Some(Duration::ZERO))
                .cpu_seconds(Duration::ZERO),
            1
        );
        let rlimits = limits.rlimits(Duration::from_secs(60));
        assert_eq!(rlimits.get_soft_hard(Resource::Cpu), Some((70, 75)));
        assert_eq!(rlimits.get(Resource::Core), Some(0));
        assert_eq!(
            rlimits.get(Resource::FileSize),
            Some(Limits::DEFAULT_MAX_FILE_BYTES)
        );
        assert_eq!(
            rlimits.get(Resource::AddressSpace).is_some(),
            cfg!(target_os = "linux")
        );
    }

    fn finished(status: i32, cgroup: CgroupOutcome) -> Finished {
        use std::os::unix::process::ExitStatusExt;
        Finished {
            pid: 1,
            status: std::process::ExitStatus::from_raw(status),
            stop: None,
            elapsed: Duration::ZERO,
            stdout: CapturedOutput::default(),
            stderr: CapturedOutput::default(),
            rlimits_applied: true,
            cgroup,
            address_space_limited: cfg!(target_os = "linux"),
            container_oom: false,
            container_pids_limit: false,
        }
    }

    #[test]
    fn reached_limits_are_told_apart() {
        let limits = Limits::default();
        let t = Duration::from_secs(60);
        let reached = |status, cgroup| limit_reached(&finished(status, cgroup), b"", &limits, t);
        let none = CgroupOutcome::NotRequested;
        // Killed by SIGXCPU, or latexmk exiting with 128 + SIGXCPU.
        for status in [SIGXCPU, (128 + SIGXCPU) << 8] {
            assert!(matches!(
                reached(status, none.clone()),
                Some(LimitReached::Resource { message: m, transient: false }) if m.contains("70 s of CPU time")
            ));
        }
        assert!(matches!(
            reached((128 + SIGXFSZ) << 8, none.clone()),
            Some(LimitReached::Output(StopReason::OutputLimit(m))) if m.contains("per-file")
        ));
        // Ordinary failures and our own SIGKILL are no limit.
        for status in [0, 1 << 8, 12 << 8, 9] {
            assert_eq!(reached(status, none.clone()), None, "{status}");
        }
        let mut usage = texrun_process::CgroupUsage::default();
        assert_eq!(reached(9, CgroupOutcome::Applied(usage)), None);
        usage.oom_kills = 1;
        assert!(matches!(
            reached(9, CgroupOutcome::Applied(usage)),
            Some(LimitReached::Resource { message: m, transient: false }) if m.contains("memory")
        ));
        usage.oom_kills = 0;
        usage.pids_max_hits = 3;
        assert!(matches!(
            reached(12 << 8, CgroupOutcome::Applied(usage)),
            Some(LimitReached::Resource { message: m, transient: true }) if m.contains("64 processes")
        ));
        // Reported from inside a container, whatever the exit (e.g. the
        // timeout's kill).
        let mut in_container = finished(9, CgroupOutcome::NotRequested);
        in_container.container_pids_limit = true;
        assert!(matches!(
            limit_reached(&in_container, b"", &limits, t),
            Some(LimitReached::Resource { message: m, transient: true }) if m.contains("64 processes")
        ));
    }

    const XMALLOC: &str = "fatal: memory exhausted (xmalloc of 40000008 bytes).";

    #[test]
    fn out_of_memory_needs_the_whole_line_on_stderr_only() {
        let yes = |stderr: &str, log: &str| ran_out_of_memory(stderr.as_bytes(), log.as_bytes());
        // The programs' own messages, as whole lines.
        assert!(yes(&format!("Latexmk: x\n{XMALLOC}\n"), ""));
        assert!(yes("Out of memory!\n", ""));
        assert!(yes("Out of memory in perl:util:safesysmalloc\n", ""));
        assert!(yes("Out of memory during request for 64 bytes\n", ""));
        // Only part of a line (e.g. a name a document chose, which latexmk
        // prints indented or after a prefix).
        for stderr in [
            format!("Latexmk: Reference `{XMALLOC}' undefined\n"),
            format!("  {XMALLOC}\n"),
            "Latexmk: label Out of memory!\n".to_owned(),
            "Out of memory!!\n".to_owned(),
            "fatal: memory exhausted (xmalloc of 10 bytes). more\n".to_owned(),
        ] {
            assert!(!yes(&stderr, ""), "{stderr}");
        }
        // The same line in the log came from the document.
        assert!(!yes(&format!("{XMALLOC}\n"), &format!("x\n{XMALLOC}\ny\n")));
        assert!(!yes("Out of memory!\n", "Out of memory!\n"));
    }

    #[test]
    fn an_out_of_memory_message_is_a_limit_only_for_a_failed_compile() {
        let limits = Limits::default();
        let t = Duration::from_secs(60);
        let mut failed = finished(1 << 8, CgroupOutcome::NotRequested);
        failed.stderr.bytes = format!("{XMALLOC}\n").into_bytes();
        let reached = limit_reached(&failed, b"", &limits, t);
        assert_eq!(
            matches!(
                &reached,
                Some(LimitReached::Resource { message, transient: false })
                    if message.contains("ran out of memory")
            ),
            cfg!(target_os = "linux"),
            "{reached:?}"
        );
        // Not when the line is also in the log.
        let log = format!("{XMALLOC}\n");
        assert_eq!(limit_reached(&failed, log.as_bytes(), &limits, t), None);
        // Not for a document whose text contains it.
        failed.stderr.bytes = format!("Latexmk: Reference `{XMALLOC}' undefined\n").into_bytes();
        assert_eq!(limit_reached(&failed, b"", &limits, t), None);
        // Not for a compile that succeeded.
        let mut ok = finished(0, CgroupOutcome::NotRequested);
        ok.stderr.bytes = format!("{XMALLOC}\n").into_bytes();
        assert_eq!(limit_reached(&ok, b"", &limits, t), None);
    }

    fn job<'a>(program: &'a Path, args: &[&str], cwd: &'a Path) -> Job<'a> {
        Job {
            program,
            args: args.iter().map(Into::into).collect(),
            cwd,
            env: EnvAllowlist::new().with("PATH", "/usr/bin:/bin"),
            timeout: Some(Duration::from_secs(20)),
            cancel: None,
            size_dirs: Vec::new(),
            limits: Limits::default(),
            start: Start::Plain,
            cgroups: None,
        }
    }

    #[test]
    fn engine_errors_keep_the_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(&job(
            Path::new("/nonexistent/texrun-test-program"),
            &[],
            dir.path(),
        ))
        .unwrap_err();
        assert!(
            matches!(engine_error(err), EngineError::Spawn { .. }),
            "spawn error"
        );
    }

    /// The rc's start gate needs `prlimit(2)`; without it the run is
    /// refused as unsupported, not as an I/O error.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    #[test]
    fn a_gate_without_prlimit_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let mut job = job(Path::new("/bin/sh"), &["-c", "touch ran"], dir.path());
        job.start = Start::StdinGate;
        let err = engine_error(run(&job).unwrap_err());
        assert!(matches!(err, EngineError::Unsupported(_)), "{err:?}");
        assert!(!dir.path().join("ran").exists());
    }

    #[test]
    fn an_exceeded_output_limit_stops_latexmk() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let mut job = job(
            Path::new("/bin/sh"),
            &["-c", "head -c 5000 /dev/zero > out/big; sleep 30"],
            dir.path(),
        );
        job.size_dirs = vec![out.as_path()];
        job.limits = Limits::default()
            .with_max_output_bytes(1000)
            .with_size_check_interval(Duration::from_millis(20));
        let done = run(&job).unwrap();
        assert!(
            matches!(&done.stop, Some(StopReason::OutputLimit(m)) if m.contains("output directory")),
            "{:?}",
            done.stop
        );
    }
}
