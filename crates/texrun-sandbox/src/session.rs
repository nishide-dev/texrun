//! One hardened container in which several programs run one after another
//! (the page previews, #46).

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use texrun_process::{Cwd, Launcher, Resource, Rlimits, RunError, Spec};

use crate::container::{Container, ContainerSpec};
use crate::error::SandboxError;
use crate::runtime::Runtime;

/// Where `sleep` is expected in the image (coreutils): the main process of
/// a session, which bounds its lifetime.
const SLEEP: &str = "/usr/bin/sleep";

/// Where `prlimit` is expected in the image (util-linux).
const PRLIMIT: &str = "/usr/bin/prlimit";

/// Timeout of `start` (detached: returns once the main process runs).
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// A container that stays up for several supervised runs: pass it as the
/// [`Launcher`] to [`texrun_process::run_with`] once per program, each
/// with the [`Spec`] of what to run in it (an absolute program in the
/// image, its arguments, the complete environment, the working directory as
/// a path in the container, and the rlimits).
///
/// Starting a container costs about 0.2 s (create, the check of its
/// restrictions, start), running a program in a started one (`exec`) about
/// 0.03 s (docs/security.md §4 "preview"), so a session is for many short
/// runs, where [`Container`] (one container per run) would spend most of the
/// time starting containers.
///
/// # What differs from [`Container`]
///
/// The container is created with the same restrictions ([`ContainerSpec`]:
/// no network, read-only root filesystem, only the given mounts, no
/// capabilities, non-root user, the cgroup limits), checked in the same way
/// before it is started ([`SandboxError::Refused`]). Its main process is
/// `sleep` for the [lifetime](Session::start) of the session; the runs are
/// `exec`s into it and share the container's cgroup (they run one after
/// another, so the limits of the cgroup apply to one run at a time), its
/// mounts and its `/tmp`.
///
/// Each run gets all of its [`Spec::rlimits`] through `prlimit` in the
/// container before the program starts. They must not exceed the hard
/// limits the session was started with (`ulimits`); `prlimit` fails the
/// run otherwise.
///
/// Killing the runtime CLI of an `exec` does not stop the program in the
/// container, so when the supervisor kills a run that did not end on its
/// own (timeout, cancellation, a failed check), the whole session is removed
/// ([`Session::stop`]) and later runs fail. Dropping the session removes
/// the container too; if texrun is killed, the main process ends after
/// the lifetime and the stopped container is left until a later texrun
/// removes it ([`Runtime::reclaim_left_containers`]).
#[derive(Debug)]
pub struct Session<'r> {
    container: Container<'r>,
}

impl<'r> Session<'r> {
    /// Creates a container of `spec` (its [`ContainerSpec::deadline`] is not
    /// used), checks its restrictions and starts it. It stays up for at
    /// most `lifetime`, with `ulimits` (not [`Resource::AddressSpace`],
    /// which the runtimes do not set) on every process in it.
    pub fn start(
        runtime: &'r Runtime,
        spec: ContainerSpec,
        lifetime: Duration,
        ulimits: Rlimits,
    ) -> Result<Self, SandboxError> {
        if ulimits.get(Resource::AddressSpace).is_some() {
            return Err(SandboxError::Invalid(
                "the address space cannot be limited for a whole session".to_owned(),
            ));
        }
        let container = Container::new(runtime, spec.with_deadline(None));
        let main = Spec::new(SLEEP, Cwd::Path(Path::new("/")))
            .with_args([lifetime.as_secs().max(1).to_string()])
            .with_rlimits(ulimits);
        if let Err(e) = container.create(&main) {
            return Err(match container.refusal() {
                Some(reason) => SandboxError::Refused(reason),
                None => run_error(runtime, "create", e),
            });
        }
        let id = container.id().expect("created");
        let args: Vec<OsString> = ["start", "--", &id].map(OsString::from).into();
        // On an error `container` is dropped here, which removes it.
        runtime.exec(&args, START_TIMEOUT)?;
        Ok(Self { container })
    }

    /// The container's name (unique per texrun process).
    pub fn name(&self) -> &str {
        self.container.name()
    }

    /// What the runtime printed on stderr while creating the container
    /// (e.g. that the kernel does not support a limit), one line each.
    pub fn warnings(&self) -> Vec<String> {
        self.container.warnings()
    }

    /// Removes the container, killing whatever runs in it. Later runs fail.
    pub fn stop(&self) {
        self.container.stop();
    }

    /// Whether the session was stopped ([`Session::stop`], or a run was
    /// killed).
    pub fn is_stopped(&self) -> bool {
        self.container.is_removed()
    }

    /// Whether the kernel's OOM killer has stopped a process in the
    /// container (`--memory`); `None` if the runtime cannot tell (e.g. the
    /// session was stopped).
    pub fn oom_killed(&self) -> Option<bool> {
        if self.is_stopped() {
            return None;
        }
        self.container.oom_killed_now()
    }

    /// The `exec` arguments for `spec`.
    pub(crate) fn exec_args(spec: &Spec<'_>, id: &str) -> Result<Vec<OsString>, RunError> {
        let invalid = |message: String| RunError::InvalidSpec(message);
        let Cwd::Path(workdir) = spec.cwd else {
            return Err(invalid(
                "a container run needs its working directory as a path in the container".to_owned(),
            ));
        };
        for (what, path) in [
            ("program", spec.program.as_path()),
            ("working directory", workdir),
        ] {
            if !path.is_absolute() {
                return Err(invalid(format!(
                    "the {what} must be an absolute path in the container: {}",
                    path.display()
                )));
            }
        }
        let mut args: Vec<OsString> = vec![
            "exec".into(),
            "--workdir".into(),
            workdir.as_os_str().to_owned(),
        ];
        for (name, value) in spec.env.vars() {
            let name_text = name.to_str().unwrap_or_default();
            if name_text.is_empty() || name_text.contains('=') {
                return Err(invalid(format!(
                    "invalid environment variable name `{}`",
                    name.display()
                )));
            }
            let mut pair = name.to_owned();
            pair.push("=");
            pair.push(value);
            args.extend(["--env".into(), pair]);
        }
        args.extend(["--".into(), id.into()]);
        if !spec.rlimits.is_empty() {
            args.push(PRLIMIT.into());
            for (resource, _) in spec.rlimits.iter() {
                let (soft, hard) = spec
                    .rlimits
                    .get_soft_hard(resource)
                    .expect("listed by iter");
                let name = prlimit_name(resource).ok_or_else(|| {
                    invalid(format!("{resource:?} cannot be limited in a container"))
                })?;
                args.push(format!("--{name}={soft}:{hard}").into());
            }
            args.push("--".into());
        }
        args.push(spec.program.as_os_str().to_owned());
        args.extend(spec.args.iter().cloned());
        Ok(args)
    }
}

impl Launcher for Session<'_> {
    fn command(&self, spec: &Spec<'_>) -> Result<Command, RunError> {
        let runtime = self.container.runtime();
        let stopped = || RunError::Spawn {
            program: format!("{} exec", runtime.kind()),
            source: io::Error::other("the container of the session was stopped"),
        };
        if self.is_stopped() {
            return Err(stopped());
        }
        let id = self.container.id().ok_or_else(stopped)?;
        let args = Self::exec_args(spec, &id)?;
        let mut cmd = Command::new(runtime.program());
        cmd.args(args)
            .env_clear()
            .envs(runtime.env().vars())
            .current_dir("/");
        Ok(cmd)
    }

    /// The limits are set in the container (`prlimit`, the cgroup).
    fn apply_rlimits(&self) -> bool {
        false
    }

    /// A run that did not end on its own may still run in the container:
    /// remove the whole session.
    fn on_kill(&self, pid: u32) {
        if !exited_on_its_own(pid) {
            self.stop();
        }
    }
}

/// Whether the (not yet reaped) child `pid` exited by itself, rather than
/// being killed or still running.
fn exited_on_its_own(pid: u32) -> bool {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};

    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    loop {
        match waitid(WaitId::Pid(pid), options) {
            Ok(Some(status)) => return status.exited(),
            Err(rustix::io::Errno::INTR) => {}
            Ok(None) | Err(_) => return false,
        }
    }
}

/// The `prlimit` option of `resource`.
fn prlimit_name(resource: Resource) -> Option<&'static str> {
    match resource {
        Resource::FileSize => Some("fsize"),
        Resource::Core => Some("core"),
        Resource::AddressSpace => Some("as"),
        Resource::Cpu => Some("cpu"),
        Resource::Processes => Some("nproc"),
        _ => None,
    }
}

/// A failed `create` as a sandbox error.
fn run_error(runtime: &Runtime, command: &str, e: RunError) -> SandboxError {
    match e {
        RunError::InvalidSpec(message) => SandboxError::Invalid(message),
        RunError::Spawn { source, .. } => SandboxError::Runtime {
            command: format!("{} {command}", runtime.kind()),
            message: source.to_string(),
        },
        other => SandboxError::Runtime {
            command: format!("{} {command}", runtime.kind()),
            message: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::ContainerLimits;
    use texrun_process::EnvAllowlist;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn exec_args_set_every_limit_before_the_program() {
        let run = Spec::new("/usr/bin/mutool", Cwd::Path(Path::new("/texrun/work")))
            .with_args(["draw", "-o", "page.png"])
            .with_env(EnvAllowlist::new().with("HOME", "/texrun/home"))
            .with_rlimits(
                Rlimits::new()
                    .with(Resource::AddressSpace, 2048)
                    .with(Resource::FileSize, 100)
                    .with_soft_hard(Resource::Cpu, 40, 45)
                    .with(Resource::Core, 0),
            );
        let args = strings(&Session::exec_args(&run, "the-id").unwrap());
        let joined = args.join(" ");
        assert!(
            joined.starts_with("exec --workdir /texrun/work --env HOME=/texrun/home -- the-id "),
            "{joined}"
        );
        for limit in [
            "--as=2048:2048",
            "--fsize=100:100",
            "--cpu=40:45",
            "--core=0:0",
        ] {
            assert!(joined.contains(limit), "missing {limit}: {joined}");
        }
        assert!(
            joined.ends_with("-- /usr/bin/mutool draw -o page.png"),
            "{joined}"
        );

        for bad in [
            Spec::new("mutool", Cwd::Path(Path::new("/w"))),
            Spec::new("/bin/x", Cwd::Path(Path::new("w"))),
            Spec::new("/bin/x", Cwd::Path(Path::new("/w")))
                .with_env(EnvAllowlist::new().with("A=B", "c")),
        ] {
            assert!(Session::exec_args(&bad, "id").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_session_limits_no_address_space() {
        let rt = Runtime::for_tests();
        let err = Session::start(
            &rt,
            ContainerSpec::new("x", ContainerLimits::new(4096, 32, 2)),
            Duration::from_secs(60),
            Rlimits::new().with(Resource::AddressSpace, 1 << 30),
        )
        .unwrap_err();
        assert!(matches!(err, SandboxError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn a_session_whose_restrictions_were_dropped_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let rt = crate::container::tests::fake_runtime(
            dir.path(),
            &crate::container::tests::host_config(0),
        );
        let err = Session::start(
            &rt,
            ContainerSpec::new("texrun-engine:latest", ContainerLimits::new(4096, 64, 2)),
            Duration::from_secs(60),
            Rlimits::new()
                .with(Resource::Core, 0)
                .with_soft_hard(Resource::Cpu, 70, 75),
        )
        .unwrap_err();
        assert!(
            matches!(&err, SandboxError::Refused(r) if r.contains("memory limit")),
            "{err:?}"
        );
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(calls.contains("rm --force -- fake-id"), "{calls}");
        assert!(!calls.lines().any(|l| l.starts_with("start")), "{calls}");
    }

    #[test]
    fn a_started_session_runs_programs_with_exec() {
        let dir = tempfile::tempdir().unwrap();
        let rt = crate::container::tests::fake_runtime(
            dir.path(),
            &crate::container::tests::host_config(4096),
        );
        let session = Session::start(
            &rt,
            ContainerSpec::new("texrun-engine:latest", ContainerLimits::new(4096, 64, 2)),
            Duration::from_secs(60),
            Rlimits::new()
                .with(Resource::Core, 0)
                .with_soft_hard(Resource::Cpu, 70, 75),
        )
        .unwrap();
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(
            calls.contains("--entrypoint /usr/bin/sleep -- texrun-engine:latest 60"),
            "{calls}"
        );
        assert!(calls.contains("start -- fake-id"), "{calls}");
        let run = Spec::new("/bin/true", Cwd::Path(Path::new("/")));
        let command = session.command(&run).unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            ["exec", "--workdir", "/", "--", "fake-id", "/bin/true"]
        );
        assert!(!session.is_stopped());
        session.stop();
        assert!(session.is_stopped());
        assert!(session.command(&run).is_err(), "stopped");
    }
}
