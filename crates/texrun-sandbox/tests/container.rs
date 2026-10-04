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
    Container, ContainerLimits, ContainerSpec, IMAGE_VERSION_LABEL, LABEL, Mount, Runtime,
    SandboxError, Session,
};

const REQUIRE_ENV: &str = "TEXRUN_REQUIRE_SANDBOX";
const IMAGE_ENV: &str = "TEXRUN_SANDBOX_IMAGE";

fn image() -> String {
    std::env::var(IMAGE_ENV)
        .ok()
        .filter(|i| !i.is_empty())
        .unwrap_or_else(|| "texrun-engine:latest".to_owned())
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
sed -n 's/^CapEff:\t*/capeff=/p; s/^CapBnd:\t*/capbnd=/p; s/^NoNewPrivs:\t*/nonewprivs=/p; s/^Seccomp:\t*/seccomp=/p' /proc/self/status
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
    // Nothing can be gained either.
    assert_eq!(get("capbnd"), "0000000000000000", "{text}");
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

/// The capability report (as the engine's probe runs it) passes in a real
/// container, and what is left is the program's own output.
#[test]
fn the_capability_report_passes_and_leaves_the_programs_output() {
    let runtime = require_sandbox!();
    let container = Container::new(runtime, ContainerSpec::new(image(), limits()));
    let (program, args) = texrun_sandbox::capability_report(
        Some(Path::new("/bin/echo")),
        &["from the program".into()],
    );
    let spec = Spec::new(program, Cwd::Path(Path::new("/")))
        .with_args(args)
        .with_env(EnvAllowlist::new().with("PATH", "/usr/bin:/bin"));
    let mut finished = run(
        &container,
        &spec,
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    assert!(finished.status.success(), "{finished:?}");
    let report = String::from_utf8_lossy(&finished.stdout.bytes).into_owned();
    assert_eq!(
        texrun_sandbox::take_capability_report(&mut finished.stdout),
        Ok(()),
        "{report}"
    );
    assert_eq!(stdout(&finished), "from the program\n");
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
    // A shell variable of 128 MiB.
    let script = "x=$(head -c 134217728 /dev/zero | tr '\\0' a); echo ${#x}";
    let container = Container::new(runtime, ContainerSpec::new(image(), limits));
    let finished = run(
        &container,
        &sh(script),
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    assert_eq!(finished.status.code(), Some(128 + 9), "{finished:?}");
    assert_eq!(
        container.outcome().unwrap().exit_code,
        Some(128 + 9),
        "{finished:?}"
    );
    // Without the report, only the runtime's record could tell, and it
    // misses kills: rootless Podman does not keep it, and Docker on cgroup
    // v2 loses a few percent of them for good (#60). So it is not asserted.

    // With the report of the compile containers, both runtimes tell every
    // time: the shell reads the cgroup's `oom_kill` counter, which the
    // kernel raises before it sends `SIGKILL`. Several kills, since a
    // missed one shows only now and then.
    for _ in 0..3 {
        let container = Container::new(
            runtime,
            ContainerSpec::new(image(), limits).with_report_pids(true),
        );
        let mut finished = run(
            &container,
            &sh(script),
            Watch::new().with_timeout(Duration::from_secs(60)),
        );
        assert_eq!(finished.status.code(), Some(128 + 9), "{finished:?}");
        assert_eq!(
            container.take_pids_report(&mut finished.stderr),
            Some(false),
            "{finished:?}"
        );
        assert!(container.outcome().unwrap().oom_killed, "{finished:?}");
    }
}

#[test]
fn the_oom_killer_takes_the_command_before_the_reporting_shell() {
    let runtime = require_sandbox!();
    let container = Container::new(
        runtime,
        ContainerSpec::new(image(), limits()).with_report_pids(true),
    );
    // The parent of the command is the reporting shell.
    let mut finished = run(
        &container,
        &sh("cat /proc/self/oom_score_adj /proc/$PPID/oom_score_adj"),
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    assert!(finished.status.success(), "{finished:?}");
    assert_eq!(
        container.take_pids_report(&mut finished.stderr),
        Some(false)
    );
    // The shell keeps the runtime's value (0 with Docker; a rootless
    // runtime may pass on a user session's own).
    let out = stdout(&finished);
    let scores: Vec<i32> = out.lines().map(|l| l.parse().unwrap()).collect();
    assert_eq!(scores.len(), 2, "{out}");
    assert_eq!(scores[0], 1000, "{out}");
    assert!(scores[1] < 1000, "{out}");
    assert!(finished.stderr.bytes.is_empty(), "{finished:?}");
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
fn the_image_is_inspected() {
    let runtime = require_sandbox!();
    let found = runtime.image(&image()).unwrap();
    // Docker reports `sha256:<hex>`, Podman the bare hex digits.
    let hex = found.id.strip_prefix("sha256:").unwrap_or(&found.id);
    assert!(
        hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "{found:?}"
    );
    assert_eq!(runtime.image_id(&image()).unwrap(), found.id);
}

/// Imports an empty file system as the image `tag`, with the Dockerfile
/// `changes` (`LABEL ...`), and removes it when dropped.
struct ImportedImage<'a> {
    runtime: &'a Runtime,
    tag: String,
}

impl<'a> ImportedImage<'a> {
    fn new(runtime: &'a Runtime, name: &str, changes: &[&str]) -> Self {
        let tag = format!("texrun-test-{name}-{}:0", std::process::id());
        let dir = tempfile::tempdir().unwrap();
        let tar = dir.path().join("empty.tar");
        // An empty tar archive: two zero blocks.
        std::fs::write(&tar, [0u8; 1024]).unwrap();
        let mut import = Command::new(runtime.program());
        import.arg("import");
        for change in changes {
            import.args(["--change", change]);
        }
        let out = import.arg(&tar).arg(&tag).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        Self { runtime, tag }
    }
}

impl Drop for ImportedImage<'_> {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime.program())
            .args(["image", "rm", "--", &self.tag])
            .output();
    }
}

/// #54: Docker 29 has no `Labels` in the config of an image without
/// labels (e.g. a local `docker build docker/engine`).
#[test]
fn an_image_without_labels_is_inspected() {
    let runtime = require_sandbox!();
    let image = ImportedImage::new(runtime, "unlabelled", &[]);
    let found = runtime.image(&image.tag).unwrap();
    assert!(!found.id.is_empty(), "{found:?}");
    assert_eq!(found.version, None, "{found:?}");

    let other = ImportedImage::new(runtime, "other-label", &["LABEL a=b"]);
    assert_eq!(runtime.image(&other.tag).unwrap().version, None);

    let labelled = ImportedImage::new(
        runtime,
        "labelled",
        &[&format!("LABEL {IMAGE_VERSION_LABEL}=9.8.7")],
    );
    let found = runtime.image(&labelled.tag).unwrap();
    assert_eq!(found.version.as_deref(), Some("9.8.7"), "{found:?}");
}

#[test]
fn a_missing_image_is_unavailable() {
    let runtime = require_sandbox!();
    let err = runtime.image_id("texrun-no-such-image:0").unwrap_err();
    assert!(matches!(err, SandboxError::Unavailable(_)), "{err:?}");
}

/// The `NetworkMode` the runtime records for the container `name`.
fn network_mode(runtime: &Runtime, name: &str) -> String {
    let out = Command::new(runtime.program())
        .args([
            "inspect",
            "--format",
            "{{.HostConfig.NetworkMode}}",
            "--",
            name,
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A perl script (perl is in the image for latexmk) that tries to reach
/// public addresses over IPv4 and IPv6 (TCP, and a UDP `connect`) and to
/// resolve a name, printing `<probe>=<error>` or `<probe>=connected`. As a
/// control, the same `connect` to a listener on the container's own
/// loopback must succeed (`lo4`): the probe can tell a connection from a
/// refused one.
const NETWORK_PROBE: &str = r#"
use strict; use warnings;
use Socket qw(:addrinfo AF_INET AF_INET6 SOCK_STREAM SOCK_DGRAM SOL_SOCKET SO_REUSEADDR INADDR_LOOPBACK inet_pton pack_sockaddr_in pack_sockaddr_in6);
socket(my $l, AF_INET, SOCK_STREAM, 0) or die "listen socket: $!";
bind($l, pack_sockaddr_in(0, INADDR_LOOPBACK)) or die "bind: $!";
listen($l, 1) or die "listen: $!";
my $lo = getsockname($l);
my @probes = (
  ["lo4", AF_INET, SOCK_STREAM, $lo],
  ["tcp4", AF_INET, SOCK_STREAM, pack_sockaddr_in(443, inet_pton(AF_INET, "1.1.1.1"))],
  ["udp4", AF_INET, SOCK_DGRAM, pack_sockaddr_in(53, inet_pton(AF_INET, "8.8.8.8"))],
  ["tcp6", AF_INET6, SOCK_STREAM, pack_sockaddr_in6(443, inet_pton(AF_INET6, "2606:4700:4700::1111"))],
  ["udp6", AF_INET6, SOCK_DGRAM, pack_sockaddr_in6(53, inet_pton(AF_INET6, "2001:4860:4860::8888"))],
);
for my $p (@probes) {
  my ($name, $family, $type, $addr) = @$p;
  my $s;
  if (!socket($s, $family, $type, 0)) { print "$name=socket: $!\n"; next; }
  if (connect($s, $addr)) { print "$name=connected\n"; } else { print "$name=$!\n"; }
}
my ($err) = getaddrinfo("example.com", "443", { socktype => SOCK_STREAM });
print "dns=", ($err ? "failed" : "resolved"), "\n";
"#;

fn perl(script: &str) -> Spec<'static> {
    Spec::new("/usr/bin/perl", Cwd::Path(Path::new("/")))
        .with_args(["-e", script])
        .with_env(EnvAllowlist::new().with("PATH", "/usr/bin:/bin"))
}

/// No route out of the container: every connection fails at once with
/// "Network is unreachable" (there is no interface but loopback, so this
/// is neither a firewall nor a timeout), and names do not resolve (#24).
fn assert_no_network(text: &str) {
    let f = fields(text);
    let get = |probe: &str| f.get(probe).map_or("", String::as_str);
    assert_eq!(get("lo4"), "connected", "the control: {text}");
    for probe in ["tcp4", "udp4"] {
        assert_eq!(get(probe), "Network is unreachable", "{probe}: {text}");
    }
    // A kernel without IPv6 cannot even create the socket.
    for probe in ["tcp6", "udp6"] {
        let result = get(probe);
        assert!(
            result == "Network is unreachable" || result.starts_with("socket: "),
            "{probe}: {text}"
        );
    }
    assert_eq!(f.get("dns").map(String::as_str), Some("failed"), "{text}");
}

#[test]
fn a_container_cannot_reach_the_network() {
    let runtime = require_sandbox!();
    let container = Container::new(runtime, ContainerSpec::new(image(), limits()));
    let finished = run(
        &container,
        &perl(NETWORK_PROBE),
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    let text = stdout(&finished);
    assert!(finished.status.success(), "{finished:?}\n{text}");
    assert_no_network(&text);
}

fn exec(
    session: &Session<'_>,
    spec: &Spec<'_>,
    watch: Watch<'_>,
) -> Result<Finished, texrun_process::RunError> {
    run_with(session, spec, watch)
}

fn session_ulimits() -> Rlimits {
    Rlimits::new()
        .with(Resource::FileSize, 1_000_000)
        .with(Resource::Core, 0)
        .with_soft_hard(Resource::Cpu, 30, 35)
}

fn start_session(runtime: &Runtime, spec: ContainerSpec, lifetime: Duration) -> Session<'_> {
    Session::start(runtime, spec, lifetime, session_ulimits()).unwrap()
}

#[test]
fn a_session_runs_programs_one_after_another_in_one_container() {
    let runtime = require_sandbox!();
    let dir = tempdir();
    let (input, work) = (dir.path().join("in"), dir.path().join("work"));
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&work).unwrap();
    std::fs::write(input.join("input.txt"), "input\n").unwrap();
    let spec = ContainerSpec::new(image(), limits())
        .with_mount(Mount::read_only(canonical(&input), "/texrun/in"))
        .with_mount(Mount::writable(canonical(&work), "/texrun/work"));
    let session = start_session(runtime, spec, Duration::from_secs(120));
    let name = session.name().to_owned();
    assert_eq!(network_mode(runtime, &name), "none");

    let script = r#"
echo "uid=$(id -u)"
sed -n 's/^CapEff:\t*/capeff=/p; s/^CapBnd:\t*/capbnd=/p; s/^NoNewPrivs:\t*/nonewprivs=/p' /proc/self/status
echo "net=$(ls /sys/class/net | tr '\n' ' ')"
echo "input=$(cat /texrun/in/input.txt)"
if touch /texrun/in/probe 2>/dev/null; then echo in=writable; else echo in=read-only; fi
if touch probe 2>/dev/null; then echo work=writable; else echo work=read-only; fi
if touch /usr/probe 2>/dev/null; then echo root_fs=writable; else echo root_fs=read-only; fi
echo "env=$(env | cut -d= -f1 | sort | tr '\n' ' ')"
echo "pwd=$(pwd)"
echo "pids_max=$(cat /sys/fs/cgroup/pids.max)"
grep -E '^Max (cpu time|file size|core file size|address space)' /proc/self/limits
"#;
    let spec = Spec::new("/bin/sh", Cwd::Path(Path::new("/texrun/work")))
        .with_args(["-c", script])
        .with_env(
            EnvAllowlist::new()
                .with("PATH", "/usr/bin:/bin")
                .with("HOME", "/texrun/work"),
        )
        .with_rlimits(
            Rlimits::new()
                .with(Resource::FileSize, 1000)
                .with(Resource::Core, 0)
                .with_soft_hard(Resource::Cpu, 20, 25)
                .with(Resource::AddressSpace, 1 << 30),
        );
    let watch = || Watch::new().with_timeout(Duration::from_secs(60));
    let finished = exec(&session, &spec, watch()).unwrap();
    let text = stdout(&finished);
    assert!(finished.status.success(), "{finished:?}\n{text}");
    let f = fields(&text);
    let get = |k: &str| f.get(k).map_or("", String::as_str);
    assert_eq!(get("uid"), rustix_uid(), "{text}");
    assert_eq!(get("capeff"), "0000000000000000", "{text}");
    assert_eq!(get("capbnd"), "0000000000000000", "{text}");
    assert_eq!(get("nonewprivs"), "1", "{text}");
    assert_eq!(get("net"), "lo", "{text}");
    assert_eq!(get("input"), "input", "{text}");
    assert_eq!(get("in"), "read-only", "{text}");
    assert_eq!(get("work"), "writable", "{text}");
    assert_eq!(get("root_fs"), "read-only", "{text}");
    // The runtime's `HOSTNAME` and the shell's `PWD`, besides the spec's
    // (`PATH` and `HOME`; Podman gets `--unsetenv-all`, so not the image's
    // `ENV`).
    assert_eq!(get("env"), "HOME HOSTNAME PATH PWD", "{text}");
    assert_eq!(get("pwd"), "/texrun/work", "{text}");
    assert_eq!(get("pids_max"), "32", "{text}");
    // Set by `prlimit` for this run, below the session's limits.
    let limit = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no {name}: {text}"))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert!(limit("Max cpu time").contains("20 25"), "{text}");
    assert!(limit("Max file size").contains("1000 1000"), "{text}");
    assert!(limit("Max core file size").contains("0 0"), "{text}");
    assert!(
        limit("Max address space").contains(&format!("{0} {0}", 1u64 << 30)),
        "{text}"
    );
    assert!(work.join("probe").exists());
    assert!(!input.join("probe").exists());

    // The next run is in the same container, which is still up.
    let finished = exec(&session, &perl(NETWORK_PROBE), watch()).unwrap();
    assert!(finished.status.success(), "{finished:?}");
    assert_no_network(&stdout(&finished));
    assert!(!session.is_stopped());
    assert_eq!(containers_named(runtime, &name).len(), 1);

    // A limit above the session's hard limit fails the run.
    let too_large = sh("true").with_rlimits(
        Rlimits::new()
            .with(Resource::FileSize, 2_000_000)
            .with(Resource::Core, 0),
    );
    let finished = exec(&session, &too_large, watch()).unwrap();
    assert!(!finished.status.success(), "{finished:?}");

    drop(session);
    assert!(containers_named(runtime, &name).is_empty());
}

#[test]
fn a_killed_run_stops_the_session() {
    let runtime = require_sandbox!();
    let session = start_session(
        runtime,
        ContainerSpec::new(image(), limits()),
        Duration::from_secs(120),
    );
    let name = session.name().to_owned();
    let finished = exec(
        &session,
        &sh("sleep 120"),
        Watch::new().with_timeout(Duration::from_secs(2)),
    )
    .unwrap();
    assert_eq!(finished.stop, Some(Stop::TimedOut));
    // The program in the container is gone with the container.
    assert!(session.is_stopped());
    assert!(containers_named(runtime, &name).is_empty());
    assert!(exec(&session, &sh("true"), Watch::new()).is_err());
}

#[test]
fn an_oom_kill_in_a_session_is_recorded() {
    let runtime = require_sandbox!();
    let limits = ContainerLimits::new(32 * 1024 * 1024, 32, 1);
    let session = start_session(
        runtime,
        ContainerSpec::new(image(), limits),
        Duration::from_secs(120),
    );
    assert_eq!(session.oom_killed(), Some(false));
    let finished = exec(
        &session,
        &sh("x=$(head -c 134217728 /dev/zero | tr '\\0' a); echo ${#x}"),
        Watch::new().with_timeout(Duration::from_secs(60)),
    )
    .unwrap();
    assert!(!finished.status.success(), "{finished:?}");
    // The cgroup counts the kill before the run ends: no waiting for the
    // runtime's record (#60).
    assert_eq!(session.oom_killed(), Some(true), "{finished:?}");
    // Only kills since the previous question.
    assert_eq!(session.oom_killed(), Some(false));
    let finished = exec(
        &session,
        &sh("kill -KILL $$"),
        Watch::new().with_timeout(Duration::from_secs(60)),
    )
    .unwrap();
    assert_eq!(finished.status.code(), Some(128 + 9), "{finished:?}");
    // A run killed otherwise is not taken for an OOM kill, although the
    // runtime may have recorded the earlier one for the whole session.
    assert_eq!(session.oom_killed(), Some(false));
}

#[test]
fn a_session_ends_by_itself_after_its_lifetime() {
    let runtime = require_sandbox!();
    let session = start_session(
        runtime,
        ContainerSpec::new(image(), limits()),
        Duration::from_secs(2),
    );
    std::thread::sleep(Duration::from_secs(4));
    let finished = exec(
        &session,
        &sh("echo still here"),
        Watch::new().with_timeout(Duration::from_secs(60)),
    )
    .unwrap();
    assert!(!finished.status.success(), "{finished:?}");
}

/// Starts as many processes as the limit allows (each sleeping), then
/// exits (`exit`) or keeps retrying refused `fork`s (`retry`), like perl
/// in latexmk.
fn fork_until_refused(retry: bool) -> Spec<'static> {
    let on_refused = if retry {
        "select(undef, undef, undef, 0.01); next"
    } else {
        "print STDERR \"refused\\n\"; exit 7"
    };
    perl(&format!(
        "$| = 1; while (1) {{ my $p = fork(); if (!defined $p) {{ {on_refused} }} \
         if ($p == 0) {{ sleep 100; exit 0 }} }}"
    ))
}

#[test]
fn a_reached_process_limit_is_reported_when_the_command_ends() {
    let runtime = require_sandbox!();
    let limits = ContainerLimits::new(512 * 1024 * 1024, 8, 1);
    let spec = ContainerSpec::new(image(), limits).with_report_pids(true);

    let container = Container::new(runtime, spec.clone());
    let mut finished = run(
        &container,
        &fork_until_refused(false),
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    assert_eq!(finished.status.code(), Some(7), "{finished:?}");
    assert_eq!(container.take_pids_report(&mut finished.stderr), Some(true));
    // Only the command's own output is left.
    assert_eq!(String::from_utf8_lossy(&finished.stderr.bytes), "refused\n");
    assert!(!finished.stderr.is_truncated());

    let container = Container::new(runtime, spec);
    let mut finished = run(
        &container,
        &sh("echo out; echo err >&2; exit 3"),
        Watch::new().with_timeout(Duration::from_secs(60)),
    );
    assert_eq!(finished.status.code(), Some(3), "{finished:?}");
    assert_eq!(
        container.take_pids_report(&mut finished.stderr),
        Some(false)
    );
    assert_eq!(String::from_utf8_lossy(&finished.stderr.bytes), "err\n");
    assert!(!finished.stderr.is_truncated());
    assert_eq!(stdout(&finished), "out\n");
    assert_eq!(container.outcome().unwrap().pids_limit_reached, None);
}

#[test]
fn a_reached_process_limit_is_reported_when_texrun_stops_the_container() {
    let runtime = require_sandbox!();
    let limits = ContainerLimits::new(512 * 1024 * 1024, 8, 1);
    for (retry, reached) in [(true, true), (false, false)] {
        let spec = ContainerSpec::new(image(), limits).with_report_pids(true);
        let container = Container::new(runtime, spec);
        let program = if retry {
            fork_until_refused(true)
        } else {
            sh("sleep 120")
        };
        let finished = run(
            &container,
            &program,
            Watch::new().with_timeout(Duration::from_secs(3)),
        );
        assert_eq!(finished.stop, Some(Stop::TimedOut), "{finished:?}");
        let outcome = container.outcome().unwrap();
        assert_eq!(outcome.pids_limit_reached, Some(reached), "{finished:?}");
        assert!(containers_named(runtime, container.name()).is_empty());
    }
}

/// Creates a container named `name` with `labels` outside texrun, and
/// leaves it `created`, `running` or `exited`.
fn create_labelled(runtime: &Runtime, name: &str, labels: &[String], state: &str) {
    let docker = |args: &[&str]| {
        let out = Command::new(runtime.program()).args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
    };
    let seconds = if state == "exited" { "0" } else { "120" };
    let image = image();
    let mut args = vec![
        "create",
        "--pull",
        "never",
        "--network",
        "none",
        "--name",
        name,
    ];
    for label in labels {
        args.extend(["--label", label]);
    }
    args.extend(["--entrypoint", "/usr/bin/sleep", "--", &image, seconds]);
    docker(&args);
    if state != "created" {
        docker(&["start", "--", name]);
    }
    if state == "exited" {
        docker(&["wait", "--", name]);
    }
}

/// Removes the named containers with `rm --force` when dropped, so that a
/// failed assertion does not leave containers with forged labels behind.
struct RemoveOnDrop<'a> {
    runtime: &'a Runtime,
    names: Vec<String>,
}

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = Command::new(self.runtime.program())
                .args(["rm", "--force", "--", name])
                .output();
        }
    }
}

/// Containers that killed texrun processes left behind are removed by a
/// later one only if they are stopped, and only if they are this user's,
/// on this host, of a texrun process that is gone.
#[test]
fn only_stopped_containers_of_gone_texrun_processes_are_reclaimed() {
    use texrun_sandbox::{Creator, LABEL_HOST, LABEL_PID, LABEL_UID};

    let runtime = require_sandbox!();
    let me = Creator::current().unwrap();
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let labels = |pid: u32, uid: u32, host: u64| {
        vec![
            format!("{LABEL}=1"),
            format!("{LABEL_PID}={pid}"),
            format!("{LABEL_UID}={uid}"),
            format!("{LABEL_HOST}={host:016x}"),
        ]
    };
    let ours = std::process::id();
    let name = |pid: u32, n: u32| format!("texrun-{pid}-{ours}{n}-0");

    let reclaimed = [
        (name(dead, 1), labels(dead, uid, me.host()), "exited"),
        (name(dead, 2), labels(dead, uid, me.host()), "created"),
    ];
    let kept = [
        // Running.
        (name(dead, 3), labels(dead, uid, me.host()), "running"),
        // Its texrun is alive.
        (name(ours, 4), labels(ours, uid, me.host()), "exited"),
        // Another user's, another host's.
        (name(dead, 5), labels(dead, uid + 1, me.host()), "exited"),
        (name(dead, 6), labels(dead, uid, me.host() ^ 1), "exited"),
        // Not named like texrun's container of that PID.
        (
            format!("other-{dead}-{ours}7"),
            labels(dead, uid, me.host()),
            "exited",
        ),
        // Only the texrun label.
        (name(dead, 8), vec![format!("{LABEL}=1")], "exited"),
    ];
    let _cleanup = RemoveOnDrop {
        runtime,
        names: reclaimed
            .iter()
            .chain(&kept)
            .map(|(name, _, _)| name.clone())
            .collect(),
    };
    for (name, labels, state) in reclaimed.iter().chain(&kept) {
        create_labelled(runtime, name, labels, state);
    }

    // Not counted: another test may reclaim some of them first.
    runtime.reclaim_left_containers().unwrap();
    for (name, _, _) in &reclaimed {
        assert!(
            containers_named(runtime, name).is_empty(),
            "{name} was kept"
        );
    }
    for (name, _, _) in &kept {
        assert_eq!(
            containers_named(runtime, name).len(),
            1,
            "{name} was removed"
        );
    }
}

/// #56: containers labelled with this machine but another host. Another
/// boot of this machine counts only for a container created before this
/// boot, which none created now is; on macOS the same machine and boot
/// with another host name is judged by the PID. Labels of another machine,
/// or malformed ones, keep it.
#[test]
fn containers_of_this_machine_are_judged_by_boot_and_creation() {
    use texrun_sandbox::{Creator, LABEL_BOOT, LABEL_HOST, LABEL_MACHINE, LABEL_PID, LABEL_UID};

    let runtime = require_sandbox!();
    let me = Creator::current().unwrap();
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let labels = |machine: Option<String>, boot: Option<String>| {
        let mut labels = vec![
            format!("{LABEL}=1"),
            format!("{LABEL_PID}={dead}"),
            format!("{LABEL_UID}={uid}"),
            format!("{LABEL_HOST}={:016x}", me.host() ^ 1),
        ];
        labels.extend(machine.map(|m| format!("{LABEL_MACHINE}={m}")));
        labels.extend(boot.map(|b| format!("{LABEL_BOOT}={b}")));
        labels
    };
    let hex = |id: u64| format!("{id:016x}");
    let machine = me.machine().unwrap_or(0);
    let boot = me.boot().unwrap_or(0);
    let ours = std::process::id();
    let name = |n: u32| format!("texrun-{dead}-{ours}{n}-0");

    // This machine and boot, another host: a renamed host on macOS.
    let renamed = (
        name(1),
        labels(Some(hex(machine)), me.boot().map(hex)),
        "exited",
    );
    let renamed_is_reclaimed = cfg!(target_os = "macos") && me.machine().is_some();
    let kept = [
        // This machine, another boot, but created now.
        (
            name(2),
            labels(Some(hex(machine)), Some(hex(boot ^ 1))),
            "exited",
        ),
        // Another machine.
        (
            name(3),
            labels(Some(hex(machine ^ 1)), me.boot().map(hex)),
            "exited",
        ),
        (
            name(4),
            labels(Some(hex(machine ^ 1)), Some(hex(boot ^ 1))),
            "created",
        ),
        // A malformed machine or boot.
        (name(5), labels(Some("me".to_owned()), None), "exited"),
        (
            name(6),
            labels(Some(hex(machine)), Some("0".to_owned())),
            "exited",
        ),
        // No machine (#49) with another host.
        (name(7), labels(None, None), "exited"),
    ];
    let _cleanup = RemoveOnDrop {
        runtime,
        names: std::iter::once(&renamed)
            .chain(&kept)
            .map(|(name, _, _)| name.clone())
            .collect(),
    };
    for (name, labels, state) in std::iter::once(&renamed).chain(&kept) {
        create_labelled(runtime, name, labels, state);
    }

    runtime.reclaim_left_containers().unwrap();
    assert_eq!(
        containers_named(runtime, &renamed.0).is_empty(),
        renamed_is_reclaimed,
        "{}",
        renamed.0
    );
    for (name, _, _) in &kept {
        assert_eq!(
            containers_named(runtime, name).len(),
            1,
            "{name} was removed"
        );
    }
}
