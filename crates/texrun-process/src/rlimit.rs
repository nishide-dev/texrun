//! Resource limits set on the child with `prlimit(2)` (docs/security.md
//! §3.2).

use std::io;

/// Whether [`Rlimits`] are applied on this platform (`prlimit(2)` exists on
/// Linux only). Elsewhere they are ignored, and a
/// [`StartMode::StdinGate`](crate::StartMode::StdinGate) only delays the
/// start.
pub const PRLIMIT_SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// A resource that can be limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Resource {
    /// `RLIMIT_FSIZE`: largest file the process may write, in bytes.
    /// Exceeding it raises `SIGXFSZ`, whose default action dumps core, so
    /// combine it with [`Resource::Core`] = 0.
    FileSize,
    /// `RLIMIT_CORE`: largest core file, in bytes (0: none).
    Core,
    /// `RLIMIT_AS`: address space, in bytes.
    AddressSpace,
    /// `RLIMIT_CPU`: CPU time, in seconds (#25). Exceeding it raises
    /// `SIGXCPU`, whose default action dumps core, so combine it with
    /// [`Resource::Core`] = 0.
    Cpu,
    /// `RLIMIT_NPROC`: number of processes. Counted per *user*, not per
    /// process tree, so a value must leave room for everything else the
    /// user runs (#25).
    Processes,
}

/// A set of resource limits. Each value becomes both the soft and the hard
/// limit, capped at texrun's own hard limit (which the child inherited), so
/// setting it never needs privileges and never raises a limit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rlimits {
    entries: Vec<(Resource, u64)>,
}

impl Rlimits {
    /// No limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Limits `resource` to `value`, replacing an earlier value.
    #[must_use]
    pub fn with(mut self, resource: Resource, value: u64) -> Self {
        match self.entries.iter_mut().find(|(r, _)| *r == resource) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((resource, value)),
        }
        self
    }

    /// Whether no limit is set.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The limit of `resource`, if set (the requested value, before capping).
    pub fn get(&self, resource: Resource) -> Option<u64> {
        self.entries
            .iter()
            .find(|(r, _)| *r == resource)
            .map(|&(_, v)| v)
    }

    /// The limits, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (Resource, u64)> + '_ {
        self.entries.iter().copied()
    }
}

/// Applies `limits` to the process `pid`. A process that is already gone
/// (`ESRCH`) is not an error; the poll loop sees its exit.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn apply(pid: rustix::process::Pid, limits: &Rlimits) -> io::Result<()> {
    use rustix::io::Errno;
    use rustix::process::{Rlimit, getrlimit, prlimit};

    for (resource, value) in limits.iter() {
        let resource = to_rustix(resource);
        let value = getrlimit(resource)
            .maximum
            .map_or(value, |hard| hard.min(value));
        let limit = Rlimit {
            current: Some(value),
            maximum: Some(value),
        };
        match prlimit(Some(pid), resource, limit) {
            Ok(_) | Err(Errno::SRCH) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
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

/// No `prlimit(2)`: nothing is applied (see [`PRLIMIT_SUPPORTED`]).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn apply(_pid: rustix::process::Pid, _limits: &Rlimits) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
