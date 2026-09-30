//! Resource limits, set on the child with `prlimit(2)` from the parent or
//! by the exec gate on itself with `setrlimit(2)` (docs/security.md §3.2,
//! §3.10).

use std::io;

/// Whether the parent can set [`Rlimits`] on a running child on this
/// platform (`prlimit(2)` exists on Linux only), as done for
/// [`StartMode::Immediate`](crate::StartMode::Immediate) and
/// [`StartMode::StdinGate`](crate::StartMode::StdinGate). Elsewhere they
/// are not applied in those modes, and a `StdinGate` only delays the start.
/// The exec gate ([`StartMode::ExecGate`](crate::StartMode::ExecGate)) does
/// not need it.
pub const PRLIMIT_SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// A resource that can be limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Resource {
    /// `RLIMIT_FSIZE`: largest file the process may write, in bytes.
    /// Exceeding it raises `SIGXFSZ`, whose default action dumps core, so
    /// it must be combined with [`Resource::Core`] = 0 (checked by
    /// [`run`](crate::run)).
    FileSize,
    /// `RLIMIT_CORE`: largest core file, in bytes (0: none).
    Core,
    /// `RLIMIT_AS`: address space, in bytes.
    ///
    /// Linux only: macOS refuses values below the address space a process
    /// has already mapped (which is large for every process there) and
    /// does not enforce it, so it is never set on macOS (see
    /// [`Finished::rlimits_applied`](crate::Finished::rlimits_applied)).
    AddressSpace,
    /// `RLIMIT_CPU`: CPU time of each process, in seconds. Reaching the
    /// soft limit raises `SIGXCPU`, whose default action dumps core, so it
    /// must be combined with [`Resource::Core`] = 0 (checked by
    /// [`run`](crate::run)); reaching the hard limit raises `SIGKILL`. On
    /// Linux a soft limit equal to the hard one is `SIGKILL` at once, so
    /// give the hard limit some room ([`Rlimits::with_soft_hard`]) when the
    /// signal is to tell the cause.
    Cpu,
    /// `RLIMIT_NPROC`: number of processes. Counted per *user*, not per
    /// process tree (every process of the user on the host counts, and
    /// root is exempt), so a value must leave room for everything else the
    /// user runs. texrun itself does not use it (docs/security.md §3.10).
    Processes,
}

/// A soft and a hard limit (`soft <= hard`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pair {
    pub(crate) soft: u64,
    pub(crate) hard: u64,
}

/// A set of resource limits.
///
/// When applied, each limit only ever lowers what the process already has:
/// the soft limit becomes the smaller of the requested soft limit and the
/// current soft limit, and the hard limit likewise (the child inherited
/// texrun's own limits). So setting them never needs privileges, never
/// raises a limit, and a soft limit the user lowered (e.g. `ulimit -S`)
/// stays in force.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rlimits {
    entries: Vec<(Resource, Pair)>,
}

impl Rlimits {
    /// No limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Limits `resource` to `value` (soft and hard limit), replacing an
    /// earlier value.
    #[must_use]
    pub fn with(self, resource: Resource, value: u64) -> Self {
        self.with_soft_hard(resource, value, value)
    }

    /// Limits `resource` to the soft limit `soft` and the hard limit
    /// `hard`, replacing an earlier value. A `soft` above `hard` is lowered
    /// to `hard`.
    #[must_use]
    pub fn with_soft_hard(mut self, resource: Resource, soft: u64, hard: u64) -> Self {
        let pair = Pair {
            soft: soft.min(hard),
            hard,
        };
        match self.entries.iter_mut().find(|(r, _)| *r == resource) {
            Some(entry) => entry.1 = pair,
            None => self.entries.push((resource, pair)),
        }
        self
    }

    /// Whether no limit is set.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The hard limit of `resource`, if set (the requested value, before
    /// capping).
    pub fn get(&self, resource: Resource) -> Option<u64> {
        self.pair(resource).map(|p| p.hard)
    }

    /// The soft and the hard limit of `resource`, if set (the requested
    /// values, before capping).
    pub fn get_soft_hard(&self, resource: Resource) -> Option<(u64, u64)> {
        self.pair(resource).map(|p| (p.soft, p.hard))
    }

    /// The limits (hard limit), in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (Resource, u64)> + '_ {
        self.entries.iter().map(|&(r, p)| (r, p.hard))
    }

    pub(crate) fn pair(&self, resource: Resource) -> Option<Pair> {
        self.entries
            .iter()
            .find(|(r, _)| *r == resource)
            .map(|&(_, p)| p)
    }

    pub(crate) fn pairs(&self) -> impl Iterator<Item = (Resource, Pair)> + '_ {
        self.entries.iter().copied()
    }

    /// Why this set must not be applied as it is: a limit whose signal
    /// dumps core ([`Resource::FileSize`], [`Resource::Cpu`]) without a
    /// soft [`Resource::Core`] limit of 0.
    pub(crate) fn problem(&self) -> Option<String> {
        let core_off = self.pair(Resource::Core).is_some_and(|p| p.soft == 0);
        self.entries
            .iter()
            .find(|(r, _)| matches!(r, Resource::FileSize | Resource::Cpu))
            .filter(|_| !core_off)
            .map(|(r, _)| {
                format!("{r:?} raises a signal that dumps core; limit Resource::Core to 0 as well")
            })
    }
}

/// The limit to set for `requested`, given the `current` one of this
/// process (`None`: unlimited): never above the current soft or hard limit.
fn capped(requested: Pair, current: rustix::process::Rlimit) -> rustix::process::Rlimit {
    let hard = current
        .maximum
        .map_or(requested.hard, |h| h.min(requested.hard));
    let soft = current
        .current
        .map_or(requested.soft, |s| s.min(requested.soft))
        .min(hard);
    rustix::process::Rlimit {
        current: Some(soft),
        maximum: Some(hard),
    }
}

/// Applies `limits` to the process `pid`. A process that is already gone
/// (`ESRCH`) is not an error; the poll loop sees its exit.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn apply(pid: rustix::process::Pid, limits: &Rlimits) -> io::Result<()> {
    use rustix::io::Errno;
    use rustix::process::{getrlimit, prlimit};

    for (resource, pair) in limits.pairs() {
        let resource = to_rustix(resource);
        // The child inherited our limits.
        let limit = capped(pair, getrlimit(resource));
        match prlimit(Some(pid), resource, limit) {
            Ok(_) | Err(Errno::SRCH) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn to_rustix(resource: Resource) -> rustix::process::Resource {
    use rustix::process::Resource as R;
    match resource {
        Resource::FileSize => R::Fsize,
        Resource::Core => R::Core,
        Resource::AddressSpace => R::As,
        Resource::Cpu => R::Cpu,
        Resource::Processes => R::Nproc,
    }
}

/// Whether `resource` can be set on this platform (by `prlimit` or by the
/// exec gate). See [`Resource::AddressSpace`].
pub(crate) const fn settable(resource: Resource) -> bool {
    match resource {
        Resource::AddressSpace => !cfg!(target_vendor = "apple"),
        Resource::FileSize | Resource::Core | Resource::Cpu | Resource::Processes => true,
    }
}

/// Sets `resource` of this process to `pair`, capped at its current soft
/// and hard limits. Used by the exec gate.
pub(crate) fn set_own(resource: Resource, pair: Pair) -> io::Result<()> {
    use rustix::process::{getrlimit, setrlimit};

    let resource = to_rustix(resource);
    setrlimit(resource, capped(pair, getrlimit(resource))).map_err(Into::into)
}

/// The name of `resource` on the exec gate's command line.
pub(crate) const fn gate_name(resource: Resource) -> &'static str {
    match resource {
        Resource::FileSize => "fsize",
        Resource::Core => "core",
        Resource::AddressSpace => "as",
        Resource::Cpu => "cpu",
        Resource::Processes => "nproc",
    }
}

/// The resource called `name` on the exec gate's command line.
pub(crate) fn from_gate_name(name: &str) -> Option<Resource> {
    [
        Resource::FileSize,
        Resource::Core,
        Resource::AddressSpace,
        Resource::Cpu,
        Resource::Processes,
    ]
    .into_iter()
    .find(|&r| gate_name(r) == name)
}

/// No `prlimit(2)`: nothing is applied (see [`PRLIMIT_SUPPORTED`]).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn apply(_pid: rustix::process::Pid, _limits: &Rlimits) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::process::Rlimit;

    #[test]
    fn values_are_unique_per_resource() {
        let limits = Rlimits::new()
            .with(Resource::FileSize, 10)
            .with(Resource::Core, 0)
            .with(Resource::FileSize, 20);
        assert_eq!(
            limits.iter().collect::<Vec<_>>(),
            [(Resource::FileSize, 20), (Resource::Core, 0)]
        );
        assert_eq!(limits.get(Resource::Core), Some(0));
        assert_eq!(limits.get(Resource::AddressSpace), None);
        assert!(!limits.is_empty());
        assert!(Rlimits::new().is_empty());
    }

    #[test]
    fn soft_limits_never_exceed_hard_limits() {
        let limits = Rlimits::new()
            .with_soft_hard(Resource::Cpu, 10, 15)
            .with_soft_hard(Resource::FileSize, 30, 20);
        assert_eq!(limits.get_soft_hard(Resource::Cpu), Some((10, 15)));
        assert_eq!(limits.get(Resource::Cpu), Some(15));
        assert_eq!(limits.get_soft_hard(Resource::FileSize), Some((20, 20)));
    }

    #[test]
    fn limits_are_capped_at_the_current_soft_and_hard_limits() {
        let req = Pair { soft: 10, hard: 15 };
        let lim = |current, maximum| Rlimit { current, maximum };
        assert_eq!(capped(req, lim(None, None)), lim(Some(10), Some(15)));
        // A lowered soft limit stays in force.
        assert_eq!(capped(req, lim(Some(5), None)), lim(Some(5), Some(15)));
        // Never above the current hard limit, and soft <= hard.
        assert_eq!(capped(req, lim(Some(8), Some(8))), lim(Some(8), Some(8)));
        assert_eq!(capped(req, lim(None, Some(12))), lim(Some(10), Some(12)));
        assert_eq!(capped(req, lim(None, Some(3))), lim(Some(3), Some(3)));
    }

    #[test]
    fn core_dumping_limits_need_core_zero() {
        for resource in [Resource::FileSize, Resource::Cpu] {
            assert!(Rlimits::new().with(resource, 1).problem().is_some());
            assert!(
                Rlimits::new()
                    .with(resource, 1)
                    .with(Resource::Core, 1)
                    .problem()
                    .is_some()
            );
            assert_eq!(
                Rlimits::new()
                    .with(resource, 1)
                    .with_soft_hard(Resource::Core, 0, 10)
                    .problem(),
                None
            );
        }
        assert_eq!(
            Rlimits::new().with(Resource::AddressSpace, 1).problem(),
            None
        );
    }
}
