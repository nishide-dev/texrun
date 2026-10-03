//! Who created a container or a scratch directory, so that the ones a
//! killed texrun left behind can be reclaimed by a later one (#49, #56,
//! docs/security.md §4 "container のライフサイクル").
//!
//! A [`Creator`] names a texrun process by its PID, the start time of that
//! process (where the kernel tells it without `unsafe`: Linux) and the
//! "host" its PID belongs to (a hash of what makes PIDs comparable: the
//! boot and the PID namespace on Linux, the host name elsewhere). Where
//! they can be told, it also names the machine and the boot (#56).
//!
//! A later texrun considers the creator gone ([`Creator::is_gone`]) only
//! if it knows that the creator's PID is comparable with its own and no
//! process has that PID any more, or one with another start time has (the
//! PID was reused); or if it knows that the creator ran on the same
//! machine in an earlier boot. Whenever it cannot tell, the creator counts
//! as alive: a leftover is kept rather than something in use removed.
//!
//! # The machine and the boot (#56)
//!
//! The host changes with every boot on Linux (and with the host name on
//! macOS), so what is left before it changed would never be reclaimed by
//! the host alone. The machine is recorded only together with the boot,
//! and only where all processes of the machine (in one boot) share one
//! PID space, which a later texrun on the same machine can rely on:
//!
//! - Linux: a hash of `/etc/machine-id`, only for a texrun in the initial
//!   PID namespace and not in a container (`/.dockerenv`,
//!   `/run/.containerenv`): a container's `machine-id` is often its
//!   image's, shared by every container of that image. The boot is a hash
//!   of `/proc/sys/kernel/random/boot_id` (read once, also for the host).
//!   The same machine and boot always have the same host here, so another
//!   host with them is unknown.
//! - macOS: a hash of the `IOPlatformUUID`, which does not change with the
//!   host name, and of `kern.bootsessionuuid` (from `/usr/sbin/ioreg` and
//!   `/usr/sbin/sysctl`, run by path without a shell, with an empty
//!   environment, a timeout and a bounded output). There are no PID
//!   namespaces, so a creator of the same machine and boot has a
//!   comparable PID, whatever its host name. If neither side knows the
//!   boot, the PID is compared as before #56 (a PID in use, maybe reused
//!   after a reboot, keeps what it left).
//! - Elsewhere, or if these cannot be read: nothing, and only the host is
//!   compared.
//!
//! A creator of the same machine and another boot is gone if, in
//! addition, what it left (a container, a scratch directory) was created
//! or last changed before this boot started (`btime` in `/proc/stat`,
//! `kern.boottime`). This excludes what was created after this boot, e.g.
//! by another live kernel that happens to share the machine identifier (a
//! cloned VM) and the runtime; it assumes that the runtime's clock (for a
//! container's creation time) agrees with this kernel's. A leftover of such
//! another kernel created before this boot is not told apart.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use rustix::io::Errno;
use rustix::process::Pid;

/// A texrun process that created a container or a scratch directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Creator {
    pid: u32,
    started: Option<u64>,
    host: u64,
    machine: Option<u64>,
    boot: Option<u64>,
}

/// How a recorded creator relates to this texrun process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Relation {
    /// Its PID is comparable with this process's.
    SamePidSpace,
    /// It ran on this machine in another boot.
    OtherBoot,
    /// Cannot be told: what it left is kept.
    Unknown,
}

/// What this process knows about itself.
#[derive(Debug)]
struct Local {
    me: Creator,
    /// When this boot started (Linux, macOS).
    booted: Option<SystemTime>,
}

impl Creator {
    /// This texrun process; `None` if the host cannot be identified (then
    /// nothing it creates is ever reclaimed).
    pub fn current() -> Option<Self> {
        local().map(|local| local.me)
    }

    /// A creator from its parts (as recorded in labels), without a machine
    /// or boot (as recorded before #56).
    pub fn new(pid: u32, started: Option<u64>, host: u64) -> Self {
        Self {
            pid,
            started,
            host,
            machine: None,
            boot: None,
        }
    }

    /// This creator with the machine and the boot it ran on (as recorded).
    #[must_use]
    pub fn with_machine(mut self, machine: Option<u64>, boot: Option<u64>) -> Self {
        self.machine = machine;
        self.boot = boot;
        self
    }

    /// The PID.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The start time of the process (Linux: clock ticks since boot, from
    /// `/proc/<pid>/stat`); `None` where it is not known.
    pub fn started(&self) -> Option<u64> {
        self.started
    }

    /// The hash of the host the PID belongs to.
    pub fn host(&self) -> u64 {
        self.host
    }

    /// The hash of the machine (see the module documentation for where it
    /// is recorded); `None` if not known.
    pub fn machine(&self) -> Option<u64> {
        self.machine
    }

    /// The hash of the boot (Linux); `None` if not known.
    pub fn boot(&self) -> Option<u64> {
        self.boot
    }

    /// A short text form for names:
    /// `<pid>-<started or x>-<host>-<machine or x>-<boot or x>` (decimal,
    /// decimal, then 16 hex digits each; only `[0-9a-fx-]`).
    pub fn tag(&self) -> String {
        let started = self
            .started
            .map_or_else(|| "x".to_owned(), |s| s.to_string());
        let id = |id: Option<u64>| id.map_or_else(|| "x".to_owned(), |id| format!("{id:016x}"));
        format!(
            "{}-{started}-{:016x}-{}-{}",
            self.pid,
            self.host,
            id(self.machine),
            id(self.boot)
        )
    }

    /// Parses [`Creator::tag`] at the start of `text`; returns the creator
    /// and the rest of `text` after the tag. A tag of #49 (`<pid>-<started
    /// or x>-<host>`, without the machine and the boot) is parsed when what
    /// follows the host is a single part (no `-`): the creator then has no
    /// machine or boot.
    pub fn parse_tag(text: &str) -> Option<(Self, &str)> {
        let mut parts = text.splitn(4, '-');
        let pid = parse_decimal(parts.next()?)?;
        let started = match parts.next()? {
            "x" => None,
            s => Some(parse_decimal(s)?),
        };
        let host = parse_hex16(parts.next()?)?;
        let creator = Self::new(u32::try_from(pid).ok()?, started, host);
        let rest = parts.next().unwrap_or("");
        let mut more = rest.splitn(3, '-');
        if let (Some(machine), Some(boot), Some(after)) = (more.next(), more.next(), more.next()) {
            let optional = |part: &str| match part {
                "x" => Some(None),
                part => parse_hex16(part).map(Some),
            };
            let (machine, boot) = (optional(machine)?, optional(boot)?);
            return Some((creator.with_machine(machine, boot), after));
        }
        Some((creator, rest))
    }

    /// Whether this creator is known to be gone (see the module
    /// documentation): its PID is comparable with this process's and no
    /// process has it any more (or one with another start time); or it
    /// ran on this machine in another boot and what it left was `created`
    /// (or last changed) before this boot started. `false` whenever that
    /// cannot be told.
    pub fn is_gone(&self, created: Option<SystemTime>) -> bool {
        let Some(local) = local() else {
            return false;
        };
        match relation(self, &local.me) {
            Relation::SamePidSpace => self.pid != local.me.pid && self.pid_is_gone(),
            Relation::OtherBoot => before_boot(created, local.booted),
            Relation::Unknown => false,
        }
    }

    /// No process has the PID any more, or one with another start time.
    fn pid_is_gone(&self) -> bool {
        let Some(pid) = i32::try_from(self.pid).ok().and_then(Pid::from_raw) else {
            return false;
        };
        match rustix::process::test_kill_process(pid) {
            Err(Errno::SRCH) => true,
            // Some process has the PID (`EPERM`: another user's).
            Ok(()) | Err(Errno::PERM) => match (self.started, start_time(self.pid)) {
                (Some(recorded), Some(now)) => recorded != now,
                _ => false,
            },
            Err(_) => false,
        }
    }
}

/// Whether all processes of a machine in one boot share one PID space
/// whatever their host (name): macOS, which has no PID namespaces. On
/// Linux the host already names the PID namespace.
const ONE_PID_SPACE_PER_BOOT: bool = cfg!(target_os = "macos");

/// How the recorded creator `other` relates to `me` on this OS.
pub(crate) fn relation(other: &Creator, me: &Creator) -> Relation {
    relation_in(other, me, ONE_PID_SPACE_PER_BOOT)
}

/// How the recorded creator `other` relates to `me`, where
/// `one_pid_space_per_boot` tells whether a machine's processes in one
/// boot share one PID space whatever their host (macOS).
///
/// - The same host: the same PID space.
/// - The same machine, both with a boot: another boot if the boots differ;
///   with the same boot, the same PID space only if
///   `one_pid_space_per_boot` (macOS: the host name changed). On Linux the
///   same machine and boot always have the same host (a machine is only
///   recorded in the initial PID namespace), so another host with them is
///   unknown (forged labels, colliding hashes).
/// - Anything else: unknown. That includes another machine and a machine
///   or boot missing on either side (a machine is only recorded with a
///   boot).
fn relation_in(other: &Creator, me: &Creator, one_pid_space_per_boot: bool) -> Relation {
    if other.host == me.host {
        return Relation::SamePidSpace;
    }
    match (other.machine, me.machine, other.boot, me.boot) {
        (Some(a), Some(b), Some(c), Some(d)) if a == b => {
            if c != d {
                Relation::OtherBoot
            } else if one_pid_space_per_boot {
                Relation::SamePidSpace
            } else {
                Relation::Unknown
            }
        }
        _ => Relation::Unknown,
    }
}

/// Whether `created` is known to be before `booted`.
pub(crate) fn before_boot(created: Option<SystemTime>, booted: Option<SystemTime>) -> bool {
    matches!((created, booted), (Some(created), Some(booted)) if created < booted)
}

fn local() -> Option<&'static Local> {
    static LOCAL: OnceLock<Option<Local>> = OnceLock::new();
    LOCAL
        .get_or_init(|| {
            let pid = std::process::id();
            let identity = identity()?;
            // A machine is only recorded together with a boot.
            let machine = identity.boot.and(identity.machine);
            Some(Local {
                me: Creator::new(pid, start_time(pid), identity.host)
                    .with_machine(machine, identity.boot),
                booted: identity.booted,
            })
        })
        .as_ref()
}

fn parse_decimal(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Exactly 16 hex digits.
pub(crate) fn parse_hex16(text: &str) -> Option<u64> {
    if text.len() != 16 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(text, 16).ok()
}

/// The start time of process `pid` in clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`). Linux only.
fn start_time(pid: u32) -> Option<u64> {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) is in parentheses and may contain spaces
    // and parentheses itself: count from the last `)`.
    let (_, rest) = stat.rsplit_once(')')?;
    // `rest` starts with field 3 (the state).
    rest.split_whitespace().nth(22 - 3)?.parse().ok()
}

/// What this process is running on, hashed.
struct Identity {
    /// What makes PIDs of this process comparable with recorded ones:
    ///
    /// - Linux: the boot (`/proc/sys/kernel/random/boot_id`) and the PID
    ///   namespace (`/proc/self/ns/pid`): a texrun in another container
    ///   sees other PIDs, even with the same runtime socket.
    /// - Elsewhere (macOS: no PID namespaces): the OS and the host name.
    ///   The runtime is always a local one (a `unix://` socket).
    host: u64,
    /// The machine (see the module documentation).
    machine: Option<u64>,
    /// The boot.
    boot: Option<u64>,
    /// When the boot started.
    booted: Option<SystemTime>,
}

/// The inode of the initial PID namespace (`PROC_PID_INIT_INO`), the same
/// in every boot.
const INITIAL_PID_NAMESPACE: &str = "pid:[4026531836]";

/// The identity of this process; `None` if the host cannot be identified.
fn identity() -> Option<Identity> {
    if cfg!(any(target_os = "linux", target_os = "android")) {
        // Read once: the host and the boot are of the same boot.
        let boot_id = read_small("/proc/sys/kernel/random/boot_id")?;
        let boot_id = boot_id.trim();
        if boot_id.is_empty() {
            return None;
        }
        let ns = std::fs::read_link("/proc/self/ns/pid").ok()?;
        let in_container = ["/.dockerenv", "/run/.containerenv"]
            .iter()
            .any(|marker| std::fs::symlink_metadata(marker).is_ok());
        let machine = (ns.as_os_str() == INITIAL_PID_NAMESPACE && !in_container)
            .then(|| read_small("/etc/machine-id"))
            .flatten()
            .and_then(|text| {
                linux_machine_id(&text).map(|id| fnv1a(format!("linux-machine\n{id}").as_bytes()))
            });
        Some(Identity {
            host: fnv1a(format!("linux\n{boot_id}\n{}", ns.display()).as_bytes()),
            machine,
            boot: Some(fnv1a(format!("linux-boot\n{boot_id}").as_bytes())),
            booted: linux_boot_time(),
        })
    } else {
        let uname = rustix::system::uname();
        let node = uname.nodename().to_string_lossy();
        if node.is_empty() {
            return None;
        }
        let host = fnv1a(format!("{}\n{node}", std::env::consts::OS).as_bytes());
        let (mut machine, mut boot, mut booted) = (None, None, None);
        if cfg!(target_os = "macos") {
            machine = run_small("/usr/sbin/ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])
                .and_then(|out| platform_uuid(&out))
                .map(|uuid| fnv1a(format!("macos-machine\n{uuid}").as_bytes()));
            if let Some((session, started)) = run_small(
                "/usr/sbin/sysctl",
                &["-n", "kern.bootsessionuuid", "kern.boottime"],
            )
            .and_then(|out| macos_boot(&out))
            {
                boot = Some(fnv1a(format!("macos-boot\n{session}").as_bytes()));
                booted = Some(started);
            }
        }
        Some(Identity {
            host,
            machine,
            boot,
            booted,
        })
    }
}

/// A valid `machine-id`: 32 lowercase hex digits (and a newline), not all
/// zero (`uninitialized` and the empty file of an image are not).
fn linux_machine_id(text: &str) -> Option<&str> {
    let id = text.strip_suffix('\n').unwrap_or(text);
    let valid = id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && id.bytes().any(|b| b != b'0');
    valid.then_some(id)
}

/// The start of this boot (Linux: `btime` in `/proc/stat`, seconds).
fn linux_boot_time() -> Option<SystemTime> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let seconds: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// The start of a small text file (a `machine-id`, a `boot_id`).
fn read_small(path: &str) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(256)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// The stdout of a system tool (`ioreg`, `sysctl`): started by path, with
/// no shell, an empty environment, a timeout and a bounded output; `None`
/// if it is missing, fails, takes too long or writes too much.
fn run_small(program: &str, args: &[&str]) -> Option<String> {
    use texrun_process::{Capture, Cwd, Spec, Watch};

    if !Path::new(program).is_file() {
        return None;
    }
    let spec = Spec::new(program, Cwd::Path(Path::new("/")))
        .with_args(args.iter().copied())
        .with_stdout(Capture::Keep(64 * 1024))
        .with_stderr(Capture::Discard);
    let finished = texrun_process::run(
        &spec,
        Watch::<()>::new().with_timeout(Duration::from_secs(5)),
    )
    .ok()?;
    if finished.stop.is_some() || !finished.status.success() || finished.stdout.is_truncated() {
        return None;
    }
    String::from_utf8(finished.stdout.bytes).ok()
}

/// The value of `"IOPlatformUUID" = "<uuid>"` in `ioreg` output, if it is a
/// UUID.
fn platform_uuid(ioreg: &str) -> Option<String> {
    let uuid = ioreg.lines().find_map(|line| {
        line.trim()
            .strip_prefix("\"IOPlatformUUID\" = \"")?
            .strip_suffix('"')
    })?;
    valid_uuid(uuid)
}

/// `uuid` in upper case if it is a UUID (hex digits and dashes,
/// 8-4-4-4-12, not all zero).
fn valid_uuid(uuid: &str) -> Option<String> {
    let groups: Vec<&str> = uuid.split('-').collect();
    let shape = groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12]);
    let hex = groups
        .iter()
        .all(|g| g.bytes().all(|b| b.is_ascii_hexdigit()));
    let zero = uuid.bytes().all(|b| b == b'0' || b == b'-');
    (shape && hex && !zero).then(|| uuid.to_ascii_uppercase())
}

/// The boot session and its start from `sysctl -n kern.bootsessionuuid
/// kern.boottime`: a UUID line, then `{ sec = <s>, usec = <us> } <date>`.
fn macos_boot(sysctl: &str) -> Option<(String, SystemTime)> {
    let mut lines = sysctl.lines();
    let session = valid_uuid(lines.next()?.trim())?;
    let boottime = lines.next()?.trim().strip_prefix("{ sec = ")?;
    let (seconds, rest) = boottime.split_once(", usec = ")?;
    let (micros, _) = rest.split_once(" }")?;
    let seconds = parse_decimal(seconds)?;
    let micros = u32::try_from(parse_decimal(micros)?).ok()?;
    if micros >= 1_000_000 || lines.next().is_some() {
        return None;
    }
    let started = SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, micros * 1000))?;
    Some((session, started))
}

/// 64-bit FNV-1a: stable across texrun versions and Rust releases (unlike
/// the standard library's hashers).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip() {
        let c = Creator::new(1234, Some(98765), 0x0123_4567_89ab_cdef);
        assert_eq!(c.tag(), "1234-98765-0123456789abcdef-x-x");
        assert_eq!(
            Creator::parse_tag(&format!("{}-r", c.tag())),
            Some((c, "r"))
        );
        let c = c.with_machine(Some(0xfe), Some(0x0123_4567_89ab_cdef));
        assert_eq!(
            c.tag(),
            "1234-98765-0123456789abcdef-00000000000000fe-0123456789abcdef"
        );
        assert_eq!(
            Creator::parse_tag(&format!("{}-rest-x", c.tag())),
            Some((c, "rest-x"))
        );
        let m = Creator::new(7, None, 1).with_machine(Some(2), None);
        assert_eq!(
            Creator::parse_tag(&format!("{}-r", m.tag())),
            Some((m, "r"))
        );
        // The tag of #49: no machine or boot.
        let old = Creator::new(1234, Some(98765), 0x0123_4567_89ab_cdef);
        assert_eq!(
            Creator::parse_tag("1234-98765-0123456789abcdef-0011223344556677"),
            Some((old, "0011223344556677"))
        );
        let c = Creator::new(7, None, 1);
        assert_eq!(Creator::parse_tag("7-x-0000000000000001"), Some((c, "")));
        for bad in [
            "",
            "x-1-0123456789abcdef",
            "1-1-0123",
            "1-1-0123456789abcdeg",
            "-1-1-0123456789abcdef",
            "99999999999-1-0123456789abcdef",
            "1--0123456789abcdef",
            // A malformed machine or boot.
            "1-1-0123456789abcdef-0123-x-r",
            "1-1-0123456789abcdef-x-y-r",
            "1-1-0123456789abcdef--x-r",
        ] {
            assert_eq!(Creator::parse_tag(bad), None, "{bad}");
        }
    }

    #[test]
    fn this_process_is_not_gone() {
        let me = Creator::current().expect("the host can be identified");
        assert_eq!(me.pid(), std::process::id());
        assert!(!me.is_gone(None));
        assert!(!me.is_gone(Some(SystemTime::UNIX_EPOCH)));
        if cfg!(target_os = "linux") {
            assert!(me.started().is_some());
            assert!(me.boot().is_some());
        }
        if cfg!(target_os = "macos") {
            assert!(me.machine().is_some(), "the IOPlatformUUID is read");
            assert!(me.boot().is_some(), "kern.bootsessionuuid is read");
            let booted = local().unwrap().booted.expect("kern.boottime is read");
            assert!(booted < SystemTime::now());
        }
        // A machine only with a boot.
        assert!(me.machine().is_none() || me.boot().is_some());
    }

    #[test]
    fn an_exited_process_is_gone_only_on_the_same_host() {
        let me = Creator::current().unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let gone = Creator::new(pid, None, me.host());
        assert!(gone.is_gone(None));
        // Another host: never.
        assert!(!Creator::new(pid, None, me.host() ^ 1).is_gone(None));
        // Another host of the same machine and boot: the PID is compared on
        // macOS (a renamed host); unknown on Linux.
        let renamed = Creator::new(pid, None, me.host() ^ 1).with_machine(me.machine(), me.boot());
        assert_eq!(
            renamed.is_gone(None),
            cfg!(target_os = "macos") && me.machine().is_some()
        );
        // Without the boot: unknown everywhere.
        let no_boot = Creator::new(pid, None, me.host() ^ 1).with_machine(me.machine(), None);
        assert!(!no_boot.is_gone(None));
        // Another host of another machine.
        let other = Creator::new(pid, None, me.host() ^ 1)
            .with_machine(me.machine().map(|m| m ^ 1), me.boot());
        assert!(!other.is_gone(None));
    }

    #[test]
    fn a_reused_pid_is_told_apart_by_its_start_time() {
        let me = Creator::current().unwrap();
        // The parent of the test (alive): the same PID with another start
        // time is a reused PID where start times are known, and alive
        // elsewhere.
        let parent = rustix::process::getppid().map_or(1, |p| p.as_raw_nonzero().get());
        let parent = u32::try_from(parent).unwrap();
        let reused = Creator::new(parent, Some(u64::MAX), me.host());
        assert_eq!(reused.is_gone(None), cfg!(target_os = "linux"));
        let unknown = Creator::new(parent, None, me.host());
        assert!(!unknown.is_gone(None));
        if let Some(started) = start_time(parent) {
            assert!(!Creator::new(parent, Some(started), me.host()).is_gone(None));
        }
    }

    /// Every combination of what is recorded and what is known here, on an
    /// OS where a machine's boot is one PID space whatever the host name
    /// (macOS) and where it is not (Linux).
    #[test]
    fn creators_are_related_by_host_machine_and_boot() {
        use Relation::{OtherBoot, SamePidSpace, Unknown};
        let me = Creator::new(1, Some(1), 0xa).with_machine(Some(0xc0), Some(0xb0));
        let other = |host, machine, boot| Creator::new(2, None, host).with_machine(machine, boot);
        // (recorded, on Linux, on macOS)
        let cases = [
            // The same host: whatever else is recorded (#49 records none).
            (other(0xa, None, None), SamePidSpace, SamePidSpace),
            (other(0xa, Some(0xf), Some(0xf)), SamePidSpace, SamePidSpace),
            // Same machine, another boot: the creator's boot is over.
            (other(0xb, Some(0xc0), Some(0xb1)), OtherBoot, OtherBoot),
            // Same machine and boot, another host: a renamed Mac; cannot
            // happen on Linux (initial PID namespace), so kept there.
            (other(0xb, Some(0xc0), Some(0xb0)), Unknown, SamePidSpace),
            // Another machine.
            (other(0xb, Some(0xc1), Some(0xb1)), Unknown, Unknown),
            (other(0xb, Some(0xc1), Some(0xb0)), Unknown, Unknown),
            // A machine or boot missing on the recorded side.
            (other(0xb, None, Some(0xb1)), Unknown, Unknown),
            (other(0xb, None, None), Unknown, Unknown),
            (other(0xb, Some(0xc0), None), Unknown, Unknown),
        ];
        for (other, linux, macos) in cases {
            assert_eq!(relation_in(&other, &me, false), linux, "{other:?}");
            assert_eq!(relation_in(&other, &me, true), macos, "{other:?}");
            let here = if cfg!(target_os = "macos") {
                macos
            } else {
                linux
            };
            assert_eq!(relation(&other, &me), here, "{other:?}");
        }
        // Missing here.
        let no_machine = Creator::new(1, None, 0xa).with_machine(None, Some(0xb0));
        let no_boot = Creator::new(1, None, 0xa).with_machine(Some(0xc0), None);
        for recorded in [
            other(0xb, Some(0xc0), Some(0xb1)),
            other(0xb, Some(0xc0), Some(0xb0)),
            other(0xb, Some(0xc0), None),
        ] {
            for one_pid_space in [false, true] {
                assert_eq!(relation_in(&recorded, &no_machine, one_pid_space), Unknown);
                assert_eq!(relation_in(&recorded, &no_boot, one_pid_space), Unknown);
            }
        }
    }

    #[test]
    fn the_macos_boot_is_read_from_sysctl_output() {
        let out = "98243C69-5A58-4CB8-B943-8DAB73C37BA6\n\
                   { sec = 1789707228, usec = 927651 } Fri Sep 18 13:53:48 2026\n";
        assert_eq!(
            macos_boot(out),
            Some((
                "98243C69-5A58-4CB8-B943-8DAB73C37BA6".to_owned(),
                SystemTime::UNIX_EPOCH + Duration::new(1_789_707_228, 927_651_000)
            ))
        );
        for bad in [
            "",
            "98243C69-5A58-4CB8-B943-8DAB73C37BA6\n",
            "not-a-uuid\n{ sec = 1, usec = 0 } x\n",
            "98243C69-5A58-4CB8-B943-8DAB73C37BA6\n{ sec = x, usec = 0 } x\n",
            "98243C69-5A58-4CB8-B943-8DAB73C37BA6\n{ sec = 1, usec = 1000000 } x\n",
            "98243C69-5A58-4CB8-B943-8DAB73C37BA6\n{ sec = 1 } x\n",
            "98243C69-5A58-4CB8-B943-8DAB73C37BA6\n{ sec = 1, usec = 0 } x\nmore\n",
        ] {
            assert_eq!(macos_boot(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn another_boot_counts_only_for_what_was_created_before_this_one() {
        let booted = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let before = booted - Duration::from_secs(60);
        let after = booted + Duration::from_secs(1);
        assert!(before_boot(Some(before), Some(booted)));
        assert!(!before_boot(Some(after), Some(booted)));
        assert!(!before_boot(Some(booted), Some(booted)));
        assert!(!before_boot(None, Some(booted)));
        assert!(!before_boot(Some(before), None));
    }

    /// A creator of this machine in another boot is gone only with a
    /// creation time before this boot, which cannot be the case for
    /// anything created now.
    ///
    /// Without a machine (in a container such as the dev container, or not
    /// in the initial PID namespace) this returns early. On Linux it is
    /// checked only where the tests run on the host itself, as on the CI's
    /// `test (linux)` / `sandbox` runners (VMs, initial PID namespace); on
    /// macOS wherever `ioreg` and `sysctl` can be read.
    #[test]
    fn another_boot_of_this_machine_needs_an_earlier_creation() {
        let me = Creator::current().unwrap();
        let (Some(machine), Some(boot)) = (me.machine(), me.boot()) else {
            eprintln!("SKIPPED: no machine recorded here");
            return;
        };
        let earlier = Creator::new(std::process::id(), None, me.host() ^ 1)
            .with_machine(Some(machine), Some(boot ^ 1));
        assert!(!earlier.is_gone(None));
        assert!(!earlier.is_gone(Some(SystemTime::now())));
        assert!(earlier.is_gone(Some(SystemTime::UNIX_EPOCH)));
        // Another machine, even long ago.
        let elsewhere = earlier.with_machine(Some(machine ^ 1), Some(boot ^ 1));
        assert!(!elsewhere.is_gone(Some(SystemTime::UNIX_EPOCH)));
    }

    #[test]
    fn machine_ids_are_validated() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(linux_machine_id(&format!("{id}\n")), Some(id));
        assert_eq!(linux_machine_id(id), Some(id));
        for bad in [
            "",
            "\n",
            "uninitialized\n",
            "00000000000000000000000000000000\n",
            "0123456789ABCDEF0123456789ABCDEF\n",
            "0123456789abcdef0123456789abcde\n",
            "0123456789abcdef0123456789abcdef0\n",
            "0123456789abcdef0123456789abcdeg\n",
        ] {
            assert_eq!(linux_machine_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_platform_uuid_is_read_from_ioreg_output() {
        let out = "+-o J314sAP  <class IOPlatformExpertDevice>\n\
                   {\n\
                   \x20 \"IOPlatformSerialNumber\" = \"XYZ\"\n\
                   \x20 \"IOPlatformUUID\" = \"aa57f66d-9834-5b0f-ab38-67aaa471c7d2\"\n\
                   }\n";
        assert_eq!(
            platform_uuid(out).as_deref(),
            Some("AA57F66D-9834-5B0F-AB38-67AAA471C7D2")
        );
        for bad in [
            "",
            "\"IOPlatformUUID\" = \"\"",
            "\"IOPlatformUUID\" = \"not-a-uuid\"",
            "\"IOPlatformUUID\" = \"00000000-0000-0000-0000-000000000000\"",
            "\"IOPlatformUUID\" = \"AA57F66D98345B0FAB3867AAA471C7D2\"",
            "\"IOPlatformUUID\" = \"AA57F66D-9834-5B0F-AB38-67AAA471C7DZ\"",
        ] {
            assert_eq!(platform_uuid(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_hash_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
