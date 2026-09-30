//! The container runtime CLI (Docker or Podman).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use texrun_process::{Capture, Cwd, EnvAllowlist, Spec, Watch, sanitize_path};

use crate::error::SandboxError;

/// Host environment variables passed to the runtime CLI (not to the
/// container): what the Docker and Podman clients need to find their
/// daemon, context or rootless storage. `PATH` is passed without empty and
/// relative entries.
pub const RUNTIME_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "CONTAINER_HOST",
    "CONTAINER_CONNECTION",
    "CONTAINERS_CONF",
    "CONTAINERS_STORAGE_CONF",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
];

/// Timeout of the short runtime commands (`version`, `image inspect`,
/// `inspect`).
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// At most this much of a runtime command's stdout / stderr is kept.
const MAX_OUTPUT: usize = 64 * 1024;

/// Which container runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuntimeKind {
    /// Docker (Docker Engine, Docker Desktop, `OrbStack`, ...).
    Docker,
    /// Podman, typically rootless.
    Podman,
}

impl RuntimeKind {
    /// Oldest supported version (`major.minor`): Docker 20.10 (`--pull`,
    /// `--init`, `--pids-limit` with cgroup v2), Podman 4.0.
    pub fn min_version(self) -> (u32, u32) {
        match self {
            Self::Docker => (20, 10),
            Self::Podman => (4, 0),
        }
    }

    /// The executable name looked up in `PATH`.
    pub fn program_name(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }
}

impl fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.program_name())
    }
}

/// A container runtime CLI whose daemon (Docker) or service (Podman) was
/// reachable when it was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    kind: RuntimeKind,
    program: PathBuf,
    version: String,
    env: EnvAllowlist,
}

impl Runtime {
    /// Finds and checks a runtime: `kind`, or, for `None`, Docker and then
    /// Podman (the first that is installed and answers).
    ///
    /// The program is looked up in the host `PATH` (absolute entries only).
    /// Fails with [`SandboxError::Unavailable`] if none is installed, the
    /// daemon cannot be reached, or the version is too old
    /// ([`RuntimeKind::min_version`]).
    pub fn detect(kind: Option<RuntimeKind>) -> Result<Self, SandboxError> {
        let env = runtime_env();
        let path = env.get("PATH").unwrap_or_default().to_owned();
        let kinds = match kind {
            Some(kind) => vec![kind],
            None => vec![RuntimeKind::Docker, RuntimeKind::Podman],
        };
        let mut reasons = Vec::new();
        for kind in kinds {
            let Some(program) = find_in_path(kind.program_name(), &path) else {
                reasons.push(format!("`{kind}` was not found in PATH"));
                continue;
            };
            match Self::with_program(kind, program) {
                Ok(runtime) => return Ok(runtime),
                Err(e) => reasons.push(e.to_string()),
            }
        }
        Err(SandboxError::Unavailable(format!(
            "no usable container runtime: {}",
            reasons.join("; ")
        )))
    }

    /// Checks the runtime `kind` at `program` (an absolute path).
    pub fn with_program(kind: RuntimeKind, program: PathBuf) -> Result<Self, SandboxError> {
        if !program.is_absolute() {
            return Err(SandboxError::Invalid(format!(
                "the {kind} program must be an absolute path: {}",
                program.display()
            )));
        }
        let mut runtime = Self {
            kind,
            program,
            version: String::new(),
            env: runtime_env(),
        };
        // The server version (Docker): only answered when the daemon is
        // reachable. Podman runs without a daemon (or through its own
        // machine / service connection).
        let format = match kind {
            RuntimeKind::Docker => "{{.Server.Version}} {{.Server.Os}}",
            RuntimeKind::Podman => "{{.Client.Version}} linux",
        };
        let out = runtime
            .query(&["version", "--format", format])
            .map_err(|e| SandboxError::Unavailable(format!("{kind} is not usable: {e}")))?;
        let (version, os) = out.trim().split_once(' ').unwrap_or((out.trim(), ""));
        if os != "linux" {
            return Err(SandboxError::Unavailable(format!(
                "{kind} runs {os:?} containers; texrun needs Linux containers"
            )));
        }
        let (min_major, min_minor) = kind.min_version();
        match parse_version(version) {
            Some((major, minor)) if (major, minor) >= (min_major, min_minor) => {}
            _ => {
                return Err(SandboxError::Unavailable(format!(
                    "{kind} {version:?} is not supported (at least {min_major}.{min_minor} is \
                     needed)"
                )));
            }
        }
        version.clone_into(&mut runtime.version);
        Ok(runtime)
    }

    /// Which runtime.
    pub fn kind(&self) -> RuntimeKind {
        self.kind
    }

    /// The runtime CLI.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The version reported by the runtime (the daemon's for Docker).
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The ID of the local image `image`, or [`SandboxError::Unavailable`]
    /// if there is none (texrun never pulls one).
    pub fn image_id(&self, image: &str) -> Result<String, SandboxError> {
        check_image(image)?;
        match self.query(&["image", "inspect", "--format", "{{.Id}}", "--", image]) {
            Ok(id) => Ok(id.trim().to_owned()),
            Err(e) => Err(SandboxError::Unavailable(format!(
                "the container image `{image}` is not available to {}: {e} (build it with \
                 `{} build -t {image} docker/engine`)",
                self.kind, self.kind
            ))),
        }
    }

    /// The environment of the runtime CLI.
    pub(crate) fn env(&self) -> &EnvAllowlist {
        &self.env
    }

    /// Runs a short runtime command and returns its stdout.
    pub(crate) fn query(&self, args: &[&str]) -> Result<String, SandboxError> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        self.exec(&args, QUERY_TIMEOUT)
    }

    /// Runs the runtime with `args` (no shell) and returns its stdout, or an
    /// error with its stderr if it fails or takes longer than `timeout`.
    pub(crate) fn exec(
        &self,
        args: &[OsString],
        timeout: Duration,
    ) -> Result<String, SandboxError> {
        let command = format!(
            "{} {}",
            self.kind,
            args.first()
                .map_or(String::new(), |a| a.to_string_lossy().into_owned())
        );
        let spec = Spec::new(&self.program, Cwd::Path(Path::new("/")))
            .with_args(args.iter().cloned())
            .with_env(self.env.clone())
            .with_stdout(Capture::Keep(MAX_OUTPUT))
            .with_stderr(Capture::Keep(MAX_OUTPUT));
        let failed = |message: String| SandboxError::Runtime {
            command: command.clone(),
            message,
        };
        let finished = texrun_process::run(&spec, Watch::<()>::new().with_timeout(timeout))
            .map_err(|e| failed(e.to_string()))?;
        if finished.stop.is_some() {
            return Err(failed(format!("no answer within {} s", timeout.as_secs())));
        }
        let stderr = String::from_utf8_lossy(&finished.stderr.bytes);
        if !finished.status.success() {
            let message = stderr.trim();
            return Err(failed(if message.is_empty() {
                format!("{}", finished.status)
            } else {
                message.to_owned()
            }));
        }
        Ok(String::from_utf8_lossy(&finished.stdout.bytes).into_owned())
    }
}

#[cfg(test)]
impl Runtime {
    /// A runtime that was never detected, for unit tests of the arguments.
    pub(crate) fn for_tests() -> Self {
        Self {
            kind: RuntimeKind::Docker,
            program: PathBuf::from("/nonexistent/texrun-test-docker"),
            version: "29.0.0".to_owned(),
            env: EnvAllowlist::new(),
        }
    }
}

/// The environment of the runtime CLI: [`RUNTIME_ENV`] from texrun's own
/// environment.
fn runtime_env() -> EnvAllowlist {
    let mut env = EnvAllowlist::new();
    for &name in RUNTIME_ENV {
        if let Some(value) = std::env::var_os(name) {
            if name == "PATH" {
                env.set(name, sanitize_path(&value));
            } else {
                env.set(name, value);
            }
        }
    }
    env
}

/// An executable file called `name` in `path`.
fn find_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// `major.minor` of a version such as `29.4.0`, `20.10.24+dfsg1` or
/// `5.4.0-dev`.
fn parse_version(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split(['.', '-', '+']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// An image reference texrun passes to the runtime: `[registry/]name[:tag]`
/// or `name@sha256:<digest>`, with the characters of the reference grammar
/// only.
pub(crate) fn check_image(image: &str) -> Result<(), SandboxError> {
    let valid = !image.is_empty()
        && image.len() <= 512
        && !image.starts_with('-')
        && image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '@'));
    if valid {
        Ok(())
    } else {
        Err(SandboxError::Invalid(format!(
            "invalid container image reference {image:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_parsed_and_compared() {
        assert_eq!(parse_version("29.4.0"), Some((29, 4)));
        assert_eq!(parse_version("20.10.24+dfsg1"), Some((20, 10)));
        assert_eq!(parse_version("5.4.0-dev"), Some((5, 4)));
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("4"), None);
        assert!((20, 10) >= RuntimeKind::Docker.min_version());
        assert!((19, 3) < RuntimeKind::Docker.min_version());
    }

    #[test]
    fn image_references_are_checked() {
        for ok in [
            "texrun-engine:latest",
            "ghcr.io/nishide-dev/texrun-engine:1.0",
            "texrun-engine@sha256:0123abcd",
            "localhost:5000/x/y",
        ] {
            assert!(check_image(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "a b", "a;b", "x\n", "$(x)"] {
            assert!(check_image(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_missing_runtime_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_in_path("docker", dir.path().as_os_str()), None);
        let err = Runtime::with_program(RuntimeKind::Docker, dir.path().join("docker"));
        assert!(matches!(err, Err(SandboxError::Unavailable(_))), "{err:?}");
        assert!(matches!(
            Runtime::with_program(RuntimeKind::Podman, "podman".into()),
            Err(SandboxError::Invalid(_))
        ));
    }

    #[test]
    fn the_runtime_env_is_an_allowlist() {
        let env = runtime_env();
        for (name, _) in env.vars() {
            assert!(
                RUNTIME_ENV.iter().any(|n| OsStr::new(n) == name),
                "{name:?}"
            );
        }
    }
}
