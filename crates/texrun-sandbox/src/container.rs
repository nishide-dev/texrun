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

/// The uid / gid used when texrun runs as root: the image's `texrun` user,
/// not an id that other host processes (e.g. `nobody`) share.
pub const ROOT_FALLBACK_ID: u32 = 10001;

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
    /// mounts belong to the user who runs texrun; [`ROOT_FALLBACK_ID`] if
    /// that is root (the writable mounts are then handed to that user
    /// first).
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
    /// What the runtime printed on stderr while creating it (warnings).
    warnings: Vec<String>,
    /// Why the created container was not started: the runtime did not
    /// apply a restriction that was asked for.
    refusal: Option<String>,
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

    /// What the runtime printed on stderr while creating the container
    /// (e.g. that the kernel does not support a limit), one line each.
    pub fn warnings(&self) -> Vec<String> {
        self.lock().warnings.clone()
    }

    /// Why the container was created but not started: the runtime's own
    /// record of it (`inspect`) lacks a restriction that was asked for,
    /// e.g. a limit that the runtime dropped. [`Launcher::command`] then
    /// fails with [`RunError::Unsupported`] and the container is removed.
    pub fn refusal(&self) -> Option<String> {
        self.lock().refusal.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `create` arguments for `spec`.
    #[allow(
        clippy::too_many_lines,
        reason = "one flat list of options, easier to review in one place"
    )]
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
        if self.runtime.is_rootless_podman() {
            args.extend(["--userns".into(), "keep-id".into()]);
        }
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
            if resource == Resource::AddressSpace {
                address_space = Some((soft, hard));
                continue;
            }
            let Some(name) = ulimit_name(resource) else {
                return Err(SandboxError::Invalid(format!(
                    "{resource:?} cannot be limited in a container"
                )));
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
                    "invalid environment variable name `{}`",
                    name.display()
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

    /// What `inspect` must show for a container created for `spec`.
    fn expected(&self, spec: &Spec<'_>) -> Expected {
        let limits = &self.spec.limits;
        let ulimits = spec
            .rlimits
            .iter()
            .filter_map(|(resource, _)| {
                let name = ulimit_name(resource)?;
                let (soft, hard) = spec.rlimits.get_soft_hard(resource)?;
                Some((name.to_owned(), soft, hard))
            })
            .collect();
        Expected {
            memory: limits.memory_bytes,
            processes: limits.processes,
            cpus: limits.cpus,
            ulimits,
        }
    }

    /// Removes a container of this name that `create` may have left
    /// (best effort; "no such container" is the normal case).
    fn remove_by_name(&self) {
        let args: Vec<OsString> = ["rm", "--force", "--", &self.name]
            .into_iter()
            .map(OsString::from)
            .collect();
        let _ = self.runtime.exec(&args, REMOVE_TIMEOUT);
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
        let host = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        let root = host.0 == 0;
        let user = match self.spec.user {
            ContainerUser::Id(uid, gid) => (uid, gid),
            ContainerUser::Host if root => (ROOT_FALLBACK_ID, ROOT_FALLBACK_ID),
            ContainerUser::Host => host,
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
        let create_failed = |message: String| {
            // The daemon may have created it anyway (e.g. after a timeout
            // of the CLI): remove it by its name, best effort.
            self.remove_by_name();
            RunError::Spawn {
                program: format!("{} create", self.runtime.kind()),
                source: io::Error::other(message),
            }
        };
        let (id, stderr) = self
            .runtime
            .exec_output(&args, CREATE_TIMEOUT)
            .map_err(|e| create_failed(e.to_string()))?;
        let id = id.trim().to_owned();
        if id.is_empty() {
            return Err(create_failed("no container ID was reported".to_owned()));
        }
        {
            let mut state = self.lock();
            state.id = Some(id.clone());
            state.warnings = stderr
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect();
        }

        // Fail closed: start it only if the runtime recorded every
        // restriction (a daemon drops unsupported limits with a warning
        // only).
        let expected = self.expected(spec);
        let checked = self
            .runtime
            .query(&["inspect", "--format", "{{json .HostConfig}}", "--", &id])
            .map_err(|e| e.to_string())
            .and_then(|json| check_host_config(&json, self.runtime.kind(), &expected));
        if let Err(reason) = checked {
            let reason = format!(
                "{} did not apply the container restrictions: {reason}",
                self.runtime.kind()
            );
            self.lock().refusal = Some(reason.clone());
            self.remove();
            return Err(RunError::Unsupported(reason));
        }

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

/// The `--ulimit` name of `resource`, for those the runtime sets.
fn ulimit_name(resource: Resource) -> Option<&'static str> {
    match resource {
        Resource::FileSize => Some("fsize"),
        Resource::Core => Some("core"),
        Resource::Cpu => Some("cpu"),
        Resource::Processes => Some("nproc"),
        _ => None,
    }
}

/// The restrictions a created container must show in its `HostConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expected {
    memory: u64,
    processes: u64,
    cpus: u32,
    /// `(name, soft, hard)` as in `--ulimit`.
    ulimits: Vec<(String, u64, u64)>,
}

/// Checks the `HostConfig` of a created container (`inspect`, JSON)
/// against what texrun asked for; the error lists every difference.
///
/// Field names are Docker's, which Podman's `inspect` also uses. Podman
/// lists the dropped capabilities one by one instead of `ALL`, and may
/// record the CPU limit as quota / period.
#[allow(
    clippy::too_many_lines,
    reason = "one flat list of checks, easier to review in one place"
)]
pub(crate) fn check_host_config(
    json: &str,
    kind: crate::RuntimeKind,
    expected: &Expected,
) -> Result<(), String> {
    use serde_json::Value;

    let config: Value =
        serde_json::from_str(json.trim()).map_err(|e| format!("unreadable HostConfig: {e}"))?;
    let int = |key: &str| config.get(key).and_then(Value::as_i64);
    let text = |key: &str| config.get(key).and_then(Value::as_str).unwrap_or_default();
    let list = |key: &str| -> Vec<String> {
        config
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let as_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let mut problems = Vec::new();
    let mut expect = |ok: bool, what: String| {
        if !ok {
            problems.push(what);
        }
    };

    let memory = as_i64(expected.memory);
    expect(
        int("Memory") == Some(memory),
        format!("memory limit {:?} instead of {memory}", int("Memory")),
    );
    expect(
        int("MemorySwap") == Some(memory),
        format!(
            "memory+swap limit {:?} instead of {memory}",
            int("MemorySwap")
        ),
    );
    let pids = as_i64(expected.processes);
    expect(
        int("PidsLimit") == Some(pids),
        format!("process limit {:?} instead of {pids}", int("PidsLimit")),
    );
    let cpus = i64::from(expected.cpus);
    let nano = int("NanoCpus").unwrap_or(0);
    let (quota, period) = (int("CpuQuota").unwrap_or(0), int("CpuPeriod").unwrap_or(0));
    expect(
        nano == cpus * 1_000_000_000 || (nano == 0 && period > 0 && quota == cpus * period),
        format!("CPU limit {nano} nano-CPUs (quota {quota} / period {period}) instead of {cpus}"),
    );
    expect(
        text("NetworkMode") == "none",
        format!("network mode {:?} instead of none", text("NetworkMode")),
    );
    expect(
        config.get("ReadonlyRootfs").and_then(Value::as_bool) == Some(true),
        "the root filesystem is not read-only".to_owned(),
    );
    expect(
        config.get("Privileged").and_then(Value::as_bool) != Some(true),
        "the container is privileged".to_owned(),
    );
    let cap_drop = list("CapDrop");
    let all_dropped = cap_drop.iter().any(|c| c.eq_ignore_ascii_case("ALL"))
        || (kind == crate::RuntimeKind::Podman && !cap_drop.is_empty());
    expect(
        all_dropped && list("CapAdd").is_empty(),
        format!(
            "capabilities not all dropped (drop {cap_drop:?}, add {:?})",
            list("CapAdd")
        ),
    );
    let security = list("SecurityOpt");
    let no_new_privileges = security
        .iter()
        .any(|o| o.starts_with("no-new-privileges") && !o.ends_with("false"));
    let unconfined = security.iter().find(|o| o.contains("unconfined"));
    expect(
        no_new_privileges,
        format!("no-new-privileges is not set (security options {security:?})"),
    );
    expect(
        unconfined.is_none(),
        format!("a security profile is disabled ({unconfined:?})"),
    );

    let ulimits: Vec<(String, i64, i64)> = config
        .get("Ulimits")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|u| {
                    let name = u.get("Name")?.as_str()?.to_ascii_lowercase();
                    let name = name.strip_prefix("rlimit_").unwrap_or(&name).to_owned();
                    Some((name, u.get("Soft")?.as_i64()?, u.get("Hard")?.as_i64()?))
                })
                .collect()
        })
        .unwrap_or_default();
    for (name, soft, hard) in &expected.ulimits {
        let want = (as_i64(*soft), as_i64(*hard));
        let got = ulimits
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|&(_, s, h)| (s, h));
        expect(
            got == Some(want),
            format!("ulimit {name} {got:?} instead of {want:?}"),
        );
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
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

    /// A `HostConfig` as Docker records it for [`expected`].
    fn host_config(memory: i64) -> String {
        serde_json::json!({
            "Memory": memory,
            "MemorySwap": 4096,
            "PidsLimit": 64,
            "NanoCpus": 2_000_000_000_i64,
            "CpuQuota": 0,
            "CpuPeriod": 0,
            "NetworkMode": "none",
            "ReadonlyRootfs": true,
            "Privileged": false,
            "CapDrop": ["ALL"],
            "CapAdd": null,
            "SecurityOpt": ["no-new-privileges"],
            "Ulimits": [
                { "Name": "core", "Soft": 0, "Hard": 0 },
                { "Name": "cpu", "Soft": 70, "Hard": 75 },
            ],
        })
        .to_string()
    }

    fn expected() -> Expected {
        Expected {
            memory: 4096,
            processes: 64,
            cpus: 2,
            ulimits: vec![("core".to_owned(), 0, 0), ("cpu".to_owned(), 70, 75)],
        }
    }

    #[test]
    fn host_config_must_show_every_restriction() {
        use crate::RuntimeKind::{Docker, Podman};
        let ok = host_config(4096);
        assert_eq!(check_host_config(&ok, Docker, &expected()), Ok(()));

        let mut value: serde_json::Value = serde_json::from_str(&ok).unwrap();
        let changed = |key: &str, v: serde_json::Value| {
            let mut value = value.clone();
            value[key] = v;
            value.to_string()
        };
        // A limit the daemon dropped reads as 0.
        for (key, v, what) in [
            ("Memory", serde_json::json!(0), "memory limit"),
            ("MemorySwap", serde_json::json!(-1), "memory+swap"),
            ("PidsLimit", serde_json::json!(0), "process limit"),
            ("NanoCpus", serde_json::json!(0), "CPU limit"),
            ("NetworkMode", serde_json::json!("bridge"), "network mode"),
            ("ReadonlyRootfs", serde_json::json!(false), "read-only"),
            ("Privileged", serde_json::json!(true), "privileged"),
            ("CapDrop", serde_json::json!([]), "capabilities"),
            ("CapAdd", serde_json::json!(["NET_RAW"]), "capabilities"),
            ("SecurityOpt", serde_json::json!([]), "no-new-privileges"),
            (
                "SecurityOpt",
                serde_json::json!(["no-new-privileges", "seccomp=unconfined"]),
                "security profile",
            ),
            (
                "Ulimits",
                serde_json::json!([{ "Name": "core", "Soft": 0, "Hard": 0 }]),
                "ulimit cpu",
            ),
        ] {
            let err = check_host_config(&changed(key, v), Docker, &expected()).unwrap_err();
            assert!(err.contains(what), "{key}: {err}");
        }
        assert!(check_host_config("not json", Docker, &expected()).is_err());

        // Podman's way of recording the same restrictions.
        value["CapDrop"] = serde_json::json!(["CAP_CHOWN", "CAP_KILL"]);
        value["NanoCpus"] = serde_json::json!(0);
        value["CpuQuota"] = serde_json::json!(200_000);
        value["CpuPeriod"] = serde_json::json!(100_000);
        value["Ulimits"] = serde_json::json!([
            { "Name": "RLIMIT_CORE", "Soft": 0, "Hard": 0 },
            { "Name": "RLIMIT_CPU", "Soft": 70, "Hard": 75 },
            { "Name": "RLIMIT_NOFILE", "Soft": 1024, "Hard": 1024 },
        ]);
        let podman = value.to_string();
        assert_eq!(check_host_config(&podman, Podman, &expected()), Ok(()));
        // Listing capabilities one by one is not `ALL` for Docker.
        assert!(check_host_config(&podman, Docker, &expected()).is_err());
    }

    /// A fake `docker` that answers like a daemon which accepted `create`
    /// but recorded `hostconfig.json`, and logs its calls.
    fn fake_runtime(dir: &Path, hostconfig: &str) -> Runtime {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("docker");
        std::fs::write(dir.join("hostconfig.json"), hostconfig).unwrap();
        std::fs::write(
            &script,
            r#"#!/bin/sh
here=$(dirname "$0")
echo "$*" >> "$here/calls"
case "$1" in
  version) echo "29.0.0 linux" ;;
  context) echo "unix:///var/run/docker.sock" ;;
  create) echo "WARNING: this kernel does not support a limit" >&2; echo fake-id ;;
  inspect)
    if [ "$3" = "{{json .HostConfig}}" ]; then cat "$here/hostconfig.json"; else echo "false false 0"; fi ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Runtime::with_program(crate::RuntimeKind::Docker, script).unwrap()
    }

    fn limited_run() -> Spec<'static> {
        Spec::new("/bin/true", Cwd::Path(Path::new("/"))).with_rlimits(
            Rlimits::new()
                .with(Resource::Core, 0)
                .with_soft_hard(Resource::Cpu, 70, 75),
        )
    }

    #[test]
    fn a_container_without_its_limits_is_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let rt = fake_runtime(dir.path(), &host_config(0));
        let container = Container::new(&rt, spec("texrun-engine:latest"));
        let err = container.command(&limited_run()).unwrap_err();
        assert!(
            matches!(&err, RunError::Unsupported(r) if r.contains("memory limit")),
            "{err:?}"
        );
        assert!(container.refusal().unwrap().contains("memory limit"));
        assert_eq!(
            container.warnings(),
            ["WARNING: this kernel does not support a limit"]
        );
        // Removed, never started.
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(calls.contains("rm --force -- fake-id"), "{calls}");
        assert!(!calls.contains("start"), "{calls}");
    }

    #[test]
    fn a_container_with_its_limits_is_started() {
        let dir = tempfile::tempdir().unwrap();
        let rt = fake_runtime(dir.path(), &host_config(4096));
        let container = Container::new(&rt, spec("texrun-engine:latest"));
        let command = container.command(&limited_run()).unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args, ["start", "--attach", "--", "fake-id"]);
        assert_eq!(container.refusal(), None);
    }

    #[test]
    fn a_failed_create_removes_the_container_by_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let rt = fake_runtime(dir.path(), &host_config(4096));
        // From now on `create` fails.
        let script = dir.path().join("docker");
        let text = std::fs::read_to_string(&script)
            .unwrap()
            .replace("echo fake-id", "exit 1");
        std::fs::write(&script, text).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let container = Container::new(&rt, spec("texrun-engine:latest"));
        let err = container.command(&limited_run()).unwrap_err();
        assert!(matches!(err, RunError::Spawn { .. }), "{err:?}");
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(
            calls.contains(&format!("rm --force -- {}", container.name())),
            "{calls}"
        );
    }
}
