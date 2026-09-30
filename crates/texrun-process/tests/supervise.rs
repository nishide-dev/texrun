//! Supervisor behavior with small shell scripts standing in for the real
//! programs (no TeX or preview tool needed).

use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use texrun_core::CancelToken;
use texrun_process::{
    Capture, Cwd, EnvAllowlist, HostLauncher, Launcher, PRLIMIT_SUPPORTED, Resource, Rlimits,
    RunError, Spec, StartMode, Stop, Watch, run, run_with,
};

/// `/bin/sh -c <script>` in `cwd`, with a minimal environment.
fn sh<'a>(cwd: &'a Path, script: &str) -> Spec<'a> {
    Spec::new("/bin/sh", Cwd::Path(cwd))
        .with_args(["-c", script])
        .with_env(EnvAllowlist::new().with("PATH", "/usr/bin:/bin"))
}

/// Whether a live (non-zombie) process of group `pgid` exists, waiting up to
/// 2 s for `SIGKILL` to take effect. Zombies are ignored: in a container
/// without an init process, killed grandchildren are reparented to a PID 1
/// that may never reap them.
fn group_alive(pgid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let out = Command::new("ps")
            .args(["-A", "-o", "pgid=,stat="])
            .output()
            .expect("ps");
        let alive = String::from_utf8_lossy(&out.stdout).lines().any(|l| {
            let mut f = l.split_whitespace();
            f.next() == Some(&pgid.to_string()) && !f.next().unwrap_or("").starts_with('Z')
        });
        if !alive || Instant::now() >= deadline {
            return alive;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `pid` is still a child of this process (alive or a zombie), i.e.
/// was not reaped.
fn is_unreaped_child(pid: u32) -> bool {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let pid = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    !matches!(
        waitid(WaitId::Pid(pid), options),
        Err(rustix::io::Errno::CHILD)
    )
}

fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = fs::read_to_string(path)
            && s.ends_with('\n')
        {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "{} was not written",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn normal_exit_is_reported_captured_and_reaped() {
    let dir = tempfile::tempdir().unwrap();
    let done = run(
        &sh(dir.path(), "echo out; echo err >&2; exit 3"),
        Watch::<()>::new().with_timeout(Duration::from_secs(20)),
    )
    .unwrap();
    assert_eq!(done.stop, None);
    assert_eq!(done.status.code(), Some(3));
    assert_eq!(done.stdout.bytes, b"out\n");
    assert_eq!(done.stderr.bytes, b"err\n");
    assert!(!is_unreaped_child(done.pid), "the leader must be reaped");
}

#[test]
fn timeout_kills_the_whole_group() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let done = run(
        &sh(dir.path(), "sleep 30 & wait"),
        Watch::<()>::new().with_timeout(Duration::from_millis(300)),
    )
    .unwrap();
    assert_eq!(done.stop, Some(Stop::TimedOut));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!group_alive(done.pid), "grandchild `sleep` must be gone");
    assert!(!is_unreaped_child(done.pid));
}

#[test]
fn an_earlier_deadline_wins_over_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let done = run(
        &sh(dir.path(), "sleep 30 & wait"),
        Watch::<()>::new()
            .with_timeout(Duration::from_secs(60))
            .with_deadline(Instant::now() + Duration::from_millis(200)),
    )
    .unwrap();
    assert_eq!(done.stop, Some(Stop::TimedOut));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!group_alive(done.pid));
}

#[test]
fn cancel_kills_the_whole_group() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    let c = cancel.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        c.cancel();
    });
    let done = run(
        &sh(dir.path(), "sleep 30 & wait"),
        Watch::<()>::new().with_cancel(&cancel),
    )
    .unwrap();
    t.join().unwrap();
    assert_eq!(done.stop, Some(Stop::Cancelled));
    assert!(!group_alive(done.pid));
    assert!(!is_unreaped_child(done.pid));
}

#[test]
fn a_check_hook_stops_the_group_with_its_value() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("grown");
    let mut calls = 0;
    let done = run(
        &sh(dir.path(), "(sleep 0.2; echo x > grown) & sleep 30"),
        Watch::new()
            .with_timeout(Duration::from_secs(20))
            .with_check(Duration::ZERO, || {
                calls += 1;
                marker.exists().then_some("too large")
            }),
    )
    .unwrap();
    assert_eq!(done.stop, Some(Stop::Check("too large")));
    assert!(calls > 1);
    assert!(!group_alive(done.pid));
}

#[test]
fn descendants_are_killed_after_a_normal_exit() {
    let dir = tempfile::tempdir().unwrap();
    let done = run(
        &sh(dir.path(), "sleep 30 >/dev/null 2>&1 & echo $!"),
        Watch::<()>::new().with_timeout(Duration::from_secs(20)),
    )
    .unwrap();
    assert_eq!(done.stop, None);
    assert!(done.status.success());
    let grandchild: u32 = String::from_utf8_lossy(&done.stdout.bytes)
        .trim()
        .parse()
        .unwrap();
    assert_ne!(grandchild, done.pid);
    assert!(!group_alive(done.pid), "the left-over `sleep` must be gone");
}

#[test]
fn a_panicking_check_still_kills_and_reaps() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let spec = sh(dir.path(), "echo $$ > pid; sleep 30 & wait");
    let result = catch_unwind(AssertUnwindSafe(|| {
        run(
            &spec,
            Watch::new()
                .with_timeout(Duration::from_secs(20))
                .with_check(Duration::ZERO, || -> Option<()> {
                    assert!(!pid_file.exists(), "check hook fails");
                    None
                }),
        )
    }));
    assert!(result.is_err(), "the panic propagates");
    let pid: u32 = wait_for_file(&pid_file).trim().parse().unwrap();
    assert!(!group_alive(pid), "the guard kills the group");
    assert!(!is_unreaped_child(pid), "the guard reaps the leader");
}

#[test]
fn output_is_capped_but_drained() {
    let dir = tempfile::tempdir().unwrap();
    let done = run(
        &sh(
            dir.path(),
            "head -c 300000 /dev/zero; head -c 10 /dev/zero >&2",
        )
        .with_stdout(Capture::Keep(1000))
        .with_stderr(Capture::Discard),
        Watch::<()>::new().with_timeout(Duration::from_secs(20)),
    )
    .unwrap();
    assert!(done.status.success());
    assert_eq!(done.stdout.bytes.len(), 1000);
    assert_eq!(done.stdout.total_bytes, 300_000);
    assert!(done.stdout.is_truncated());
    assert_eq!(done.stderr.total_bytes, 0);
}

#[test]
fn missing_program_is_a_spawn_error() {
    let dir = tempfile::tempdir().unwrap();
    let spec = Spec::new("/nonexistent/texrun-test-program", Cwd::Path(dir.path()));
    let err = run(&spec, Watch::<()>::new()).unwrap_err();
    assert!(matches!(err, RunError::Spawn { .. }), "{err:?}");
}

#[test]
fn the_environment_is_exactly_the_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let spec = Spec::new("/usr/bin/env", Cwd::Path(dir.path())).with_env(
        EnvAllowlist::new()
            .with("HOME", "/nonexistent-home")
            .with_path(std::ffi::OsStr::new(".:/usr/bin:rel")),
    );
    let done = run(&spec, Watch::<()>::new()).unwrap();
    let mut vars: Vec<_> = String::from_utf8(done.stdout.bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    vars.sort();
    assert_eq!(vars, ["HOME=/nonexistent-home", "PATH=/usr/bin"]);
}

#[test]
fn stdin_is_empty_unless_gated() {
    let dir = tempfile::tempdir().unwrap();
    let script = "if read t; then echo \"got $t\"; else echo eof; fi";
    let done = run(&sh(dir.path(), script), Watch::<()>::new()).unwrap();
    assert_eq!(done.stdout.bytes, b"eof\n");

    let gated = sh(dir.path(), script).with_start(StartMode::StdinGate {
        token: b"go\n".to_vec(),
    });
    let done = run(&gated, Watch::<()>::new()).unwrap();
    assert_eq!(done.stdout.bytes, b"got go\n");
}

/// Runs `pwd -P` in the held directory `fd` (named `path`).
fn pwd_in(fd: &fs::File, path: &Path) -> Result<Vec<u8>, RunError> {
    use std::os::fd::AsFd;
    let spec = Spec::new(
        "/bin/sh",
        Cwd::Dir {
            fd: fd.as_fd(),
            path,
        },
    )
    .with_args(["-c", "pwd -P"])
    .with_env(EnvAllowlist::new().with("PATH", "/usr/bin:/bin"));
    run(&spec, Watch::<()>::new()).map(|done| done.stdout.bytes)
}

#[test]
fn a_held_directory_is_the_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let dir = root_path.join("work");
    fs::create_dir(&dir).unwrap();
    let fd = fs::File::open(&dir).unwrap();
    assert_eq!(
        pwd_in(&fd, &dir).unwrap(),
        format!("{}\n", dir.display()).as_bytes()
    );

    // The directory is moved away and its name reused: the child must not
    // run in the new directory of that name.
    let moved = root_path.join("moved");
    fs::rename(&dir, &moved).unwrap();
    fs::create_dir(&dir).unwrap();
    let result = pwd_in(&fd, &dir);
    if cfg!(target_os = "linux") {
        // The child changes into the descriptor itself.
        assert_eq!(result.unwrap(), format!("{}\n", moved.display()).as_bytes());
    } else {
        // Without `/proc`: the path is checked and refused.
        assert!(matches!(result, Err(RunError::Io { .. })), "{result:?}");
    }
}

#[test]
fn a_long_gate_token_is_refused_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let spec = sh(dir.path(), "touch ran").with_start(StartMode::StdinGate {
        token: vec![b'x'; StartMode::MAX_TOKEN_LEN + 1],
    });
    let err = run(&spec, Watch::<()>::new()).unwrap_err();
    assert!(matches!(err, RunError::InvalidSpec(_)), "{err:?}");
    assert!(!marker.exists());
}

/// Records the hook calls of a [`HostLauncher`].
#[derive(Default)]
struct Recording {
    fail_on_spawn: bool,
    apply_rlimits: bool,
    calls: std::sync::Mutex<Vec<String>>,
}

impl Recording {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Launcher for Recording {
    fn command(&self, spec: &Spec<'_>) -> Result<Command, RunError> {
        self.calls.lock().unwrap().push("command".to_owned());
        HostLauncher.command(spec)
    }
    fn apply_rlimits(&self) -> bool {
        self.apply_rlimits
    }
    fn on_spawn(&self, pid: u32) -> std::io::Result<()> {
        self.calls.lock().unwrap().push(format!("spawn {pid}"));
        if self.fail_on_spawn {
            Err(std::io::Error::other("attach failed"))
        } else {
            Ok(())
        }
    }
    fn on_kill(&self, pid: u32) {
        self.calls.lock().unwrap().push(format!("kill {pid}"));
    }
    fn on_reaped(&self, pid: u32) {
        self.calls.lock().unwrap().push(format!("reaped {pid}"));
    }
}

#[test]
fn launcher_hooks_are_called_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let launcher = Recording::default();
    let done = run_with(&launcher, &sh(dir.path(), "exit 0"), Watch::<()>::new()).unwrap();
    let pid = done.pid;
    assert_eq!(
        launcher.calls(),
        [
            "command".to_owned(),
            format!("spawn {pid}"),
            format!("kill {pid}"),
            format!("reaped {pid}"),
        ]
    );
}

#[test]
fn a_failing_on_spawn_kills_and_reaps() {
    let dir = tempfile::tempdir().unwrap();
    let launcher = Recording {
        fail_on_spawn: true,
        ..Recording::default()
    };
    let err = run_with(
        &launcher,
        &sh(dir.path(), "sleep 30 & wait"),
        Watch::<()>::new(),
    )
    .unwrap_err();
    assert!(matches!(err, RunError::Io { .. }), "{err:?}");
    let calls = launcher.calls();
    let pid: u32 = calls[1].strip_prefix("spawn ").unwrap().parse().unwrap();
    assert_eq!(calls[2..], [format!("kill {pid}"), format!("reaped {pid}")]);
    assert!(!group_alive(pid));
    assert!(!is_unreaped_child(pid));
}

#[test]
fn rlimits_are_reported_as_applied_or_not() {
    let dir = tempfile::tempdir().unwrap();
    let limited = sh(dir.path(), "exit 0").with_rlimits(Rlimits::new().with(Resource::Core, 0));
    let done = run(&limited, Watch::<()>::new()).unwrap();
    assert_eq!(done.rlimits_applied, PRLIMIT_SUPPORTED);

    // A launcher that applies them itself.
    let launcher = Recording::default();
    let done = run_with(&launcher, &limited, Watch::<()>::new()).unwrap();
    assert!(!done.rlimits_applied);

    let done = run(&sh(dir.path(), "exit 0"), Watch::<()>::new()).unwrap();
    assert!(!done.rlimits_applied, "none requested");
}

#[cfg(not(target_os = "linux"))]
mod without_prlimit {
    use super::*;

    #[test]
    fn required_rlimits_are_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let spec = sh(dir.path(), "touch ran")
            .with_rlimits(
                Rlimits::new()
                    .with(Resource::FileSize, 1000)
                    .with(Resource::Core, 0),
            )
            .with_require_rlimits(true);
        let err = run(&spec, Watch::<()>::new()).unwrap_err();
        assert!(matches!(err, RunError::Unsupported(_)), "{err:?}");
        assert!(!dir.path().join("ran").exists(), "nothing was started");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// The limit called `name` in `/proc/<pid>/limits` output (soft and
    /// hard must be equal).
    fn limit(limits: &str, name: &str) -> String {
        let line = limits
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("{name} missing in {limits}"));
        // Columns: name, soft, hard, units.
        let mut cols: Vec<_> = line[name.len()..].split_whitespace().collect();
        if cols.len() == 3 {
            cols.pop();
        }
        assert_eq!(cols[0], cols[1], "soft and hard limit are equal: {line}");
        cols[0].to_owned()
    }

    fn gated(spec: Spec<'_>) -> Spec<'_> {
        spec.with_start(StartMode::StdinGate {
            token: b"go\n".to_vec(),
        })
    }

    #[test]
    fn rlimits_are_set_before_a_gated_child_starts() {
        let dir = tempfile::tempdir().unwrap();
        let spec = gated(sh(dir.path(), "read t && cat /proc/$$/limits")).with_rlimits(
            Rlimits::new()
                .with(Resource::FileSize, 123_456)
                .with(Resource::Core, 0)
                .with(Resource::AddressSpace, 3 << 30)
                .with(Resource::Cpu, 600),
        );
        let done = run(&spec, Watch::<()>::new()).unwrap();
        assert!(done.status.success());
        assert!(done.rlimits_applied);
        let limits = String::from_utf8(done.stdout.bytes).unwrap();
        assert_eq!(limit(&limits, "Max file size"), "123456", "{limits}");
        assert_eq!(limit(&limits, "Max core file size"), "0", "{limits}");
        assert_eq!(
            limit(&limits, "Max address space"),
            (3u64 << 30).to_string(),
            "{limits}"
        );
        assert_eq!(limit(&limits, "Max cpu time"), "600", "{limits}");
    }

    /// With `Immediate`, the leader itself gets the limits, only later than
    /// its start: it polls its own limits (not those of a child, which may
    /// have been started before the limits arrived).
    #[test]
    fn rlimits_reach_an_immediate_leader() {
        let dir = tempfile::tempdir().unwrap();
        let script = "i=0; while [ $i -lt 500 ]; do \
                        grep -q '^Max file size *123456 ' /proc/$$/limits && break; \
                        i=$((i+1)); sleep 0.01; \
                      done; cat /proc/$$/limits";
        let spec = sh(dir.path(), script).with_rlimits(
            Rlimits::new()
                .with(Resource::FileSize, 123_456)
                .with(Resource::Core, 0),
        );
        let done = run(
            &spec,
            Watch::<()>::new().with_timeout(Duration::from_secs(20)),
        )
        .unwrap();
        assert!(done.status.success());
        let limits = String::from_utf8(done.stdout.bytes).unwrap();
        assert_eq!(limit(&limits, "Max file size"), "123456", "{limits}");
        assert_eq!(limit(&limits, "Max core file size"), "0", "{limits}");
    }

    #[test]
    fn rlimits_are_capped_at_our_hard_limit() {
        use rustix::process::{Resource as R, Rlimit, getrlimit, setrlimit};
        // Lower this process's own hard limit to a finite value, so that
        // the cap is checked even where it is unlimited. Only `RLIMIT_CORE`
        // is touched, which the other tests set to 0 anyway.
        let own = getrlimit(R::Core);
        let hard = own.maximum.map_or(1 << 20, |h| h.min(1 << 20));
        setrlimit(
            R::Core,
            Rlimit {
                // Soft = hard: the cap applies to both (see the unit tests).
                current: Some(hard),
                maximum: Some(hard),
            },
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let spec = gated(sh(dir.path(), "read t && cat /proc/$$/limits"))
            .with_rlimits(Rlimits::new().with(Resource::Core, u64::MAX - 1));
        let done = run(&spec, Watch::<()>::new()).unwrap();
        assert!(done.status.success(), "no privilege is needed");
        let limits = String::from_utf8(done.stdout.bytes).unwrap();
        assert_eq!(
            limit(&limits, "Max core file size"),
            hard.to_string(),
            "{limits}"
        );
    }

    #[test]
    fn a_launcher_can_take_over_the_rlimits() {
        let dir = tempfile::tempdir().unwrap();
        let spec = gated(sh(dir.path(), "read t && cat /proc/$$/limits")).with_rlimits(
            Rlimits::new()
                .with(Resource::FileSize, 123_456)
                .with(Resource::Core, 0),
        );
        let launcher = Recording::default(); // `apply_rlimits() == false`
        let done = run_with(&launcher, &spec, Watch::<()>::new()).unwrap();
        let limits = String::from_utf8(done.stdout.bytes).unwrap();
        let line = limits
            .lines()
            .find(|l| l.starts_with("Max file size"))
            .unwrap();
        assert!(!line.contains("123456"), "{line}");

        let launcher = Recording {
            apply_rlimits: true,
            ..Recording::default()
        };
        let done = run_with(&launcher, &spec, Watch::<()>::new()).unwrap();
        let limits = String::from_utf8(done.stdout.bytes).unwrap();
        assert_eq!(limit(&limits, "Max file size"), "123456", "{limits}");
    }

    #[test]
    fn the_file_size_limit_stops_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let spec = gated(sh(dir.path(), "read t && head -c 100000 /dev/zero > big")).with_rlimits(
            Rlimits::new()
                .with(Resource::FileSize, 1000)
                .with(Resource::Core, 0),
        );
        let done = run(
            &spec,
            Watch::<()>::new().with_timeout(Duration::from_secs(20)),
        )
        .unwrap();
        assert!(!done.status.success());
        assert_eq!(fs::metadata(dir.path().join("big")).unwrap().len(), 1000);
        assert!(!dir.path().join("core").exists());
    }

    #[test]
    fn required_rlimits_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        let spec = sh(dir.path(), "exit 0")
            .with_rlimits(Rlimits::new().with(Resource::Core, 0))
            .with_require_rlimits(true);
        assert!(run(&spec, Watch::<()>::new()).unwrap().rlimits_applied);
    }
}
