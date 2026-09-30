//! The container restrictions, checked from inside a container of the
//! engine image with small `sh` scripts (no TeX involved).
//!
//! These tests need a container runtime and the engine image
//! (`docker build -t texrun-engine:latest docker/engine`, or the image in
//! `TEXRUN_SANDBOX_IMAGE`). Without them they are skipped, unless
//! `TEXRUN_REQUIRE_SANDBOX=1` (CI's `sandbox` job) turns that into a
//! failure.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use texrun_core::CancelToken;
use texrun_process::{
    Cwd, EnvAllowlist, Finished, Launcher, Resource, Rlimits, Spec, Stop, Watch, run_with,
};
use texrun_sandbox::{
    Container, ContainerLimits, ContainerSpec, DEFAULT_IMAGE, LABEL, Mount, Runtime, SandboxError,
};

const REQUIRE_ENV: &str = "TEXRUN_REQUIRE_SANDBOX";
const IMAGE_ENV: &str = "TEXRUN_SANDBOX_IMAGE";

fn image() -> String {
    std::env::var(IMAGE_ENV)
        .ok()
        .filter(|i| !i.is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE.to_owned())
}

/// The runtime, if it and the image can be used.
fn runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        let runtime = Runtime::detect(None).map_err(|e| e.to_string())?;
        runtime.image_id(&image()).map_err(|e| e.to_string())?;
        Ok(runtime)
    });
    match runtime {
        Ok(runtime) => Some(runtime),
        Err(e) if std::env::var_os(REQUIRE_ENV).is_some_and(|v| v == "1") => {
            panic!("the container sandbox is required ({REQUIRE_ENV}=1) but not usable: {e}")
        }
        Err(e) => {
            static REPORTED: OnceLock<()> = OnceLock::new();
            REPORTED.get_or_init(|| {
                let _ = writeln!(
                    std::io::stderr(),
                    "texrun-sandbox container: SKIPPED ({e}; set {REQUIRE_ENV}=1 to fail instead)"
                );
            });
            None
        }
    }
}

macro_rules! require_sandbox {
    () => {
        match runtime() {
            Some(runtime) => runtime,
            None => return,
        }
    };
}

/// A temporary directory the runtime can mount (below the system temporary
/// directory, which Docker Desktop and `OrbStack` share with their VM).
fn tempdir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("texrun-sandbox-it-")
        .tempdir()
        .unwrap();
    // The container user is texrun's user; the directory must also be
    // readable when that is mapped differently by the runtime.
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    dir
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap()
}

fn limits() -> ContainerLimits {
    ContainerLimits::new(512 * 1024 * 1024, 32, 1)
}

fn sh(script: &str) -> Spec<'static> {
    Spec::new("/bin/sh", Cwd::Path(Path::new("/")))
        .with_args(["-c", script])
        .with_env(EnvAllowlist::new().with("PATH", "/usr/bin:/bin"))
}

fn run(container: &Container<'_>, spec: &Spec<'_>, watch: Watch<'_>) -> Finished {
    run_with(container, spec, watch).unwrap()
}

fn stdout(finished: &Finished) -> String {
    String::from_utf8_lossy(&finished.stdout.bytes).into_owned()
}

/// `key=value` lines.
fn fields(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.trim().to_owned()))
        .collect()
}

/// Names of the containers with `name` (any state).
fn containers_named(runtime: &Runtime, name: &str) -> Vec<String> {
    let out = Command::new(runtime.program())
        .args(["ps", "--all", "--quiet", "--filter"])
        .arg(format!("name=^{name}$"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_container_is_isolated_and_unprivileged() {
    let runtime = require_sandbox!();
    let ws = tempdir();
    let out = ws.path().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(ws.path().join("input.txt"), "input\n").unwrap();
    // A host directory that is not mounted.
    let hidden = tempdir();
    std::fs::write(hidden.path().join("host-only.txt"), "host\n").unwrap();
    let hidden_file = canonical(&hidden.path().join("host-only.txt"));

    let spec = ContainerSpec::new(image(), limits())
        .with_mount(Mount::read_only(canonical(ws.path()), "/workspace"))
        .with_mount(Mount::writable(canonical(&out), "/workspace/out"));
    let container = Container::new(runtime, spec);
    let script = format!(
        r#"
echo "uid=$(id -u)"
echo "gid=$(id -g)"
sed -n 's/^CapEff:\t*/capeff=/p; s/^NoNewPrivs:\t*/nonewprivs=/p; s/^Seccomp:\t*/seccomp=/p' /proc/self/status
echo "net=$(ls /sys/class/net | tr '\n' ' ')"
if touch /usr/texrun-probe 2>/dev/null; then echo root_fs=writable; else echo root_fs=read-only; fi
if touch /tmp/probe 2>/dev/null; then echo tmp=writable; else echo tmp=read-only; fi
if touch /workspace/probe 2>/dev/null; then echo workspace=writable; else echo workspace=read-only; fi
if touch /workspace/out/probe 2>/dev/null; then echo out=writable; else echo out=read-only; fi
echo "input=$(cat /workspace/input.txt)"
if [ -e '{hidden}' ] || [ -e '{hidden_dir}' ]; then echo host_path=visible; else echo host_path=absent; fi
echo "env=$(env | cut -d= -f1 | sort | tr '\n' ' ')"
echo "pids_max=$(cat /sys/fs/cgroup/pids.max)"
echo "memory_max=$(cat /sys/fs/cgroup/memory.max)"
echo "swap_max=$(cat /sys/fs/cgroup/memory.swap.max 2>/dev/null || echo none)"
echo "cpu_max=$(cat /sys/fs/cgroup/cpu.max)"
"#,
        hidden = hidden_file.display(),
        hidden_dir = canonical(hidden.path()).display(),
    );
    let finished = run(
        &container,
        &sh(&script),
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    let text = stdout(&finished);
    assert!(finished.status.success(), "{finished:?}\n{text}");
    let f = fields(&text);
    let get = |k: &str| f.get(k).map_or("", String::as_str);

    assert_ne!(get("uid"), "0", "{text}");
    assert_eq!(get("uid"), rustix_uid(), "{text}");
    assert_eq!(get("capeff"), "0000000000000000", "{text}");
    assert_eq!(get("nonewprivs"), "1", "{text}");
    // The runtime's default seccomp profile is in force (filter mode).
    assert_eq!(get("seccomp"), "2", "{text}");
    assert_eq!(get("net"), "lo", "no network but loopback: {text}");
    assert_eq!(get("root_fs"), "read-only", "{text}");
    assert_eq!(get("tmp"), "writable", "{text}");
    assert_eq!(get("workspace"), "read-only", "{text}");
    assert_eq!(get("out"), "writable", "{text}");
    assert_eq!(get("input"), "input", "{text}");
    assert_eq!(get("host_path"), "absent", "{text}");
    assert_eq!(get("env"), "HOME HOSTNAME PATH PWD", "{text}");
    assert_eq!(get("pids_max"), "32", "{text}");
    assert_eq!(get("memory_max"), (512 * 1024 * 1024).to_string(), "{text}");
    assert!(matches!(get("swap_max"), "0" | "none"), "{text}");
    assert_eq!(get("cpu_max"), "100000 100000", "{text}");
    assert!(out.join("probe").exists());
    assert!(!ws.path().join("probe").exists());
    // Removed after the run.
    assert!(containers_named(runtime, container.name()).is_empty());
}

/// texrun's uid, as the container reports it.
fn rustix_uid() -> &'static str {
    static UID: OnceLock<String> = OnceLock::new();
    UID.get_or_init(|| {
        let out = Command::new("id").arg("-u").output().unwrap();
        let uid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if uid == "0" {
            texrun_sandbox::ROOT_FALLBACK_ID.to_string()
        } else {
            uid
        }
    })
}

#[test]
fn rlimits_are_set_before_the_program_starts() {
    let runtime = require_sandbox!();
    let container = Container::new(runtime, ContainerSpec::new(image(), limits()));
    let spec = sh("cat /proc/self/limits").with_rlimits(
        Rlimits::new()
            .with(Resource::FileSize, 1_000_000)
            .with(Resource::Core, 0)
            .with_soft_hard(Resource::Cpu, 30, 35)
            .with(Resource::AddressSpace, 1 << 30),
    );
    let finished = run(
        &container,
        &spec,
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    let text = stdout(&finished);
    assert!(finished.status.success(), "{text}");
    let line = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no {name}: {text}"))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert!(line("Max cpu time").contains("30 35"), "{text}");
    assert!(line("Max file size").contains("1000000 1000000"), "{text}");
    assert!(line("Max core file size").contains("0 0"), "{text}");
    assert!(
        line("Max address space").contains(&format!("{0} {0}", 1u64 << 30)),
        "{text}"
    );
    // The supervisor leaves the limits to the runtime.
    assert!(!container.apply_rlimits());
}

#[test]
fn exit_status_and_output_are_the_programs() {
    let runtime = require_sandbox!();
    let container = Container::new(runtime, ContainerSpec::new(image(), limits()));
    let finished = run(
        &container,
        &sh("echo out; echo err >&2; exit 7"),
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    assert_eq!(finished.status.code(), Some(7));
    assert_eq!(stdout(&finished), "out\n");
    assert_eq!(String::from_utf8_lossy(&finished.stderr.bytes), "err\n");
    assert_eq!(container.outcome().unwrap().exit_code, Some(7));
    assert!(containers_named(runtime, container.name()).is_empty());
}

#[test]
fn a_timeout_kills_and_removes_the_container() {
    let runtime = require_sandbox!();
    let container = Container::new(runtime, ContainerSpec::new(image(), limits()));
    let started = Instant::now();
    let finished = run(
        &container,
        &sh("sleep 120"),
        Watch::new().with_timeout(Duration::from_secs(2)),
    );
    assert_eq!(finished.stop, Some(Stop::TimedOut));
    assert!(started.elapsed() < Duration::from_secs(60));
    assert!(containers_named(runtime, container.name()).is_empty());
}

#[test]
fn cancellation_kills_and_removes_the_container() {
    let runtime = require_sandbox!();
    let container = Container::new(runtime, ContainerSpec::new(image(), limits()));
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        trigger.cancel();
    });
    let finished = run(
        &container,
        &sh("sleep 120"),
        Watch::new()
            .with_timeout(Duration::from_secs(90))
            .with_cancel(&cancel),
    );
    canceller.join().unwrap();
    assert_eq!(finished.stop, Some(Stop::Cancelled));
    assert!(containers_named(runtime, container.name()).is_empty());
}

#[test]
fn the_deadline_ends_the_container_by_itself() {
    let runtime = require_sandbox!();
    let spec = ContainerSpec::new(image(), limits()).with_deadline(Some(Duration::from_secs(2)));
    let container = Container::new(runtime, spec);
    let finished = run(
        &container,
        &sh("sleep 120"),
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    // Killed inside the container, not by the supervisor.
    assert_eq!(finished.stop, None);
    assert_eq!(finished.status.code(), Some(128 + 9), "{finished:?}");
}

#[test]
fn an_oom_kill_is_recorded() {
    let runtime = require_sandbox!();
    let limits = ContainerLimits::new(32 * 1024 * 1024, 32, 1);
    let container = Container::new(runtime, ContainerSpec::new(image(), limits));
    // A shell variable of 128 MiB.
    let finished = run(
        &container,
        &sh("x=$(head -c 134217728 /dev/zero | tr '\\0' a); echo ${#x}"),
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    assert!(!finished.status.success(), "{finished:?}");
    assert!(container.outcome().unwrap().oom_killed, "{finished:?}");
}

#[test]
fn a_created_container_is_removed_when_dropped() {
    let runtime = require_sandbox!();
    let container = Container::new(runtime, ContainerSpec::new(image(), limits()));
    let name = container.name().to_owned();
    // Created, never started.
    let _command = container.command(&sh("true")).unwrap();
    assert_eq!(containers_named(runtime, &name).len(), 1);
    drop(container);
    assert!(containers_named(runtime, &name).is_empty());
}

#[test]
fn containers_carry_the_texrun_label() {
    let runtime = require_sandbox!();
    let container = Container::new(runtime, ContainerSpec::new(image(), limits()));
    let _command = container.command(&sh("true")).unwrap();
    let out = Command::new(runtime.program())
        .args(["inspect", "--format"])
        .arg(format!("{{{{index .Config.Labels \"{LABEL}\"}}}}"))
        .arg(container.name())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1", "{out:?}");
}

#[test]
fn a_missing_image_is_unavailable() {
    let runtime = require_sandbox!();
    let err = runtime.image_id("texrun-no-such-image:0").unwrap_err();
    assert!(matches!(err, SandboxError::Unavailable(_)), "{err:?}");
}
