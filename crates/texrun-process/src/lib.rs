//! Supervised execution of external programs for texrun
//! (docs/security.md §3.2, §3.4, §3.6).
//!
//! Every program texrun starts (latexmk, the preview tools) runs through
//! [`run`], which applies the same rules:
//!
//! - **no shell, cleared environment**: the program is started by path with
//!   an argv array, after `env_clear()`, with only the variables of an
//!   [`EnvAllowlist`] (whose `PATH` never has empty or relative entries,
//!   [`sanitize_path`]);
//! - **own process group**: the child leads a new process group
//!   (`process_group(0)`). On timeout, cancellation or a failed
//!   [`Watch::with_check`] the whole group gets `SIGKILL`, and once more
//!   after a normal exit, so no descendant outlives the run;
//! - **kill before reap**: the leader's exit is detected with
//!   `waitid(WEXITED | WNOHANG | WNOWAIT)`, which leaves it a zombie. The
//!   zombie keeps its PID and PGID reserved, so `killpg` cannot reach an
//!   unrelated group; the leader is reaped only after the group was killed.
//!   A guard does the same if the caller unwinds (e.g. a panicking check);
//! - **one poll loop** checks, in this order, the leader's exit, the
//!   [`CancelToken`](texrun_core::CancelToken), the deadline and the
//!   caller's check hook, every [`POLL_INTERVAL`];
//! - **bounded output**: stdout / stderr are drained to EOF by reader
//!   threads, keeping only a prefix of each ([`Capture::Keep`]); the readers
//!   are waited for at most [`READER_GRACE`] after the group is gone;
//! - **resource limits**: [`Rlimits`] never exceed texrun's own hard
//!   limit. Setting them in the child between `fork` and `exec` would need
//!   `pre_exec`, i.e. `unsafe`, which the workspace forbids, so there are
//!   three start modes ([`StartMode`]):
//!   - [`StartMode::ExecGate`]: the program is started through an exec gate
//!     ([`run_gate`], configured with [`ExecGate`]) that sets the limits on
//!     itself with `setrlimit(2)` and then `exec`s the program, so the
//!     program and all its descendants are limited from the first
//!     instruction, on Linux and macOS;
//!   - [`StartMode::StdinGate`]: on Linux, the limits are set with
//!     `prlimit(2)` from the parent while the program itself waits at a
//!     caller-defined start gate on stdin (e.g. the latexmk rc);
//!   - [`StartMode::Immediate`]: `prlimit(2)` right after the spawn, best
//!     effort only (see its documentation for the gap this leaves).
//!
//! `EINTR` is retried everywhere (`waitid`, pipe reads).
//!
//! # Extension points
//!
//! - [`Resource`] already names `RLIMIT_CPU` / `RLIMIT_NPROC` for #25; the
//!   values are the caller's choice.
//! - The exec gate runs its steps before `exec` in a fixed order
//!   ([`run_gate`]); a cgroup attach (#25) can be done by the parent in
//!   [`Launcher::on_spawn`] while the gate waits, or added as a gate step.
//! - A [`Launcher`] decides *how* a [`Spec`] is started. [`HostLauncher`]
//!   runs it directly on the host; a container backend (#26) can turn the
//!   same spec into a runtime invocation, and a cgroup-based limiter (#25)
//!   can attach the child on spawn ([`Launcher::on_spawn`]) and kill the
//!   whole cgroup together with the group ([`Launcher::on_kill`]) and
//!   clean up after the reap ([`Launcher::on_reaped`]).
//!
//! # Platform support
//!
//! Unix only. `prlimit(2)` exists on Linux only ([`PRLIMIT_SUPPORTED`]); on
//! other Unix systems (macOS) [`Rlimits`] are only applied through the exec
//! gate (without it they are not applied, recorded in
//! [`Finished::rlimits_applied`]; [`Spec::require_rlimits`] turns this into
//! [`RunError::Unsupported`]) and callers rely
//! on their check hooks.

#[cfg(not(unix))]
compile_error!("texrun-process supports Unix hosts only");

mod capture;
mod env;
mod error;
mod gate;
mod rlimit;
mod spec;
mod supervise;

pub use capture::CapturedOutput;
pub use env::{EnvAllowlist, sanitize_path};
pub use error::RunError;
pub use gate::{ExecGate, run_gate};
pub use rlimit::{PRLIMIT_SUPPORTED, Resource, Rlimits};
pub use spec::{Capture, Cwd, HostLauncher, Launcher, Spec, StartMode};
pub use supervise::{Finished, POLL_INTERVAL, READER_GRACE, Stop, Watch, run, run_with};
