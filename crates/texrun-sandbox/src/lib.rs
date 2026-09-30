//! Container sandbox for texrun (docs/security.md §4, #26).
//!
//! This crate runs one supervised program ([`texrun_process::Spec`]) inside
//! a hardened container of a Docker or Podman runtime, instead of directly
//! on the host. It knows nothing about TeX: the TeX Live engine
//! (`texrun_texlive::ContainerEngine`) decides what to run and which
//! directories to mount, and uses [`Container`] as the
//! [`Launcher`](texrun_process::Launcher) of its latexmk run.
//!
//! # What a container gets
//!
//! Every container is created with ([`Container`]):
//!
//! - `--network none`: no network at all (#24);
//! - `--read-only`: a read-only root filesystem (the image, including the
//!   TeX Live tree), plus a small `tmpfs` at `/tmp` (`noexec,nosuid,nodev`);
//! - only the [`Mount`]s given, each read-only unless marked writable:
//!   nothing else of the host filesystem is visible;
//! - `--cap-drop ALL`, `--security-opt no-new-privileges`, `--ipc none`;
//! - a non-root user: the uid / gid of texrun (or [`ROOT_FALLBACK_ID`], the
//!   image's own user, when texrun runs as root);
//! - `--pids-limit`, `--memory` (= `--memory-swap`, no swap) and `--cpus`
//!   from [`ContainerLimits`], and the [`Spec::rlimits`](texrun_process::Spec::rlimits)
//!   as `--ulimit` (`RLIMIT_AS`, which the runtimes do not accept, through
//!   `prlimit` in the container before the program starts);
//! - exactly the environment of the spec (`--env`; the runtime adds
//!   `HOSTNAME`, set to `texrun`);
//! - `--init` (an init process as PID 1 that reaps orphans and forwards
//!   signals), `--log-driver none` (the output is only streamed to texrun),
//!   `--pull never` (a missing image is an error, never a download);
//! - a deadline ([`ContainerSpec::deadline`]): the program runs under
//!   `timeout --signal=KILL`, so the container ends on its own even if
//!   texrun is killed and cannot remove it.
//!
//! # Lifecycle
//!
//! [`Launcher::command`](texrun_process::Launcher::command) creates the
//! container (`create`, with a unique name and the label [`LABEL`]), reads
//! back the restrictions the runtime recorded (`inspect`; a runtime that
//! dropped one, e.g. a limit its kernel does not support, fails the run
//! instead of starting a weaker container, [`Container::refusal`]) and
//! returns `start --attach`, which the supervisor spawns and watches like any
//! program: its stdout / stderr are the container's, its exit status the
//! container's. Killing the runtime CLI does not stop a container, so the
//! [`Launcher::on_kill`](texrun_process::Launcher::on_kill) hook (timeout,
//! cancellation, output limit, and after every normal exit) records the
//! container's state and removes it with `rm --force`, which kills what is
//! still running; [`Launcher::on_reaped`](texrun_process::Launcher::on_reaped)
//! and dropping the [`Container`] remove it again if that failed, so a
//! created container is removed on every path through texrun. Containers
//! left by a texrun process that was killed can be found by their label.
//!
//! The runtime CLI itself (the `docker` / `podman` binary, trusted like TeX
//! Live on the host) is always started by path with an argument array and
//! no shell, with only the environment it needs to reach its daemon
//! ([`RUNTIME_ENV`]).
//!
//! # Platform support
//!
//! Unix only, like `texrun-process`. The container runs Linux; on macOS the
//! runtime runs it in a VM (Docker Desktop, `OrbStack`, `podman machine`),
//! and the mounted directories must be shared with that VM (the system
//! temporary directory is by default).

#[cfg(not(unix))]
compile_error!("texrun-sandbox supports Unix hosts only");

mod container;
mod error;
mod runtime;
mod session;

pub use container::{
    Container, ContainerLimits, ContainerOutcome, ContainerSpec, ContainerUser, Mount,
    ROOT_FALLBACK_ID,
};
pub use error::SandboxError;
pub use runtime::{RUNTIME_ENV, Runtime, RuntimeKind};
pub use session::Session;

/// Image used when none is configured. Built from `docker/engine/Dockerfile`
/// (`docker build -t texrun-engine:latest docker/engine`).
pub const DEFAULT_IMAGE: &str = "texrun-engine:latest";

/// Label (`<LABEL>=1`) of every container texrun creates, for finding ones
/// left behind by a texrun process that was killed.
pub const LABEL: &str = "org.texrun.sandbox";
