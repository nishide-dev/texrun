//! Supervisor behavior with small shell scripts standing in for the real
//! programs (no TeX or preview tool needed).

use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use texrun_core::CancelToken;
use texrun_process::{Capture, Cwd, EnvAllowlist, RunError, Spec, StartMode, Stop, Watch, run};
#[cfg(target_os = "linux")]
use texrun_process::{Resource, Rlimits};

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

#[test]
fn a_held_directory_is_the_working_directory() {
    use std::os::fd::AsFd;

    let root = tempfile::tempdir().unwrap();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let dir = root_path.join("work");
    fs::create_dir(&dir).unwrap();
    let fd = fs::File::open(&dir).unwrap();
    let spec = |path| {
        Spec::new(
            "/bin/sh",
            Cwd::Dir {
                fd: fd.as_fd(),
                path,
            },
        )
        .with_args(["-c", "pwd -P"])
        .with_env(EnvAllowlist::new().with("PATH", "/usr/bin:/bin"))
    };
    let done = run(&spec(&dir), Watch::<()>::new()).unwrap();
    assert_eq!(done.stdout.bytes, format!("{}\n", dir.display()).as_bytes());

    // The directory is moved away and its name reused: the child must not
    // run in the new directory of that name.
    let moved = root_path.join("moved");
    fs::rename(&dir, &moved).unwrap();
    fs::create_dir(&dir).unwrap();
    match run(&spec(&dir), Watch::<()>::new()) {
        // Linux: the descriptor itself.
        Ok(done) => assert_eq!(
            done.stdout.bytes,
            format!("{}\n", moved.display()).as_bytes()
        ),
        // Without `/proc`: the path is checked and refused.
        Err(RunError::Io { .. }) => {}
        Err(e) => panic!("{e:?}"),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// The (soft) limit called `name` in `/proc/<pid>/limits` output.
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

    #[test]
    fn rlimits_are_set_before_a_gated_child_starts() {
        let dir = tempfile::tempdir().unwrap();
        let spec = sh(dir.path(), "read t && cat /proc/$$/limits")
            .with_rlimits(
                Rlimits::new()
                    .with(Resource::FileSize, 123_456)
                    .with(Resource::Core, 0)
                    .with(Resource::AddressSpace, 3 << 30)
                    .with(Resource::Cpu, 600),
            )
            .with_start(StartMode::StdinGate {
                token: b"go\n".to_vec(),
            });
        let done = run(&spec, Watch::<()>::new()).unwrap();
        assert!(done.status.success());
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

    #[test]
    fn rlimits_are_capped_at_our_hard_limit() {
        use rustix::process::{Resource as R, getrlimit};
        let hard = getrlimit(R::Core).maximum;
        let dir = tempfile::tempdir().unwrap();
        let spec = sh(dir.path(), "read t && cat /proc/$$/limits")
            .with_rlimits(Rlimits::new().with(Resource::Core, u64::MAX - 1))
            .with_start(StartMode::StdinGate {
                token: b"go\n".to_vec(),
            });
        let done = run(&spec, Watch::<()>::new()).unwrap();
        assert!(done.status.success(), "no privilege is needed");
        let limits = String::from_utf8(done.stdout.bytes).unwrap();
        let expected = hard.map_or_else(|| (u64::MAX - 1).to_string(), |h| h.to_string());
        let got = limit(&limits, "Max core file size");
        assert!(got == expected || got == "unlimited", "{got} vs {expected}");
    }

    #[test]
    fn the_file_size_limit_stops_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let spec = sh(dir.path(), "read t && head -c 100000 /dev/zero > big")
            .with_rlimits(
                Rlimits::new()
                    .with(Resource::FileSize, 1000)
                    .with(Resource::Core, 0),
            )
            .with_start(StartMode::StdinGate {
                token: b"go\n".to_vec(),
            });
        let done = run(
            &spec,
            Watch::<()>::new().with_timeout(Duration::from_secs(20)),
        )
        .unwrap();
        assert!(!done.status.success());
        assert_eq!(fs::metadata(dir.path().join("big")).unwrap().len(), 1000);
        assert!(!dir.path().join("core").exists());
    }
}
