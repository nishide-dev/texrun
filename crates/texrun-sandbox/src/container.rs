//! One hardened container, as a [`Launcher`].

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use texrun_process::{Cwd, Launcher, Resource, RunError, Spec};

use crate::error::SandboxError;
use crate::runtime::{Runtime, check_image};

/// Timeout of `create`: creating a container from a local image normally
/// takes well under a second, but a busy daemon or a VM that is just
/// starting (macOS) can take longer.
const CREATE_TIMEOUT: Duration = Duration::from_secs(120);

/// Timeout of `rm --force`.
const REMOVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where `prlimit` is expected in the image (util-linux), for
/// `RLIMIT_AS`.
const PRLIMIT: &str = "/usr/bin/prlimit";

/// Where `timeout` is expected in the image (coreutils), for
/// [`ContainerSpec::deadline`].
const TIMEOUT: &str = "/usr/bin/timeout";

/// The uid / gid used when texrun runs as root: `nobody`.
const NOBODY: u32 = 65534;

/// A host directory made visible in the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Absolute host path (canonical, so that the runtime does not follow a
    /// symlink somewhere else).
    pub host: PathBuf,
    /// Absolute path in the container.
    pub guest: PathBuf,
    /// Whether the container may write to it.
    pub writable: bool,
}

impl Mount {
    /// A read-only mount.
    pub fn read_only(host: impl Into<PathBuf>, guest: impl Into<PathBuf>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
            writable: false,
        }
    }

    /// A writable mount.
    pub fn writable(host: impl Into<PathBuf>, guest: impl Into<PathBuf>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
            writable: true,
        }
    }

    /// The `--mount` value. Paths are part of a comma-separated list, so
    /// they must not contain `,` or `"` (or control characters).
    fn option(&self) -> Result<OsString, SandboxError> {
        let text = |what: &str, path: &Path| -> Result<String, SandboxError> {
            let text = path
                .to_str()
                .filter(|t| {
                    path.is_absolute() && !t.chars().any(|c| c == ',' || c == '"' || c.is_control())
                })
                .ok_or_else(|| {
                    SandboxError::Invalid(format!(
                        "cannot mount {what} {}: it must be an absolute UTF-8 path without `,`, \
                         `\"` or control characters",
                        path.display()
                    ))
                })?;
            Ok(text.to_owned())
        };
        let mut option = format!(
            "type=bind,source={},target={}",
            text("the host directory", &self.host)?,
            text("at", &self.guest)?
        );
        if !self.writable {
            option.push_str(",readonly");
        }
        Ok(option.into())
    }
}

/// Limits of the whole container (its cgroup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerLimits {
    /// `--memory` and `--memory-swap` (no swap), in bytes.
    pub memory_bytes: u64,
    /// `--pids-limit`: processes and threads.
    pub processes: u64,
    /// `--cpus`.
    pub cpus: u32,
}

impl ContainerLimits {
    /// The given limits.
    pub fn new(memory_bytes: u64, processes: u64, cpus: u32) -> Self {
        Self {
            memory_bytes,
            processes,
            cpus,
        }
    }
}

/// Which user the container runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ContainerUser {
    /// texrun's effective uid / gid, so that files written to writable
    /// mounts belong to the user who runs texrun; `65534:65534` if that
    /// is root (the writable mounts are then handed to that user first).
    #[default]
    Host,
    /// This uid / gid (must not be 0).
    Id(u32, u32),
}

/// How to create a container (everything except what is run in it, which
/// comes from the [`Spec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerSpec {
    /// The image (must exist locally; texrun never pulls).
    pub image: String,
    /// The only host directories the container sees. Order does not
    /// matter: the runtime mounts parents before children.
    pub mounts: Vec<Mount>,
    /// Limits of the whole container.
    pub limits: ContainerLimits,
    /// The user in the container.
    pub user: ContainerUser,
    /// `--runtime` (e.g. `runsc` for gVisor, if the daemon has it
    /// configured). `None`: the daemon's default. Not tested by texrun.
    pub oci_runtime: Option<String>,
    /// Upper bound on how long the program runs, enforced inside the
    /// container with `timeout --signal=KILL`: it ends the container even
    /// if texrun is killed and cannot remove it. Should be longer than
    /// texrun's own timeout, which normally stops the run first.
    pub deadline: Option<Duration>,
    /// Size of the `tmpfs` at `/tmp`, in bytes.
    pub tmp_bytes: u64,
}

impl ContainerSpec {
    /// Default [`ContainerSpec::tmp_bytes`]: 64 MiB.
    pub const DEFAULT_TMP_BYTES: u64 = 64 * 1024 * 1024;

    /// A container of `image` with `limits`, no mounts, the host user and
    /// no deadline.
    pub fn new(image: impl Into<String>, limits: ContainerLimits) -> Self {
        Self {
            image: image.into(),
            mounts: Vec::new(),
            limits,
            user: ContainerUser::Host,
            oci_runtime: None,
            deadline: None,
            tmp_bytes: Self::DEFAULT_TMP_BYTES,
        }
    }

    /// Adds a mount.
    #[must_use]
    pub fn with_mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount);
        self
    }

    /// Sets [`ContainerSpec::user`].
    #[must_use]
    pub fn with_user(mut self, user: ContainerUser) -> Self {
        self.user = user;
        self
    }

    /// Sets [`ContainerSpec::oci_runtime`].
    #[must_use]
    pub fn with_oci_runtime(mut self, runtime: Option<String>) -> Self {
        self.oci_runtime = runtime;
        self
    }

    /// Sets [`ContainerSpec::deadline`].
    #[must_use]
    pub fn with_deadline(mut self, deadline: Option<Duration>) -> Self {
        self.deadline = deadline;
        self
    }
}

/// What the runtime recorded about a container, read before it was
/// removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerOutcome {
    /// The kernel's OOM killer stopped a process of the container
    /// (`--memory`).
    pub oom_killed: bool,
    /// The exit code of the container's main process, if it ended.
    pub exit_code: Option<i32>,
}

/// State of the container across the launcher hooks.
#[derive(Debug, Default)]
struct State {
    /// ID of the created container.
    id: Option<String>,
    /// Whether it was removed.
    removed: bool,
    /// What was read before removing it.
    outcome: Option<ContainerOutcome>,
}

/// One container for one supervised run: pass it to
/// [`texrun_process::run_with`] together with the [`Spec`] of what to run
/// in it (an absolute program in the image, its arguments, the complete
/// environment, the working directory as a path in the container, and the
/// rlimits).
///
/// The container is created by [`Launcher::command`] and removed by the
/// supervisor's hooks, or at the latest when this value is dropped. A value
/// serves one run.
#[derive(Debug)]
pub struct Container<'r> {
    runtime: &'r Runtime,
    spec: ContainerSpec,
    name: String,
    state: Mutex<State>,
}

impl<'r> Container<'r> {
    /// A container of `spec`, not created yet.
    pub fn new(runtime: &'r Runtime, spec: ContainerSpec) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!(
            "texrun-{}-{}-{nanos:09}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            runtime,
            spec,
            name,
            state: Mutex::new(State::default()),
        }
    }

    /// The container's name (unique per texrun process).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the runtime recorded about the container before it was
    /// removed; `None` if it was never created or could not be inspected.
    pub fn outcome(&self) -> Option<ContainerOutcome> {
        self.lock().outcome
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `create` arguments for `spec`.
    pub(crate) fn create_args(
        &self,
        spec: &Spec<'_>,
        user: (u32, u32),
    ) -> Result<Vec<OsString>, SandboxError> {
        let c = &self.spec;
        check_image(&c.image)?;
        let Cwd::Path(workdir) = spec.cwd else {
            return Err(SandboxError::Invalid(
                "a container run needs its working directory as a path in the container".to_owned(),
            ));
        };
        for (what, path) in [
            ("program", spec.program.as_path()),
            ("working directory", workdir),
        ] {
            if !path.is_absolute() {
                return Err(SandboxError::Invalid(format!(
                    "the {what} must be an absolute path in the container: {}",
                    path.display()
                )));
            }
        }
        if user.0 == 0 {
            return Err(SandboxError::Invalid(
                "the container must not run as root".to_owned(),
            ));
        }

        let mut args: Vec<OsString> = Vec::new();
        let mut push = |parts: &[&str]| args.extend(parts.iter().map(OsString::from));
        let label = format!("{}=1", crate::LABEL);
        let pid_label = format!("{}.pid={}", crate::LABEL, std::process::id());
        let user = format!("{}:{}", user.0, user.1);
        let memory = c.limits.memory_bytes.to_string();
        let pids = c.limits.processes.to_string();
        let cpus = c.limits.cpus.to_string();
        let tmpfs = format!("/tmp:rw,noexec,nosuid,nodev,size={}", c.tmp_bytes);
        push(&[
            "create",
            "--pull",
            "never",
            "--name",
            &self.name,
            "--label",
            &label,
            "--label",
            &pid_label,
            "--init",
            "--network",
            "none",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--ipc",
            "none",
            "--user",
            &user,
            "--pids-limit",
            &pids,
            "--memory",
            &memory,
            "--memory-swap",
            &memory,
            "--cpus",
            &cpus,
            "--tmpfs",
            &tmpfs,
            "--hostname",
            "texrun",
            "--log-driver",
            "none",
        ]);
        if let Some(runtime) = &c.oci_runtime {
            if runtime.is_empty()
                || !runtime
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
            {
                return Err(SandboxError::Invalid(format!(
                    "invalid OCI runtime name {runtime:?}"
                )));
            }
            args.extend(["--runtime".into(), runtime.into()]);
        }

        // Resource limits: `--ulimit` where the runtime accepts it, and
        // `RLIMIT_AS` with `prlimit` in the container, before the program.
        let mut address_space = None;
        for (resource, _) in spec.rlimits.iter() {
            let (soft, hard) = spec
                .rlimits
                .get_soft_hard(resource)
                .expect("listed by iter");
            let name = match resource {
                Resource::FileSize => "fsize",
                Resource::Core => "core",
                Resource::Cpu => "cpu",
                Resource::Processes => "nproc",
                Resource::AddressSpace => {
                    address_space = Some((soft, hard));
                    continue;
                }
                other => {
                    return Err(SandboxError::Invalid(format!(
                        "{other:?} cannot be limited in a container"
                    )));
                }
            };
            args.extend(["--ulimit".into(), format!("{name}={soft}:{hard}").into()]);
        }

        for mount in &c.mounts {
            args.extend(["--mount".into(), mount.option()?]);
        }
        args.extend(["--workdir".into(), workdir.as_os_str().to_owned()]);
        for (name, value) in spec.env.vars() {
            let name_text = name.to_str().unwrap_or_default();
            if name_text.is_empty() || name_text.contains('=') {
                return Err(SandboxError::Invalid(format!(
                    "invalid environment variable name {name:?}"
                )));
            }
            let mut pair = name.to_owned();
            pair.push("=");
            pair.push(value);
            args.extend(["--env".into(), pair]);
        }

        // The command: [timeout --signal=KILL <s>s] [prlimit --as=S:H --]
        // program args...
        let mut command: Vec<OsString> = Vec::new();
        if let Some(deadline) = c.deadline {
            let secs = deadline.as_secs().max(1);
            command.extend([
                TIMEOUT.into(),
                "--signal=KILL".into(),
                format!("{secs}s").into(),
            ]);
        }
        if let Some((soft, hard)) = address_space {
            command.extend([
                PRLIMIT.into(),
                format!("--as={soft}:{hard}").into(),
                "--".into(),
            ]);
        }
        command.push(spec.program.as_os_str().to_owned());
        command.extend(spec.args.iter().cloned());
        let (entrypoint, rest) = command.split_first().expect("the program is there");
        args.extend(["--entrypoint".into(), entrypoint.clone(), "--".into()]);
        args.push(c.image.clone().into());
        args.extend(rest.iter().cloned());
        Ok(args)
    }

    /// Reads the container's state and removes it (killing what still
    /// runs). Idempotent; errors are ignored (the next hook, or the drop,
    /// tries again).
    fn remove(&self) {
        let mut state = self.lock();
        let Some(id) = state.id.clone() else { return };
        if state.removed {
            return;
        }
        if state.outcome.is_none() {
            state.outcome = self.inspect(&id);
        }
        let args: Vec<OsString> = ["rm", "--force", "--"]
            .into_iter()
            .map(OsString::from)
            .chain([OsString::from(&id)])
            .collect();
        if self.runtime.exec(&args, REMOVE_TIMEOUT).is_ok() {
            state.removed = true;
        }
    }

    fn inspect(&self, id: &str) -> Option<ContainerOutcome> {
        let out = self
            .runtime
            .query(&[
                "inspect",
                "--format",
                "{{.State.OOMKilled}} {{.State.Running}} {{.State.ExitCode}}",
                "--",
                id,
            ])
            .ok()?;
        let mut fields = out.split_whitespace();
        let oom_killed = fields.next()? == "true";
        let running = fields.next()? == "true";
        let exit_code = fields.next()?.parse().ok().filter(|_| !running);
        Some(ContainerOutcome {
            oom_killed,
            exit_code,
        })
    }

    /// The user of the container, and whether texrun runs as root (so that
    /// writable mounts must be handed to that user).
    fn user(&self) -> ((u32, u32), bool) {
        let euid = rustix::process::geteuid().as_raw();
        let egid = rustix::process::getegid().as_raw();
        let root = euid == 0;
        let user = match self.spec.user {
            ContainerUser::Id(uid, gid) => (uid, gid),
            ContainerUser::Host if root => (NOBODY, NOBODY),
            ContainerUser::Host => (euid, egid),
        };
        (user, root)
    }
}

impl Launcher for Container<'_> {
    fn command(&self, spec: &Spec<'_>) -> Result<Command, RunError> {
        let invalid = |e: SandboxError| RunError::InvalidSpec(e.to_string());
        let (user, root) = self.user();
        let args = self.create_args(spec, user).map_err(invalid)?;
        if root {
            for mount in self.spec.mounts.iter().filter(|m| m.writable) {
                chown_tree(&mount.host, user).map_err(|e| RunError::Io {
                    context: format!(
                        "handing {} to the container user {}:{}",
                        mount.host.display(),
                        user.0,
                        user.1
                    ),
                    source: e,
                })?;
            }
        }
        {
            let state = self.lock();
            if state.id.is_some() {
                return Err(RunError::InvalidSpec(
                    "a Container serves one run only".to_owned(),
                ));
            }
        }
        let id = self
            .runtime
            .exec(&args, CREATE_TIMEOUT)
            .map_err(|e| RunError::Spawn {
                program: format!("{} create", self.runtime.kind()),
                source: io::Error::other(e.to_string()),
            })?;
        let id = id.trim().to_owned();
        if id.is_empty() {
            return Err(RunError::Spawn {
                program: format!("{} create", self.runtime.kind()),
                source: io::Error::other("no container ID was reported"),
            });
        }
        self.lock().id = Some(id.clone());

        let mut cmd = Command::new(self.runtime.program());
        cmd.args(["start", "--attach", "--"])
            .arg(&id)
            .env_clear()
            .envs(self.runtime.env().vars())
            .current_dir("/");
        Ok(cmd)
    }

    /// The runtime applies the limits: `--ulimit` and the cgroup of the
    /// container. The supervisor must neither `prlimit` the runtime CLI nor
    /// put it into a cgroup of its own.
    fn apply_rlimits(&self) -> bool {
        false
    }

    fn on_kill(&self, _pid: u32) {
        self.remove();
    }

    fn on_reaped(&self, _pid: u32) {
        self.remove();
    }
}

impl Drop for Container<'_> {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Hands `dir` and everything below it to `user` (without following
/// symlinks).
fn chown_tree(dir: &Path, user: (u32, u32)) -> io::Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        std::os::unix::fs::lchown(&path, Some(user.0), Some(user.1))?;
        if std::fs::symlink_metadata(&path)?.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use texrun_process::{EnvAllowlist, Rlimits};

    fn runtime() -> Runtime {
        Runtime::for_tests()
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_str().unwrap().to_owned())
            .collect()
    }

    fn spec(image: &str) -> ContainerSpec {
        ContainerSpec::new(image, ContainerLimits::new(4096, 64, 2))
            .with_mount(Mount::read_only("/host/ws", "/workspace"))
            .with_mount(Mount::writable(
                "/host/ws/.texrun/out",
                "/workspace/.texrun/out",
            ))
            .with_deadline(Some(Duration::from_secs(90)))
    }

    #[test]
    fn create_args_harden_the_container() {
        let rt = runtime();
        let container = Container::new(&rt, spec("texrun-engine:latest"));
        let run = Spec::new("/usr/bin/latexmk", Cwd::Path(Path::new("/workspace/src")))
            .with_args(["-norc", "./-x.tex"])
            .with_env(EnvAllowlist::new().with("HOME", "/workspace/.texrun/home"))
            .with_rlimits(
                Rlimits::new()
                    .with(Resource::FileSize, 100)
                    .with(Resource::Core, 0)
                    .with_soft_hard(Resource::Cpu, 70, 75)
                    .with(Resource::AddressSpace, 4096),
            );
        let args = strings(&container.create_args(&run, (501, 20)).unwrap());
        let joined = args.join(" ");
        for expected in [
            "--pull never",
            "--init",
            "--network none",
            "--read-only",
            "--cap-drop ALL",
            "--security-opt no-new-privileges",
            "--ipc none",
            "--user 501:20",
            "--pids-limit 64",
            "--memory 4096",
            "--memory-swap 4096",
            "--cpus 2",
            "--tmpfs /tmp:rw,noexec,nosuid,nodev,size=67108864",
            "--log-driver none",
            "--ulimit fsize=100:100",
            "--ulimit core=0:0",
            "--ulimit cpu=70:75",
            "--mount type=bind,source=/host/ws,target=/workspace,readonly",
            "--mount type=bind,source=/host/ws/.texrun/out,target=/workspace/.texrun/out --",
            "--workdir /workspace/src",
            "--env HOME=/workspace/.texrun/home",
            "--entrypoint /usr/bin/timeout -- texrun-engine:latest --signal=KILL 90s \
             /usr/bin/prlimit --as=4096:4096 -- /usr/bin/latexmk -norc ./-x.tex",
        ] {
            assert!(
                joined.contains(expected),
                "missing `{expected}` in {joined}"
            );
        }
        assert_eq!(args[0], "create");
        // The image and the command come last, after `--`.
        assert!(joined.ends_with("/usr/bin/latexmk -norc ./-x.tex"));
        assert!(!joined.contains("--privileged"));
        assert!(!joined.contains("--network host"));
    }

    #[test]
    fn create_args_refuse_what_cannot_be_expressed() {
        let rt = runtime();
        let run = |program: &str, cwd: &'static str| {
            Spec::new(program.to_owned(), Cwd::Path(Path::new(cwd)))
        };
        let container = Container::new(&rt, spec("texrun-engine:latest"));
        assert!(
            container
                .create_args(&run("latexmk", "/w"), (1, 1))
                .is_err()
        );
        assert!(container.create_args(&run("/bin/x", "w"), (1, 1)).is_err());
        assert!(container.create_args(&run("/bin/x", "/w"), (0, 0)).is_err());
        let bad_env = run("/bin/x", "/w").with_env(EnvAllowlist::new().with("A=B", "c"));
        assert!(container.create_args(&bad_env, (1, 1)).is_err());
        for bad in [
            spec("-image"),
            spec("x").with_mount(Mount::read_only("/a,b", "/c")),
            spec("x").with_mount(Mount::read_only("/a", "/c\"d")),
            spec("x").with_mount(Mount::read_only("relative", "/c")),
            spec("x").with_oci_runtime(Some("runsc --x".to_owned())),
        ] {
            let container = Container::new(&rt, bad.clone());
            assert!(
                container.create_args(&run("/bin/x", "/w"), (1, 1)).is_err(),
                "{bad:?}"
            );
        }
        let gvisor = Container::new(&rt, spec("x").with_oci_runtime(Some("runsc".to_owned())));
        let args = strings(&gvisor.create_args(&run("/bin/x", "/w"), (1, 1)).unwrap());
        assert!(args.join(" ").contains("--runtime runsc"));
    }

    #[test]
    fn names_are_unique() {
        let rt = runtime();
        let a = Container::new(&rt, spec("x"));
        let b = Container::new(&rt, spec("x"));
        assert_ne!(a.name(), b.name());
        assert!(a.name().starts_with("texrun-"));
        assert_eq!(a.outcome(), None);
    }

    #[test]
    fn chown_tree_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/f"), "").unwrap();
        std::os::unix::fs::symlink("/", dir.path().join("sub/root")).unwrap();
        let me = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        // To ourselves: always allowed, and the walk must not descend into
        // the symlink to `/`.
        chown_tree(dir.path(), me).unwrap();
    }
}
