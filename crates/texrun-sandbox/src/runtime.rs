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
    rootless: bool,
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
            rootless: false,
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
        match kind {
            RuntimeKind::Docker => {
                // Bind mounts name host paths of the daemon's machine: only
                // a daemon on this machine (a Unix socket; Docker Desktop and
                // OrbStack forward theirs to their VM) sees texrun's
                // workspace.
                let endpoint = runtime
                    .query(&[
                        "context",
                        "inspect",
                        "--format",
                        "{{.Endpoints.docker.Host}}",
                    ])
                    .map_err(|e| SandboxError::Unavailable(format!("{kind} is not usable: {e}")))?;
                check_local_endpoint(kind, endpoint.trim())?;
                if let Some(host) = runtime.env.get("DOCKER_HOST") {
                    check_local_endpoint(kind, &host.to_string_lossy())?;
                }
                let security = runtime
                    .query(&["info", "--format", "{{json .SecurityOptions}}"])
                    .map_err(|e| SandboxError::Unavailable(format!("{kind} is not usable: {e}")))?;
                if docker_is_rootless(&security)? {
                    return Err(SandboxError::Unavailable(ROOTLESS_DOCKER.to_owned()));
                }
            }
            RuntimeKind::Podman => {
                // `podman version` answers without a usable service (e.g. a
                // stopped `podman machine`); `podman info` does not.
                // Rootless Podman maps container uids to subordinate ids of
                // the user; `--userns keep-id` keeps texrun's uid, so that
                // the container can write the output directory.
                let info = runtime
                    .query(&[
                        "info",
                        "--format",
                        "{{.Host.Security.Rootless}} {{.Host.CgroupsVersion}} \
                         {{json .Host.CgroupControllers}}",
                    ])
                    .map_err(|e| SandboxError::Unavailable(format!("{kind} is not usable: {e}")))?;
                runtime.rootless = check_podman_cgroups(&info)?;
            }
        }
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

    /// Whether this is rootless Podman (containers get `--userns keep-id`).
    pub fn is_rootless_podman(&self) -> bool {
        self.kind == RuntimeKind::Podman && self.rootless
    }

    /// The ID of the local image `image`, or [`SandboxError::Unavailable`]
    /// if there is none (texrun never pulls one).
    pub fn image_id(&self, image: &str) -> Result<String, SandboxError> {
        self.image(image).map(|i| i.id)
    }

    /// The local image `image` (its ID and version label), or
    /// [`SandboxError::Unavailable`] if there is none (texrun never pulls
    /// one).
    pub fn image(&self, image: &str) -> Result<Image, SandboxError> {
        check_image(image)?;
        // Not `{{json .Config.Labels}}`: Docker 29 leaves `Labels` out of
        // the config of an image without labels (e.g. a local
        // `docker build docker/engine`), and the template then fails.
        // `{{index .Config "Labels"}}` does not work with Podman, whose
        // `.Config` is a struct. The config (env, command, labels, ...) of
        // an image is far below `MAX_OUTPUT`; a longer one fails to parse
        // and the image is unavailable.
        let format = "{{.Id}} {{json .Config}}";
        match self.query(&["image", "inspect", "--format", format, "--", image]) {
            Ok(out) => parse_image(&out).ok_or_else(|| {
                SandboxError::Unavailable(format!(
                    "unexpected `{} image inspect` output for `{image}`: {:?}",
                    self.kind,
                    truncate(out.trim(), 256)
                ))
            }),
            Err(e) => Err(SandboxError::Unavailable(format!(
                "the container image `{image}` is not available to {}: {e} (texrun never \
                 pulls images: pull it with `{} pull {image}`, or build it from docker/engine \
                 in the texrun repository)",
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
        self.exec_output(args, timeout).map(|(stdout, _)| stdout)
    }

    /// [`Self::exec`], also returning the stderr of a successful command
    /// (e.g. the warnings of `create`).
    pub(crate) fn exec_output(
        &self,
        args: &[OsString],
        timeout: Duration,
    ) -> Result<(String, String), SandboxError> {
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
        Ok((
            String::from_utf8_lossy(&finished.stdout.bytes).into_owned(),
            stderr.into_owned(),
        ))
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
            rootless: false,
            env: EnvAllowlist::new(),
        }
    }

    /// [`Runtime::for_tests`] as Podman, rootless or not.
    pub(crate) fn for_tests_podman(rootless: bool) -> Self {
        Self {
            kind: RuntimeKind::Podman,
            program: PathBuf::from("/nonexistent/texrun-test-podman"),
            version: "4.9.3".to_owned(),
            rootless,
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

/// Why rootless Docker is not used (docs/security.md §4).
const ROOTLESS_DOCKER: &str = "docker runs in rootless mode, which texrun does not support: \
     its containers map the user texrun passes to a subordinate uid of the host, which cannot \
     write the output directory (rootless Docker has no `--userns keep-id`). Use rootless Podman \
     (`--container-runtime podman`) or a Docker daemon that runs as root";

/// Whether `docker info --format '{{json .SecurityOptions}}'` says that the
/// daemon runs in rootless mode (`name=rootless`). Output that is not a
/// list of strings is an error: the mode is then unknown.
fn docker_is_rootless(out: &str) -> Result<bool, SandboxError> {
    let options: Option<Vec<String>> = serde_json::from_str(out.trim()).map_err(|_| {
        SandboxError::Unavailable(format!(
            "unexpected `docker info` output: {:?}",
            truncate(out.trim(), 256)
        ))
    })?;
    Ok(options
        .unwrap_or_default()
        .iter()
        .any(|o| o.split(',').next() == Some("name=rootless")))
}

/// The cgroup controllers that the container limits need: `--memory` /
/// `--memory-swap`, `--pids-limit`, `--cpus`.
const LIMIT_CONTROLLERS: &[&str] = &["memory", "pids", "cpu"];

/// Checks `podman info --format '{{.Host.Security.Rootless}}
/// {{.Host.CgroupsVersion}} {{json .Host.CgroupControllers}}'` and returns
/// whether Podman is rootless.
///
/// Rootless Podman can only set the container limits on cgroup v2 with the
/// [`LIMIT_CONTROLLERS`] delegated to the user (on cgroup v1 it ignores
/// them). Without them it is unavailable here, with the reason; the
/// `HostConfig` check of every container would refuse it anyway.
fn check_podman_cgroups(out: &str) -> Result<bool, SandboxError> {
    let unexpected = || {
        SandboxError::Unavailable(format!(
            "unexpected `podman info` output: {:?}",
            truncate(out.trim(), 256)
        ))
    };
    let mut fields = out.trim().splitn(3, ' ');
    let rootless = match fields.next() {
        Some("true") => true,
        Some("false") => false,
        _ => return Err(unexpected()),
    };
    let version = fields.next().ok_or_else(unexpected)?;
    let controllers: Option<Vec<String>> =
        serde_json::from_str(fields.next().ok_or_else(unexpected)?).map_err(|_| unexpected())?;
    if !rootless {
        return Ok(false);
    }
    let controllers = controllers.unwrap_or_default();
    let missing: Vec<&str> = LIMIT_CONTROLLERS
        .iter()
        .copied()
        .filter(|c| !controllers.iter().any(|have| have == c))
        .collect();
    if version != "v2" || !missing.is_empty() {
        return Err(SandboxError::Unavailable(format!(
            "rootless podman cannot limit the containers here: it needs cgroup v2 (this host has \
             {version}) with the {} controllers delegated to the user (missing: {}); see \
             docs/security.md §4",
            LIMIT_CONTROLLERS.join(", "),
            if missing.is_empty() {
                "none".to_owned()
            } else {
                missing.join(", ")
            }
        )));
    }
    Ok(true)
}

/// Refuses a Docker endpoint on another machine (`tcp://`, `ssh://`, ...):
/// its bind mounts would name paths there, not texrun's workspace.
fn check_local_endpoint(kind: RuntimeKind, endpoint: &str) -> Result<(), SandboxError> {
    if endpoint.starts_with("unix://") {
        Ok(())
    } else {
        Err(SandboxError::Unavailable(format!(
            "{kind} uses the daemon at {endpoint:?}; texrun needs a local daemon (a unix:// \
             socket), because the workspace is mounted by its host path"
        )))
    }
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

/// A local image ([`Runtime::image`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Image {
    /// The image ID (`sha256:...`), which compiles are created from.
    pub id: String,
    /// The version of texrun the image was published for: its
    /// [`IMAGE_VERSION_LABEL`](crate::IMAGE_VERSION_LABEL). `None` if the
    /// image has no such label (e.g. a local build), or one that is not a
    /// plain version string.
    pub version: Option<String>,
}

/// The first `max` bytes of `s` (at a character boundary), with `...` if
/// it is longer, for an error message.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let end = (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    format!("{}...", &s[..end])
}

/// Parses `{{.Id}} {{json .Config}}`. For an image without labels, the
/// config's `Labels` is missing (Docker 29), `null` (older Docker, Podman)
/// or `{}`.
fn parse_image(out: &str) -> Option<Image> {
    use serde_json::Value;
    let (id, config) = out.trim().split_once(' ')?;
    if id.is_empty() {
        return None;
    }
    let labels = match serde_json::from_str(config).ok()? {
        Value::Object(mut config) => config.remove("Labels").unwrap_or(Value::Null),
        Value::Null => Value::Null,
        _ => return None,
    };
    let labels = match labels {
        Value::Object(labels) => labels,
        Value::Null => serde_json::Map::new(),
        _ => return None,
    };
    let version = labels
        .get(crate::IMAGE_VERSION_LABEL)
        .and_then(Value::as_str)
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 64
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
        })
        .map(str::to_owned);
    Some(Image {
        id: id.to_owned(),
        version,
    })
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
    fn only_local_docker_endpoints_are_used() {
        let docker = RuntimeKind::Docker;
        assert!(check_local_endpoint(docker, "unix:///var/run/docker.sock").is_ok());
        assert!(check_local_endpoint(docker, "unix:///Users/u/.orbstack/run/docker.sock").is_ok());
        for remote in [
            "tcp://10.0.0.1:2376",
            "ssh://host",
            "npipe:////./pipe/x",
            "",
        ] {
            assert!(
                matches!(
                    check_local_endpoint(docker, remote),
                    Err(SandboxError::Unavailable(_))
                ),
                "{remote}"
            );
        }
    }

    #[test]
    fn rootless_docker_is_detected() {
        for (out, rootless) in [
            (
                r#"["name=apparmor","name=seccomp,profile=builtin","name=cgroupns"]"#,
                false,
            ),
            (
                r#"["name=seccomp,profile=builtin","name=rootless","name=cgroupns"]"#,
                true,
            ),
            ("[\"name=rootless,foo=bar\"]\n", true),
            ("[]", false),
            ("null", false),
        ] {
            assert_eq!(docker_is_rootless(out).unwrap(), rootless, "{out}");
        }
        for bad in ["", "rootless", "{}", "[1]"] {
            assert!(
                matches!(docker_is_rootless(bad), Err(SandboxError::Unavailable(_))),
                "{bad:?}"
            );
        }
        assert!(ROOTLESS_DOCKER.contains("podman"));
    }

    #[test]
    fn rootless_podman_needs_delegated_cgroup_controllers() {
        let delegated = "true v2 [\"cpuset\",\"cpu\",\"io\",\"memory\",\"pids\"]\n";
        assert!(check_podman_cgroups(delegated).unwrap());
        // Rootful: the HostConfig check decides.
        assert!(!check_podman_cgroups("false v1 null").unwrap());
        assert!(!check_podman_cgroups("false v2 []").unwrap());
        for (out, what) in [
            ("true v1 null", "has v1"),
            ("true v2 [\"memory\",\"pids\"]", "missing: cpu"),
            ("true v2 []", "missing: memory, pids, cpu"),
            ("true v2 null", "missing: memory"),
        ] {
            let err = check_podman_cgroups(out).unwrap_err().to_string();
            assert!(err.contains(what), "{out}: {err}");
        }
        for bad in ["", "yes v2 []", "true", "true v2", "true v2 [1]"] {
            assert!(
                matches!(check_podman_cgroups(bad), Err(SandboxError::Unavailable(_))),
                "{bad:?}"
            );
        }
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
    fn image_inspect_output_is_parsed() {
        let labelled = format!(
            "sha256:0123 {{\"Env\":[\"PATH=/bin\"],\"Labels\":{{\"{}\":\"0.1.0\",\"other\":\"x\"}}}}\n",
            crate::IMAGE_VERSION_LABEL
        );
        assert_eq!(
            parse_image(&labelled),
            Some(Image {
                id: "sha256:0123".into(),
                version: Some("0.1.0".into())
            })
        );
        for unlabelled in [
            // Docker 29: no `Labels` key.
            "sha256:0123 {\"Env\":[\"PATH=/bin\"],\"Cmd\":[\"bash\"]}",
            "sha256:0123 {}",
            // Older Docker and Podman: `null` or `{}`.
            "sha256:0123 {\"Labels\":null}",
            "sha256:0123 {\"Labels\":{}}",
            "sha256:0123 {\"Labels\":{\"a\":\"b\"}}",
            // An image without a config at all.
            "sha256:0123 null",
        ] {
            assert_eq!(
                parse_image(unlabelled),
                Some(Image {
                    id: "sha256:0123".into(),
                    version: None
                }),
                "{unlabelled}"
            );
        }
        // Only a plain version string is reported.
        let odd = format!(
            "sha256:0123 {{\"Labels\":{{\"{}\":\"1.0 \\u001b[31m\"}}}}",
            crate::IMAGE_VERSION_LABEL
        );
        assert_eq!(parse_image(&odd).unwrap().version, None);
        let not_a_string = format!(
            "sha256:0123 {{\"Labels\":{{\"{}\":1}}}}",
            crate::IMAGE_VERSION_LABEL
        );
        assert_eq!(parse_image(&not_a_string).unwrap().version, None);
        for bad in [
            "",
            "sha256:0123",
            " {}",
            "sha256:0123 {",
            "sha256:0123 []",
            "sha256:0123 {\"Labels\":[]}",
            "sha256:0123 {\"Labels\":\"x\"}",
        ] {
            assert_eq!(parse_image(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn long_output_is_truncated_in_messages() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "abc...");
        // Not inside a character.
        assert_eq!(truncate("aé", 2), "a...");
        assert_eq!(truncate(&"x".repeat(70_000), 256).len(), 259);
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
