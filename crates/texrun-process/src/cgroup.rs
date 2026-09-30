//! cgroup v2 limits for a whole process tree (Linux, docs/security.md
//! §3.10).
//!
//! Every run gets its own child cgroup of a *delegated* cgroup that texrun
//! may manage ([`Cgroups::detect`]). The supervisor creates it with the
//! limits ([`CgroupLimits`]: `memory.max`, `pids.max`, `cpu.max`) before
//! spawning, moves the child into it right after the spawn, while the
//! child still waits at its start gate (so every descendant is inside),
//! kills it with `cgroup.kill` together with the process group (which also
//! reaches processes that left the group), and removes it after reading
//! its events (`memory.events`, `pids.events`).
//!
//! Where no delegated cgroup can be used (macOS, a read-only cgroup mount,
//! a cgroup shared with other processes, no `cgroup.kill`), [`Cgroups`] is
//! *unavailable* with the reason, and runs go on with the rlimits only
//! (recorded in [`Finished::cgroup`](crate::Finished::cgroup)), unless the
//! caller made it required ([`Cgroups::with_required`]).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Limits of a run's cgroup. `None` leaves a limit at the parent's value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CgroupLimits {
    /// `memory.max`, in bytes, for all processes of the run together
    /// (page cache included; the kernel reclaims that first). The run's
    /// cgroup also gets `memory.swap.max = 0` (where swap accounting
    /// exists), so the limit is not stretched by swap, and
    /// `memory.oom.group = 1`, so the OOM killer stops the whole run.
    pub memory_max: Option<u64>,
    /// `pids.max`: processes *and threads* of the run together.
    pub pids_max: Option<u64>,
    /// `cpu.max`: at most `quota` microseconds of CPU time per `period`
    /// microseconds (e.g. `(200_000, 100_000)`: two CPUs). Only set where
    /// the `cpu` controller is available.
    pub cpu_max: Option<(u64, u64)>,
}

impl CgroupLimits {
    /// No limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets [`CgroupLimits::memory_max`].
    #[must_use]
    pub fn with_memory_max(mut self, bytes: u64) -> Self {
        self.memory_max = Some(bytes);
        self
    }

    /// Sets [`CgroupLimits::pids_max`].
    #[must_use]
    pub fn with_pids_max(mut self, count: u64) -> Self {
        self.pids_max = Some(count);
        self
    }

    /// Sets [`CgroupLimits::cpu_max`] to `cpus` whole CPUs.
    #[must_use]
    pub fn with_cpus(mut self, cpus: u32) -> Self {
        self.cpu_max = Some((u64::from(cpus) * CPU_PERIOD_US, CPU_PERIOD_US));
        self
    }
}

/// The `cpu.max` period used by [`CgroupLimits::with_cpus`] (the kernel's
/// default): 100 ms.
const CPU_PERIOD_US: u64 = 100_000;

/// What a run's cgroup recorded (read before it was removed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CgroupUsage {
    /// Processes the OOM killer killed because of `memory.max`
    /// (`memory.events` `oom_kill`).
    pub oom_kills: u64,
    /// Times a process or thread could not be created because of
    /// `pids.max` (`pids.events` `max`).
    pub pids_max_hits: u64,
    /// Largest memory use of the run (`memory.peak`, if the kernel has it).
    pub memory_peak: Option<u64>,
    /// Largest number of processes and threads (`pids.peak`, if the kernel
    /// has it).
    pub pids_peak: Option<u64>,
    /// Periods in which the run was throttled by `cpu.max` (`cpu.stat`
    /// `nr_throttled`, where the cpu controller is used).
    pub cpu_throttled: Option<u64>,
}

/// Whether and how a run was placed in a cgroup.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CgroupOutcome {
    /// The [`Spec`](crate::Spec) asked for no cgroup (or the launcher
    /// applies the limits itself).
    #[default]
    NotRequested,
    /// A cgroup was asked for but could not be used (the reason); the run
    /// had the rlimits only.
    Unavailable(String),
    /// The run was in its own cgroup from before the program started.
    Applied(CgroupUsage),
}

/// Where the per-run cgroups are created: a delegated cgroup v2 directory
/// that texrun may manage, or the reason there is none.
///
/// Cheap to clone; clones share the same parent cgroup.
#[derive(Clone)]
pub struct Cgroups {
    state: State,
    required: bool,
}

#[derive(Clone)]
enum State {
    Available(Arc<imp::Parent>),
    Unavailable(String),
}

impl fmt::Debug for Cgroups {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Cgroups");
        match &self.state {
            State::Available(parent) => s.field("parent", &parent.dir()),
            State::Unavailable(reason) => s.field("unavailable", reason),
        };
        s.field("required", &self.required).finish()
    }
}

/// Equal if both use the same parent cgroup (or are unavailable for the
/// same reason) and are equally required.
impl PartialEq for Cgroups {
    fn eq(&self, other: &Self) -> bool {
        self.required == other.required
            && match (&self.state, &other.state) {
                (State::Available(a), State::Available(b)) => a.dir() == b.dir(),
                (State::Unavailable(a), State::Unavailable(b)) => a == b,
                _ => false,
            }
    }
}

impl Eq for Cgroups {}

impl Cgroups {
    /// Finds a cgroup texrun may create run cgroups in, and prepares it.
    ///
    /// Linux with cgroup v2 at `/sys/fs/cgroup` only. Two places are
    /// considered, in this order:
    ///
    /// 1. **this process's own cgroup**, if it was explicitly delegated
    ///    (e.g. `systemd-run --user --scope -p Delegate=yes`): it carries
    ///    systemd's delegation marker (the `trusted.delegate` or
    ///    `user.delegate` xattr), or it is owned by this process's user,
    ///    which is not root. Being writable is not enough (root can write
    ///    to every cgroup, including those systemd manages alone). No
    ///    other process may be in it: the cgroup v2 rule that only a cgroup
    ///    without processes can hand controllers to its children means this
    ///    process first moves itself into a leaf child
    ///    (`texrun-<pid>.main`), as systemd recommends for delegated
    ///    cgroups; the leaf is left behind (empty) when texrun exits;
    /// 2. **the root of this process's cgroup namespace**, if that is not
    ///    the host's root cgroup (i.e. in a container), cgroup v2 is mounted
    ///    with `nsdelegate` (the kernel then treats the namespace as
    ///    delegated to it, as a container runtime does), it is writable and
    ///    has no processes of its own (e.g. a container whose processes
    ///    were moved into a child cgroup).
    ///
    /// A place is used only if `cgroup.kill` exists (Linux 5.14), the
    /// `memory` and `pids` controllers can be enabled for its children
    /// (`cpu` too where possible), and a test cgroup with the limits can be
    /// created and removed; otherwise every change made to it is undone.
    /// Empty run cgroups of texrun processes that are gone (e.g. killed)
    /// are removed from it. Cgroups that are merely writable further up
    /// (e.g. a `systemd --user` slice) are never used: they belong to
    /// another manager. [`Cgroups::at`] uses a given cgroup without these
    /// delegation checks.
    ///
    /// Otherwise the result is [unavailable](Cgroups::check) with the
    /// reason.
    pub fn detect() -> Self {
        Self::from(imp::detect())
    }

    /// Uses `dir`, a cgroup v2 directory, as the parent of the run
    /// cgroups (e.g. one prepared by the caller), after the same usability
    /// checks as [`Cgroups::detect`] but without its delegation checks:
    /// that `dir` may be managed by texrun is the caller's decision. Moving
    /// a child there also needs write access to the common ancestor of its
    /// cgroup and `dir`.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self::from(imp::at(&dir.into()))
    }

    /// Cgroups that cannot be used, because of `reason`.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            state: State::Unavailable(reason.into()),
            required: false,
        }
    }

    /// Whether a run must fail ([`RunError::Unsupported`](crate::RunError::Unsupported),
    /// or [`RunError::Io`](crate::RunError::Io) when creating or entering
    /// the run's cgroup fails) rather than go on with the rlimits only.
    /// Default: `false`.
    #[must_use]
    pub fn with_required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// See [`Cgroups::with_required`].
    pub fn is_required(&self) -> bool {
        self.required
    }

    /// `Ok` if run cgroups can be created, or why not.
    pub fn check(&self) -> Result<(), String> {
        match &self.state {
            State::Available(_) => Ok(()),
            State::Unavailable(reason) => Err(reason.clone()),
        }
    }

    /// The parent directory of the run cgroups, if available.
    pub fn parent(&self) -> Option<&Path> {
        match &self.state {
            State::Available(parent) => Some(parent.dir()),
            State::Unavailable(_) => None,
        }
    }

    pub(crate) fn available(&self) -> Option<&imp::Parent> {
        match &self.state {
            State::Available(parent) => Some(parent),
            State::Unavailable(_) => None,
        }
    }
}

impl From<Result<imp::Parent, String>> for Cgroups {
    fn from(found: Result<imp::Parent, String>) -> Self {
        Self {
            state: match found {
                Ok(parent) => State::Available(Arc::new(parent)),
                Err(reason) => State::Unavailable(reason),
            },
            required: false,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) use linux as imp;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) use other as imp;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) mod linux {
    use std::fs;
    use std::io::{self, Write as _};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::{CgroupLimits, CgroupUsage};

    /// Where cgroup v2 is mounted.
    const MOUNT: &str = "/sys/fs/cgroup";
    /// Controllers the run cgroups need.
    const NEEDED: [&str; 2] = ["memory", "pids"];
    /// How long a killed run cgroup is waited for to become empty.
    const EMPTY_WAIT: Duration = Duration::from_secs(2);

    /// A prepared parent cgroup.
    #[derive(Debug)]
    pub(crate) struct Parent {
        dir: PathBuf,
        cpu: bool,
        next: AtomicU64,
    }

    impl Parent {
        pub(crate) fn dir(&self) -> &Path {
            &self.dir
        }

        /// Creates a run cgroup with `limits`.
        pub(crate) fn create(&self, limits: &CgroupLimits) -> io::Result<Run> {
            let pid = std::process::id();
            let dir = loop {
                let n = self.next.fetch_add(1, Ordering::Relaxed);
                let dir = self.dir.join(format!("texrun-{pid}.{n}"));
                match fs::create_dir(&dir) {
                    Ok(()) => break dir,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            };
            let run = Run { dir };
            // On error `run` is dropped, which removes the directory.
            run.set_limits(limits, self.cpu)?;
            Ok(run)
        }
    }

    /// The cgroup of one run. Removed when dropped (best effort).
    #[derive(Debug)]
    pub(crate) struct Run {
        dir: PathBuf,
    }

    impl Run {
        fn set_limits(&self, limits: &CgroupLimits, cpu: bool) -> io::Result<()> {
            if let Some(bytes) = limits.memory_max {
                write(&self.dir.join("memory.max"), &bytes.to_string())?;
                let swap = self.dir.join("memory.swap.max");
                if swap.exists() {
                    write(&swap, "0")?;
                }
                write(&self.dir.join("memory.oom.group"), "1")?;
            }
            if let Some(count) = limits.pids_max {
                write(&self.dir.join("pids.max"), &count.to_string())?;
            }
            if let (Some((quota, period)), true) = (limits.cpu_max, cpu) {
                write(&self.dir.join("cpu.max"), &format!("{quota} {period}"))?;
            }
            Ok(())
        }

        /// Moves the process `pid` (with all its threads) into this cgroup.
        pub(crate) fn attach(&self, pid: u32) -> io::Result<()> {
            write(&self.dir.join("cgroup.procs"), &pid.to_string())
        }

        /// Kills every process in the cgroup (`cgroup.kill`). Errors are
        /// ignored, like those of `killpg`.
        pub(crate) fn kill(&self) {
            let _ = write(&self.dir.join("cgroup.kill"), "1");
        }

        /// Waits (bounded) until the cgroup is empty, reads its events and
        /// removes it.
        pub(crate) fn finish(self) -> CgroupUsage {
            let deadline = Instant::now() + EMPTY_WAIT;
            while populated(&self.dir) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            CgroupUsage {
                oom_kills: event(&self.dir.join("memory.events"), "oom_kill").unwrap_or(0),
                pids_max_hits: event(&self.dir.join("pids.events"), "max").unwrap_or(0),
                memory_peak: number(&self.dir.join("memory.peak")),
                pids_peak: number(&self.dir.join("pids.peak")),
                cpu_throttled: event(&self.dir.join("cpu.stat"), "nr_throttled"),
            }
            // Dropping `self` removes the directory.
        }
    }

    impl Drop for Run {
        fn drop(&mut self) {
            // Fails while processes are still inside (e.g. stuck in the
            // kernel); the empty directory is then left behind.
            let _ = fs::remove_dir(&self.dir);
        }
    }

    fn populated(dir: &Path) -> bool {
        event(&dir.join("cgroup.events"), "populated").is_none_or(|n| n != 0)
    }

    fn write(path: &Path, value: &str) -> io::Result<()> {
        // One `write(2)`: cgroup files take a value per write.
        let mut file = fs::OpenOptions::new().write(true).open(path)?;
        let n = file.write(value.as_bytes())?;
        if n == value.len() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "short write to {}",
                path.display()
            )))
        }
    }

    /// The value of `key` in a flat keyed file (`key value` lines).
    fn event(path: &Path, key: &str) -> Option<u64> {
        let text = fs::read_to_string(path).ok()?;
        text.lines().find_map(|line| {
            let (k, v) = line.split_once(' ')?;
            (k == key).then(|| v.trim().parse().ok()).flatten()
        })
    }

    fn number(path: &Path) -> Option<u64> {
        fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    fn words(path: &Path) -> io::Result<Vec<String>> {
        Ok(fs::read_to_string(path)?
            .split_whitespace()
            .map(str::to_owned)
            .collect())
    }

    fn writable(path: &Path) -> bool {
        rustix::fs::access(path, rustix::fs::Access::WRITE_OK).is_ok()
    }

    /// This process's cgroup, relative to the mount (the namespace root).
    fn own_cgroup() -> Result<PathBuf, String> {
        let text = fs::read_to_string("/proc/self/cgroup")
            .map_err(|e| format!("cannot read /proc/self/cgroup: {e}"))?;
        let rel = text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| "this process is not in a cgroup v2 hierarchy".to_owned())?;
        Ok(PathBuf::from(rel.trim_start_matches('/')))
    }

    pub(crate) fn detect() -> Result<Parent, String> {
        let mount = Path::new(MOUNT);
        if !mount.join("cgroup.controllers").is_file() {
            return Err(format!("cgroup v2 is not mounted at {MOUNT}"));
        }
        let own = mount.join(own_cgroup()?);
        let own_reason = match delegated(&own).and_then(|()| prepare(&own, true)) {
            Ok(parent) => return Ok(parent),
            Err(reason) => reason,
        };
        if own != mount {
            return namespace_root(mount)
                .and_then(|()| prepare(mount, false))
                .map_err(|root_reason| {
                    format!(
                        "no delegated cgroup: own cgroup: {own_reason}; namespace root: \
                         {root_reason}"
                    )
                });
        }
        Err(format!("no delegated cgroup: {own_reason}"))
    }

    pub(crate) fn at(dir: &Path) -> Result<Parent, String> {
        let own = Path::new(MOUNT).join(own_cgroup()?);
        let is_own = fs::canonicalize(dir).ok() == fs::canonicalize(&own).ok();
        prepare(dir, is_own)
    }

    /// `Ok` if `dir` was explicitly delegated to this process: it carries
    /// systemd's delegation marker (the `trusted.delegate` or
    /// `user.delegate` xattr, set for `Delegate=yes`), or it is owned by
    /// this process's user, which is not root (a cgroup delegated to a
    /// user is chowned to them). Being writable is not enough: root can
    /// write to every cgroup, including those another manager owns.
    fn delegated(dir: &Path) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;

        let markers: Vec<Option<Vec<u8>>> = ["trusted.delegate", "user.delegate"]
            .iter()
            .map(|name| {
                let mut buf = [0u8; 16];
                rustix::fs::getxattr(dir, *name, &mut buf[..])
                    .ok()
                    .map(|n| buf[..n].to_vec())
            })
            .collect();
        let owners: Vec<Option<u32>> = ["", "cgroup.procs", "cgroup.subtree_control"]
            .iter()
            .map(|name| fs::metadata(dir.join(name)).ok().map(|m| m.uid()))
            .collect();
        let euid = rustix::process::geteuid().as_raw();
        if has_delegation_evidence(&markers, &owners, euid) {
            Ok(())
        } else {
            Err(format!(
                "{} is not delegated to texrun (no delegation marker, and not owned by this \
                 non-root user)",
                dir.display()
            ))
        }
    }

    /// See [`delegated`]: a marker `1`, or every file owned by `euid` ≠ 0.
    pub(super) fn has_delegation_evidence(
        markers: &[Option<Vec<u8>>],
        owners: &[Option<u32>],
        euid: u32,
    ) -> bool {
        let marked = markers.iter().flatten().any(|m| m.as_slice() == b"1");
        let owned = euid != 0 && !owners.is_empty() && owners.iter().all(|o| *o == Some(euid));
        marked || owned
    }

    /// `Ok` if `mount`, the root of this process's cgroup namespace, is a
    /// delegation boundary: not the host's root cgroup (which has no
    /// `cgroup.type`; in the host's namespace the mount is that root), and
    /// mounted with `nsdelegate`, under which the kernel treats a cgroup
    /// namespace as delegated (e.g. a container's, handed to it by the
    /// runtime).
    fn namespace_root(mount: &Path) -> Result<(), String> {
        if !mount.join("cgroup.type").is_file() {
            return Err(format!("{} is the host's root cgroup", mount.display()));
        }
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")
            .map_err(|e| format!("cannot read /proc/self/mountinfo: {e}"))?;
        if mounted_with_nsdelegate(&mountinfo, MOUNT) {
            Ok(())
        } else {
            Err(format!("{MOUNT} is not mounted with nsdelegate"))
        }
    }

    /// Whether the cgroup2 mount at `point` in `mountinfo` has the
    /// `nsdelegate` super option.
    pub(super) fn mounted_with_nsdelegate(mountinfo: &str, point: &str) -> bool {
        mountinfo.lines().any(|line| {
            let Some((mount, fs)) = line.split_once(" - ") else {
                return false;
            };
            let mut fs = fs.split(' ');
            mount.split(' ').nth(4) == Some(point)
                && fs.next() == Some("cgroup2")
                && fs
                    .nth(1)
                    .is_some_and(|opts| opts.split(',').any(|o| o == "nsdelegate"))
        })
    }

    /// Makes `dir` usable as the parent of run cgroups. `is_own`: this
    /// process is in `dir` and may move itself into a leaf. Every change
    /// is undone if `dir` turns out to be unusable.
    fn prepare(dir: &Path, is_own: bool) -> Result<Parent, String> {
        let shown = dir.display();
        if !dir.join("cgroup.type").is_file() {
            return Err(format!("{shown} is not a non-root cgroup v2 directory"));
        }
        if !dir.join("cgroup.kill").is_file() {
            return Err("cgroup.kill is missing (Linux 5.14 or later is needed)".to_owned());
        }
        for name in ["", "cgroup.procs", "cgroup.subtree_control"] {
            if !writable(&dir.join(name)) {
                return Err(format!("{shown} is not writable"));
            }
        }
        let available = words(&dir.join("cgroup.controllers"))
            .map_err(|e| format!("cannot read the controllers of {shown}: {e}"))?;
        if let Some(missing) = NEEDED.iter().find(|c| !available.iter().any(|a| a == *c)) {
            return Err(format!(
                "the {missing} controller is not available in {shown}"
            ));
        }
        let enabled = words(&dir.join("cgroup.subtree_control")).unwrap_or_default();
        let is_enabled = |c: &str| enabled.iter().any(|e| e == c);
        let wanted: Vec<&str> = NEEDED.iter().copied().filter(|c| !is_enabled(c)).collect();
        let wants_cpu = available.iter().any(|a| a == "cpu") && !is_enabled("cpu");
        let mut undo = Undo {
            dir,
            leaf: None,
            disable: Vec::new(),
        };
        if !wanted.is_empty() || wants_cpu {
            let procs = words(&dir.join("cgroup.procs"))
                .map_err(|e| format!("cannot read the processes of {shown}: {e}"))?;
            let me = std::process::id().to_string();
            if is_own && procs == [me] {
                undo.leaf = Some(move_into_leaf(dir)?);
            } else if !procs.is_empty() {
                return Err(format!(
                    "{shown} has other processes, so its controllers cannot be enabled for \
                     child cgroups"
                ));
            }
        }
        if !wanted.is_empty() {
            let request: Vec<String> = wanted.iter().map(|c| format!("+{c}")).collect();
            write(&dir.join("cgroup.subtree_control"), &request.join(" ")).map_err(|e| {
                format!(
                    "cannot enable the {} controllers in {shown}: {e}",
                    wanted.join(", ")
                )
            })?;
            undo.disable.extend(wanted.iter().copied());
        }
        // Best effort, apart from memory and pids: the cpu controller cannot
        // always be enabled (e.g. with realtime processes around).
        let cpu = is_enabled("cpu")
            || (wants_cpu && write(&dir.join("cgroup.subtree_control"), "+cpu").is_ok());
        if wants_cpu && cpu {
            undo.disable.push("cpu");
        }
        let parent = Parent {
            dir: dir.to_owned(),
            cpu,
            next: AtomicU64::new(0),
        };
        probe(&parent)?;
        remove_stale(dir);
        undo.disable.clear();
        undo.leaf = None;
        Ok(parent)
    }

    /// Changes [`prepare`] made to a cgroup, undone when dropped unless
    /// cleared.
    struct Undo<'a> {
        dir: &'a Path,
        /// The leaf this process moved into.
        leaf: Option<PathBuf>,
        /// Controllers enabled for the children.
        disable: Vec<&'a str>,
    }

    impl Drop for Undo<'_> {
        fn drop(&mut self) {
            if !self.disable.is_empty() {
                let request: Vec<String> = self.disable.iter().map(|c| format!("-{c}")).collect();
                let _ = write(&self.dir.join("cgroup.subtree_control"), &request.join(" "));
            }
            if let Some(leaf) = self.leaf.take() {
                let _ = write(
                    &self.dir.join("cgroup.procs"),
                    &std::process::id().to_string(),
                );
                let _ = fs::remove_dir(leaf);
            }
        }
    }

    /// Removes run cgroups left behind by texrun processes that are gone
    /// (e.g. killed): only empty ones, since `rmdir` fails for a cgroup
    /// with processes or children. Nothing is killed.
    fn remove_stale(dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|n| n.strip_prefix("texrun-"))
                .and_then(|n| n.split_once('.'))
                .and_then(|(pid, _)| pid.parse::<u32>().ok())
            else {
                continue;
            };
            if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
                let _ = fs::remove_dir(entry.path());
            }
        }
    }

    /// Moves this process into a new leaf child of `dir` (its cgroup).
    fn move_into_leaf(dir: &Path) -> Result<PathBuf, String> {
        let leaf = dir.join(format!("texrun-{}.main", std::process::id()));
        fs::create_dir(&leaf)
            .and_then(|()| write(&leaf.join("cgroup.procs"), &std::process::id().to_string()))
            .map_err(|e| {
                let _ = fs::remove_dir(&leaf);
                format!("cannot move texrun into {}: {e}", leaf.display())
            })?;
        Ok(leaf)
    }

    /// Creates and removes a run cgroup with every limit, to find out now
    /// rather than on the first run.
    fn probe(parent: &Parent) -> Result<(), String> {
        let limits = CgroupLimits::new()
            .with_memory_max(1 << 30)
            .with_pids_max(64)
            .with_cpus(1);
        parent
            .create(&limits)
            .map(drop)
            .map_err(|e| format!("cannot create a cgroup in {}: {e}", parent.dir.display()))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) mod other {
    use std::io;
    use std::path::Path;

    use super::{CgroupLimits, CgroupUsage};

    const REASON: &str = "cgroups exist on Linux only";

    /// Never constructed: cgroups are Linux only.
    #[derive(Debug)]
    pub(crate) enum Parent {}

    impl Parent {
        pub(crate) fn dir(&self) -> &Path {
            match *self {}
        }

        pub(crate) fn create(&self, _limits: &CgroupLimits) -> io::Result<Run> {
            match *self {}
        }
    }

    /// Never constructed.
    #[derive(Debug)]
    pub(crate) enum Run {}

    impl Run {
        pub(crate) fn attach(&self, _pid: u32) -> io::Result<()> {
            match *self {}
        }

        pub(crate) fn kill(&self) {
            match *self {}
        }

        pub(crate) fn finish(self) -> CgroupUsage {
            match self {}
        }
    }

    pub(crate) fn detect() -> Result<Parent, String> {
        Err(REASON.to_owned())
    }

    pub(crate) fn at(_dir: &Path) -> Result<Parent, String> {
        Err(REASON.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_builder() {
        let limits = CgroupLimits::new()
            .with_memory_max(10)
            .with_pids_max(3)
            .with_cpus(2);
        assert_eq!(limits.memory_max, Some(10));
        assert_eq!(limits.pids_max, Some(3));
        assert_eq!(limits.cpu_max, Some((200_000, 100_000)));
    }

    #[test]
    fn unavailable_cgroups_say_why() {
        let cgroups = Cgroups::unavailable("no delegation");
        assert_eq!(cgroups.check(), Err("no delegation".to_owned()));
        assert!(cgroups.parent().is_none());
        assert!(!cgroups.is_required());
        assert!(cgroups.with_required(true).is_required());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn delegation_needs_a_marker_or_a_non_root_owner() {
        use super::linux::has_delegation_evidence as evidence;
        let one = Some(b"1".to_vec());
        let owned = [Some(1000), Some(1000), Some(1000)];
        let root = [Some(0), Some(0), Some(0)];
        // Writable to root, but no marker: not delegated.
        assert!(!evidence(&[None, None], &root, 0));
        assert!(evidence(&[one.clone(), None], &root, 0));
        assert!(evidence(&[None, one], &root, 0));
        assert!(!evidence(&[Some(b"0".to_vec()), None], &root, 0));
        // Chowned to a non-root user.
        assert!(evidence(&[None, None], &owned, 1000));
        assert!(!evidence(&[None, None], &owned, 1001));
        assert!(!evidence(
            &[None, None],
            &[Some(1000), Some(0), Some(1000)],
            1000
        ));
        assert!(!evidence(
            &[None, None],
            &[Some(1000), None, Some(1000)],
            1000
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn nsdelegate_is_read_from_mountinfo() {
        use super::linux::mounted_with_nsdelegate as nsdelegate;
        let line = |opts: &str| {
            format!("35 30 0:30 / /sys/fs/cgroup ro,nosuid shared:9 - cgroup2 cgroup {opts}\n")
        };
        assert!(nsdelegate(
            &line("rw,nsdelegate,memory_recursiveprot"),
            "/sys/fs/cgroup"
        ));
        assert!(!nsdelegate(
            &line("rw,memory_recursiveprot"),
            "/sys/fs/cgroup"
        ));
        assert!(!nsdelegate(&line("rw,nsdelegate"), "/mnt"));
        let other_fs = "35 30 0:30 / /sys/fs/cgroup rw - tmpfs tmpfs rw,nsdelegate\n";
        assert!(!nsdelegate(other_fs, "/sys/fs/cgroup"));
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    #[test]
    fn cgroups_are_linux_only() {
        assert!(Cgroups::detect().check().unwrap_err().contains("Linux"));
        assert!(Cgroups::at("/sys/fs/cgroup").check().is_err());
    }
}
