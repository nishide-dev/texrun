//! CPU, memory and process limits (docs/security.md §3.10): programs that
//! exceed them are stopped and the run returns.
//!
//! The cgroup tests need a delegated cgroup ([`Cgroups::detect`]). Without
//! one they are skipped, unless `TEXRUN_REQUIRE_CGROUP=1` (set by the CI
//! job that prepares one, docs/development.md), which turns the skip into a
//! failure.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use texrun_process::{
    CgroupLimits, CgroupOutcome, Cgroups, Cwd, EnvAllowlist, ExecGate, Resource, Rlimits, RunError,
    Spec, StartMode, Watch, run,
};

fn gate() -> ExecGate {
    ExecGate::new(env!("CARGO_BIN_EXE_texrun-exec-gate"))
}

/// `/bin/sh -c <script>` in `cwd` through the exec gate.
fn gated_sh<'a>(cwd: &'a Path, script: &str) -> Spec<'a> {
    Spec::new("/bin/sh", Cwd::Path(cwd))
        .with_args(["-c", script])
        .with_env(EnvAllowlist::new().with("PATH", "/usr/bin:/bin"))
        .with_start(StartMode::ExecGate(gate()))
}

fn watch() -> Watch<'static, ()> {
    Watch::new().with_timeout(Duration::from_secs(30))
}

const SIGXCPU: i32 = rustix::process::Signal::XCPU.as_raw();

#[test]
fn limits_whose_signal_dumps_core_need_core_zero() {
    let dir = tempfile::tempdir().unwrap();
    for resource in [Resource::Cpu, Resource::FileSize] {
        let spec = gated_sh(dir.path(), "touch ran").with_rlimits(Rlimits::new().with(resource, 5));
        let err = run(&spec, watch()).unwrap_err();
        assert!(matches!(err, RunError::InvalidSpec(_)), "{err:?}");
        assert!(!dir.path().join("ran").exists(), "nothing was started");
    }
}

/// A program that keeps the CPU busy is stopped by `SIGXCPU` at the soft
/// limit, long before the timeout.
#[test]
fn the_cpu_time_limit_stops_a_busy_program() {
    let dir = tempfile::tempdir().unwrap();
    let spec = gated_sh(dir.path(), "while :; do :; done").with_rlimits(
        Rlimits::new()
            .with_soft_hard(Resource::Cpu, 1, 3)
            .with(Resource::Core, 0),
    );
    let done = run(&spec, watch()).unwrap();
    assert_eq!(done.stop, None, "stopped by the limit, not the supervisor");
    assert_eq!(done.status.signal(), Some(SIGXCPU), "{:?}", done.status);
    assert!(done.elapsed < Duration::from_secs(20), "{:?}", done.elapsed);
    assert!(!dir.path().join("core").exists());
}

/// Soft and hard limits are passed separately.
#[test]
fn soft_and_hard_limits_differ() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(target_os = "linux")]
    let script = "grep '^Max cpu time' /proc/$$/limits";
    #[cfg(not(target_os = "linux"))]
    let script = "ulimit -St; ulimit -Ht";
    let spec = gated_sh(dir.path(), script).with_rlimits(
        Rlimits::new()
            .with_soft_hard(Resource::Cpu, 600, 605)
            .with(Resource::Core, 0),
    );
    let done = run(&spec, watch()).unwrap();
    let out = String::from_utf8(done.stdout.bytes).unwrap();
    let numbers: Vec<_> = out
        .split_whitespace()
        .filter(|w| w.parse::<u64>().is_ok())
        .collect();
    assert_eq!(numbers, ["600", "605"], "{out}");
}

/// `RLIMIT_AS` makes a large allocation fail; the program ends and the run
/// returns.
#[cfg(target_os = "linux")]
#[test]
fn the_address_space_limit_stops_a_large_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let spec = Spec::new("/usr/bin/perl", Cwd::Path(dir.path()))
        .with_args(["-e", "my $x = 'a' x (1 << 30); print length $x"])
        .with_start(StartMode::ExecGate(gate()))
        .with_rlimits(
            Rlimits::new()
                .with(Resource::AddressSpace, 256 << 20)
                .with(Resource::Core, 0),
        );
    let done = run(&spec, watch()).unwrap();
    assert!(!done.status.success(), "{:?}", done.status);
    assert!(done.stdout.bytes.is_empty());
    assert!(done.rlimits_applied);
}

#[test]
fn a_required_cgroup_that_cannot_be_used_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cgroups = Cgroups::unavailable("no delegated cgroup").with_required(true);
    let spec = gated_sh(dir.path(), "touch ran").with_cgroup(&cgroups, limits());
    let err = run(&spec, watch()).unwrap_err();
    assert!(
        matches!(&err, RunError::Unsupported(m) if m.contains("no delegated cgroup")),
        "{err:?}"
    );
    assert!(!dir.path().join("ran").exists());
}

#[test]
fn an_optional_cgroup_that_cannot_be_used_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let cgroups = Cgroups::unavailable("no delegated cgroup");
    let spec = gated_sh(dir.path(), "touch ran").with_cgroup(&cgroups, limits());
    let done = run(&spec, watch()).unwrap();
    assert!(done.status.success());
    assert!(dir.path().join("ran").exists());
    assert_eq!(
        done.cgroup,
        CgroupOutcome::Unavailable("no delegated cgroup".to_owned())
    );
}

fn limits() -> CgroupLimits {
    CgroupLimits::new()
        .with_memory_max(256 << 20)
        .with_pids_max(32)
        .with_cpus(1)
}

/// The delegated cgroup of this test process, or `None` (the test is
/// skipped) unless `TEXRUN_REQUIRE_CGROUP=1`.
#[cfg(target_os = "linux")]
fn cgroups() -> Option<&'static Cgroups> {
    use std::sync::OnceLock;
    static CGROUPS: OnceLock<Cgroups> = OnceLock::new();
    let cgroups = CGROUPS.get_or_init(Cgroups::detect);
    match cgroups.check() {
        Ok(()) => Some(cgroups),
        Err(reason) if std::env::var_os("TEXRUN_REQUIRE_CGROUP").is_some_and(|v| v == "1") => {
            panic!("TEXRUN_REQUIRE_CGROUP=1, but no cgroup can be used: {reason}")
        }
        Err(reason) => {
            eprintln!("skipped: no delegated cgroup ({reason})");
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn applied(outcome: &CgroupOutcome) -> texrun_process::CgroupUsage {
    match outcome {
        CgroupOutcome::Applied(usage) => *usage,
        other => panic!("not in a cgroup: {other:?}"),
    }
}

/// Whether process `pid` still exists (and is not a zombie).
#[cfg(target_os = "linux")]
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z'))
    })
}

#[cfg(target_os = "linux")]
fn wait_until_gone(pid: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// A child started at once is already in the run's cgroup, and the cgroup
/// is removed afterwards.
#[cfg(target_os = "linux")]
#[test]
fn cgroup_contains_the_program_from_the_start() {
    let Some(cgroups) = cgroups() else { return };
    let dir = tempfile::tempdir().unwrap();
    let spec = gated_sh(dir.path(), "cat /proc/self/cgroup").with_cgroup(cgroups, limits());
    let done = run(&spec, watch()).unwrap();
    assert!(done.status.success());
    applied(&done.cgroup);
    let out = String::from_utf8(done.stdout.bytes).unwrap();
    let prefix = format!("texrun-{}.", std::process::id());
    let name = out
        .trim()
        .rsplit('/')
        .next()
        .filter(|name| {
            name.strip_prefix(&prefix)
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        })
        .unwrap_or_else(|| panic!("not in a run cgroup: {out}"))
        .to_owned();
    assert!(
        !cgroups.parent().unwrap().join(&name).exists(),
        "the run cgroup {name} was removed"
    );
}

/// Memory beyond `memory.max` gets the run OOM-killed, and that is
/// reported.
#[cfg(target_os = "linux")]
#[test]
fn cgroup_memory_limit_stops_a_large_allocation() {
    let Some(cgroups) = cgroups() else { return };
    let dir = tempfile::tempdir().unwrap();
    let spec = Spec::new("/usr/bin/perl", Cwd::Path(dir.path()))
        .with_args(["-e", "my $x = 'a' x (512 << 20); print length $x"])
        .with_start(StartMode::ExecGate(gate()))
        .with_cgroup(cgroups, CgroupLimits::new().with_memory_max(64 << 20));
    let done = run(&spec, watch()).unwrap();
    assert!(!done.status.success(), "{:?}", done.status);
    let usage = applied(&done.cgroup);
    assert!(usage.oom_kills >= 1, "{usage:?}");
}

/// Starting processes beyond `pids.max` fails, is reported, and every
/// process started is gone after the run.
#[cfg(target_os = "linux")]
#[test]
fn cgroup_process_limit_stops_new_processes() {
    let Some(cgroups) = cgroups() else { return };
    let dir = tempfile::tempdir().unwrap();
    let script = "i=0; while [ $i -lt 40 ]; do sleep 30 & echo $!; i=$((i+1)); done; exit 0";
    let spec =
        gated_sh(dir.path(), script).with_cgroup(cgroups, CgroupLimits::new().with_pids_max(8));
    let done = run(&spec, watch()).unwrap();
    let usage = applied(&done.cgroup);
    assert!(usage.pids_max_hits >= 1, "{usage:?}");
    let started = String::from_utf8(done.stdout.bytes).unwrap();
    let pids: Vec<_> = started.split_whitespace().collect();
    assert!(!pids.is_empty() && pids.len() < 40, "{started}");
    for pid in pids {
        assert!(wait_until_gone(pid), "process {pid} is still running");
    }
}

/// A descendant in a session of its own is out of reach of the process
/// group kill, but not of `cgroup.kill`.
#[cfg(target_os = "linux")]
#[test]
fn cgroup_kill_reaches_a_process_outside_the_group() {
    let Some(cgroups) = cgroups() else { return };
    assert!(
        Path::new("/usr/bin/setsid").exists(),
        "setsid (util-linux) is needed for this test"
    );
    let dir = tempfile::tempdir().unwrap();
    let script = "setsid sleep 60 </dev/null >/dev/null 2>&1 & echo $!; exit 0";

    // Control: without a cgroup the process survives the run.
    let done = run(&gated_sh(dir.path(), script), watch()).unwrap();
    let pid = String::from_utf8(done.stdout.bytes)
        .unwrap()
        .trim()
        .to_owned();
    let survived = !wait_until_gone(&pid);
    if survived {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-9", &pid])
            .status();
    }
    assert!(
        survived,
        "the control process {pid} should have left the group"
    );

    let spec = gated_sh(dir.path(), script).with_cgroup(cgroups, limits());
    let done = run(&spec, watch()).unwrap();
    applied(&done.cgroup);
    let pid = String::from_utf8(done.stdout.bytes)
        .unwrap()
        .trim()
        .to_owned();
    assert!(wait_until_gone(&pid), "process {pid} is still running");
}

/// The cgroup's `cpu.max` throttles a busy program without stopping it;
/// the timeout still does.
#[cfg(target_os = "linux")]
#[test]
fn cgroup_cpu_limit_throttles_but_the_timeout_stops() {
    let Some(cgroups) = cgroups() else { return };
    let dir = tempfile::tempdir().unwrap();
    let spec = gated_sh(dir.path(), "while :; do :; done")
        .with_cgroup(cgroups, CgroupLimits::new().with_cpus(1));
    let start = Instant::now();
    let done = run(
        &spec,
        Watch::<()>::new().with_timeout(Duration::from_millis(500)),
    )
    .unwrap();
    assert_eq!(done.stop, Some(texrun_process::Stop::TimedOut));
    assert!(start.elapsed() < Duration::from_secs(10));
    applied(&done.cgroup);
}
