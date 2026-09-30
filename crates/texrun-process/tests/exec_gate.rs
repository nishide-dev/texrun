//! The exec gate (`StartMode::ExecGate`), with the `texrun-exec-gate`
//! binary of this crate.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use texrun_process::{
    Cwd, EnvAllowlist, ExecGate, HostLauncher, Launcher, Resource, Rlimits, RunError, Spec,
    StartMode, Watch, run, run_with,
};

fn gate() -> ExecGate {
    ExecGate::new(env!("CARGO_BIN_EXE_texrun-exec-gate"))
}

/// `/bin/sh -c <script>` in `cwd` through the gate.
fn gated_sh<'a>(cwd: &'a Path, script: &str) -> Spec<'a> {
    Spec::new("/bin/sh", Cwd::Path(cwd))
        .with_args(["-c", script])
        .with_env(EnvAllowlist::new().with("PATH", "/usr/bin:/bin"))
        .with_start(StartMode::ExecGate(gate()))
}

fn limits() -> Rlimits {
    Rlimits::new()
        .with(Resource::FileSize, 1 << 20)
        .with(Resource::Core, 0)
        .with(Resource::AddressSpace, 3 << 30)
        .with(Resource::Cpu, 600)
}

/// A script that reports the limits of the leader and of a child it
/// starts at once, without waiting for anything.
#[cfg(target_os = "linux")]
const REPORT: &str = "cat /proc/$$/limits; echo ---; cat /proc/self/limits";
/// `ulimit` of the leader (a builtin) and of a child shell started at once.
#[cfg(not(target_os = "linux"))]
const REPORT: &str = "ulimit -f; ulimit -c; ulimit -t; echo ---; \
                      /bin/sh -c 'ulimit -f; ulimit -c; ulimit -t'";

/// Checks the output of [`REPORT`] for [`limits`].
fn check_report(out: &str) {
    let (leader, child) = out.split_once("---\n").unwrap_or_else(|| panic!("{out}"));
    for part in [leader, child] {
        #[cfg(target_os = "linux")]
        {
            let limit = |name: &str| {
                let line = part
                    .lines()
                    .find(|l| l.starts_with(name))
                    .unwrap_or_else(|| panic!("{name} missing in {part}"));
                let cols: Vec<_> = line[name.len()..].split_whitespace().collect();
                assert_eq!(cols[0], cols[1], "soft and hard are equal: {line}");
                cols[0].to_owned()
            };
            assert_eq!(limit("Max file size"), (1u64 << 20).to_string(), "{out}");
            assert_eq!(limit("Max core file size"), "0", "{out}");
            assert_eq!(
                limit("Max address space"),
                (3u64 << 30).to_string(),
                "{out}"
            );
            assert_eq!(limit("Max cpu time"), "600", "{out}");
        }
        #[cfg(not(target_os = "linux"))]
        {
            // `ulimit -f` counts 512-byte blocks in POSIX mode and
            // 1024-byte blocks otherwise (bash as `/bin/sh` on macOS).
            let lines: Vec<_> = part.lines().collect();
            assert!(
                lines == ["1024", "0", "600"] || lines == ["2048", "0", "600"],
                "{out}"
            );
        }
    }
}

#[test]
fn limits_are_set_before_the_program_starts() {
    let dir = tempfile::tempdir().unwrap();
    let spec = gated_sh(dir.path(), REPORT).with_rlimits(limits());
    let done = run(
        &spec,
        Watch::<()>::new().with_timeout(Duration::from_secs(20)),
    )
    .unwrap();
    assert!(done.status.success(), "{:?}", done.stderr);
    assert_eq!(done.gate_fallback, None);
    // macOS cannot set `RLIMIT_AS`, so not all were applied there.
    assert_eq!(done.rlimits_applied, cfg!(target_os = "linux"));
    check_report(&String::from_utf8(done.stdout.bytes).unwrap());
}

/// The same, many times in a row: nothing depends on timing. (Run it under
/// load to see that; see the PR for the measurement.)
#[test]
fn limits_are_set_before_the_program_starts_every_time() {
    let dir = tempfile::tempdir().unwrap();
    let spec = gated_sh(dir.path(), REPORT).with_rlimits(limits());
    for _ in 0..50 {
        let done = run(
            &spec,
            Watch::<()>::new().with_timeout(Duration::from_secs(20)),
        )
        .unwrap();
        assert!(done.status.success());
        check_report(&String::from_utf8(done.stdout.bytes).unwrap());
    }
}

#[test]
fn a_child_started_at_once_cannot_write_past_the_file_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let spec = gated_sh(dir.path(), "head -c 100000 /dev/zero > big").with_rlimits(
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
    assert!(done.rlimits_applied);
    assert_eq!(fs::metadata(dir.path().join("big")).unwrap().len(), 1000);
    assert!(!dir.path().join("core").exists());
}

#[test]
fn arguments_and_environment_are_passed_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let spec = Spec::new("/usr/bin/env", Cwd::Path(dir.path()))
        .with_env(
            EnvAllowlist::new()
                .with("HOME", "/nonexistent-home")
                .with("PATH", "/usr/bin:/bin"),
        )
        .with_start(StartMode::ExecGate(gate()));
    let done = run(&spec, Watch::<()>::new()).unwrap();
    let mut vars: Vec<_> = String::from_utf8(done.stdout.bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    vars.sort();
    assert_eq!(vars, ["HOME=/nonexistent-home", "PATH=/usr/bin:/bin"]);

    // Arguments that look like options or shell syntax reach the program
    // as they are.
    let args = ["--", "--rlimit", "a b", "$(touch x)", "*", ""];
    let spec = Spec::new("/usr/bin/printf", Cwd::Path(dir.path()))
        .with_args(std::iter::once("[%s]").chain(args))
        .with_start(StartMode::ExecGate(gate()));
    let done = run(&spec, Watch::<()>::new()).unwrap();
    assert_eq!(
        String::from_utf8(done.stdout.bytes).unwrap(),
        "[--][--rlimit][a b][$(touch x)][*][]"
    );
    assert!(!dir.path().join("x").exists());
}

#[test]
fn stdin_of_the_program_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let script = "if read t; then echo \"got $t\"; else echo eof; fi";
    let done = run(&gated_sh(dir.path(), script), Watch::<()>::new()).unwrap();
    assert_eq!(done.stdout.bytes, b"eof\n");
}

#[test]
fn a_missing_program_is_a_spawn_error() {
    let dir = tempfile::tempdir().unwrap();
    let spec = Spec::new("/nonexistent/texrun-test-program", Cwd::Path(dir.path()))
        .with_start(StartMode::ExecGate(gate()));
    let err = run(&spec, Watch::<()>::new()).unwrap_err();
    match err {
        RunError::Spawn { program, source } => {
            assert_eq!(program, "/nonexistent/texrun-test-program");
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_relative_program_is_refused_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let spec = Spec::new("sh", Cwd::Path(dir.path())).with_start(StartMode::ExecGate(gate()));
    let err = run(&spec, Watch::<()>::new()).unwrap_err();
    assert!(matches!(err, RunError::InvalidSpec(_)), "{err:?}");
}

#[test]
fn a_missing_gate_falls_back_or_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let missing = ExecGate::new("/nonexistent/texrun-exec-gate");
    let spec = gated_sh(dir.path(), "touch ran")
        .with_start(StartMode::ExecGate(missing))
        .with_rlimits(Rlimits::new().with(Resource::Core, 0));

    // Best effort: started as with `Immediate`, and recorded.
    let done = run(&spec, Watch::<()>::new()).unwrap();
    assert!(done.status.success());
    assert!(done.gate_fallback.unwrap().contains("/nonexistent"));
    assert_eq!(done.rlimits_applied, texrun_process::PRLIMIT_SUPPORTED);
    fs::remove_file(dir.path().join("ran")).unwrap();

    // Required: refused before anything runs.
    let err = run(&spec.with_require_rlimits(true), Watch::<()>::new()).unwrap_err();
    assert!(matches!(err, RunError::Unsupported(_)), "{err:?}");
    assert!(!dir.path().join("ran").exists());
}

#[test]
fn a_gate_that_does_not_speak_the_protocol_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    // `/usr/bin/true` exits at once without starting the program.
    let spec = gated_sh(dir.path(), "touch ran")
        .with_start(StartMode::ExecGate(ExecGate::new("/usr/bin/true")));
    let err = run(&spec, Watch::<()>::new()).unwrap_err();
    assert!(matches!(err, RunError::ExecGate(_)), "{err:?}");
    assert!(!dir.path().join("ran").exists());
}

#[test]
fn required_limits_the_platform_cannot_set_are_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let spec = gated_sh(dir.path(), "touch ran")
        .with_rlimits(Rlimits::new().with(Resource::AddressSpace, 3 << 30))
        .with_require_rlimits(true);
    let result = run(&spec, Watch::<()>::new());
    if cfg!(target_os = "linux") {
        assert!(result.unwrap().rlimits_applied);
    } else {
        let err = result.unwrap_err();
        assert!(matches!(err, RunError::Unsupported(_)), "{err:?}");
        assert!(!dir.path().join("ran").exists());
    }
}

/// Runs the gate binary by hand with `args` and `stdin`.
fn gate_by_hand(args: &[&str], stdin: Stdio, dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_texrun-exec-gate"))
        .args(args)
        .current_dir(dir)
        .stdin(stdin)
        .output()
        .unwrap()
}

#[test]
fn without_the_token_the_gate_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let args = ["texrun-exec-gate/1", "--", "/usr/bin/touch", "ran"];
    let out = gate_by_hand(&args, Stdio::null(), dir.path());
    assert_eq!(
        out.status.code(),
        Some(i32::from(ExecGate::EXIT_NOT_RELEASED))
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("start signal missing"));
    assert!(!dir.path().join("ran").exists());

    // Other bytes than the token do not release it either.
    let mut child = Command::new(env!("CARGO_BIN_EXE_texrun-exec-gate"))
        .args(args)
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(b"texrun-exec-gate: no!\n");
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(i32::from(ExecGate::EXIT_NOT_RELEASED)));
    assert!(!dir.path().join("ran").exists());
}

#[test]
fn invalid_arguments_run_nothing() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        &[][..],
        &["--", "/usr/bin/touch", "ran"],
        &["texrun-exec-gate/1", "/usr/bin/touch", "ran"],
        &["texrun-exec-gate/1", "--", "touch", "ran"],
        &[
            "texrun-exec-gate/1",
            "--rlimit",
            "bogus=1",
            "--",
            "/usr/bin/touch",
            "ran",
        ],
    ] {
        let out = gate_by_hand(args, Stdio::null(), dir.path());
        assert_eq!(
            out.status.code(),
            Some(i32::from(ExecGate::EXIT_USAGE)),
            "{args:?}"
        );
        assert!(!dir.path().join("ran").exists(), "{args:?}");
    }
}

/// A launcher that records the hooks, to see that `on_spawn` runs while
/// the gate still waits.
#[derive(Default)]
struct Probe {
    marker: std::sync::Mutex<Option<std::path::PathBuf>>,
    seen: std::sync::Mutex<Option<bool>>,
}

impl Launcher for Probe {
    fn command(&self, spec: &Spec<'_>) -> Result<Command, RunError> {
        HostLauncher.command(spec)
    }
    fn on_spawn(&self, _pid: u32) -> std::io::Result<()> {
        // Give a program that was not held a chance to run first.
        std::thread::sleep(Duration::from_millis(200));
        let marker = self.marker.lock().unwrap().clone().unwrap();
        *self.seen.lock().unwrap() = Some(marker.exists());
        Ok(())
    }
}

#[test]
fn the_program_waits_for_on_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Probe::default();
    *probe.marker.lock().unwrap() = Some(dir.path().join("ran"));
    let done = run_with(
        &probe,
        &gated_sh(dir.path(), "touch ran"),
        Watch::<()>::new(),
    )
    .unwrap();
    assert!(done.status.success());
    assert_eq!(
        *probe.seen.lock().unwrap(),
        Some(false),
        "ran before on_spawn"
    );
    assert!(dir.path().join("ran").exists());
}

#[test]
fn a_required_gate_that_cannot_be_used_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    for gate in [
        ExecGate::new("/nonexistent/texrun-exec-gate"),
        ExecGate::unavailable("no executable path known"),
    ] {
        // No `require_rlimits`: the gate itself is required.
        let spec = gated_sh(dir.path(), "touch ran")
            .with_start(StartMode::ExecGate(gate.with_required(true)))
            .with_rlimits(Rlimits::new().with(Resource::Core, 0));
        let err = run(&spec, Watch::<()>::new()).unwrap_err();
        assert!(matches!(err, RunError::Unsupported(_)), "{err:?}");
        assert!(!dir.path().join("ran").exists());
    }
}

#[test]
fn an_unavailable_gate_falls_back_with_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let spec = gated_sh(dir.path(), "exit 0").with_start(StartMode::ExecGate(
        ExecGate::unavailable("no executable path known"),
    ));
    let done = run(&spec, Watch::<()>::new()).unwrap();
    assert!(done.status.success());
    assert_eq!(
        done.gate_fallback.as_deref(),
        Some("no executable path known")
    );
}

#[test]
fn a_gate_run_by_hand_does_not_write_to_its_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stdin");
    fs::write(&path, "").unwrap();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let out = gate_by_hand(&["bogus"], Stdio::from(file), dir.path());
    assert_eq!(out.status.code(), Some(i32::from(ExecGate::EXIT_USAGE)));
    assert_eq!(
        fs::read(&path).unwrap(),
        b"",
        "a report was written to stdin"
    );
}
